//! Drive the reused review quality gates over OpenCode cycles.
//!
//! [`ReviewGates`] owns the same [`CoverageGate`] + [`RefuteGate`] the native flow composes
//! ([`crate::flows::run_review`]); their internal counters (coverage bounces, the refute one-shot)
//! advance exactly as native because each cycle's reconstructed outcome is fed through the identical
//! `after_turn_actions`. Only the *finish-time* gates live here — the per-turn budget/wind-down
//! policies are OpenCode's own loop concern (see the module doc).

use std::sync::{Arc, Mutex};

use lci_agent_loop::{
    ChatMessage, LoopStats, Nudge, PolicyAction, TurnOutcome, TurnPolicy, TurnState,
};
use lci_agent_types::{ToolOutcome, ToolSpec};

use crate::policies::{
    CoverageGate, CoverageState, RefuteGate, SastAnchorGate, SastLead, SastLeadSink,
};
use crate::tools::RUN_SAST;

/// A minimal `TurnState` for driving the finish-time gates. The reused gates read only `turn`
/// (CoverageGate's wind-down guard) and `max_turns` off it; everything else is empty/default,
/// mirroring the policy unit tests' own scaffold.
fn turn_state(turn: usize, max_turns: usize) -> TurnState<'static> {
    static MESSAGES: [ChatMessage; 0] = [];
    static TOOLS: [ToolSpec; 0] = [];
    static STATS: LoopStats = LoopStats {
        files_read: 0,
        searches: 0,
        batches: 0,
        successful_writes: 0,
        findings_recorded: 0,
    };
    TurnState {
        turn,
        max_turns,
        messages: &MESSAGES,
        base_tools: &TOOLS,
        stats: &STATS,
        converging: false,
    }
}

/// What the supervisor should do after one observed OpenCode cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// The cycle didn't request finish (or aborted): keep driving / finalize as the cycle dictates.
    Proceed,
    /// A quality gate rejected the finish. Re-prompt OpenCode with this text (the combined nudge),
    /// then observe the next cycle — do NOT finalize.
    RejectFinish(String),
    /// Every gate accepted the finish: finalize. `disclosure`, when `Some`, is the coverage note to
    /// append to the posted summary (ADR-0069 / #306).
    Accept { disclosure: Option<String> },
}

/// The reused review quality gates, driven over OpenCode `session/prompt` cycles instead of native
/// turns. Owns the same [`CoverageGate`] + [`RefuteGate`] + [`SastAnchorGate`] the native flow composes.
pub struct ReviewGates {
    coverage: CoverageGate,
    coverage_state: CoverageState,
    refute: RefuteGate,
    /// SAST anchor gate (#305/#406) — bounces a triage verdict recorded on a line opengrep never
    /// flagged. Native feeds it via an in-process [`SastLeadSink`] the `run_sast` tool pushes into; in
    /// the OpenCode path that tool runs in a separate process (`lci-review-mcp`), so we instead recover
    /// its leads from the observed `run_sast` result digest and push them into `sast_leads` ourselves —
    /// the gate then reads the sink and anchors verdicts identically to native.
    sast_anchor: SastAnchorGate,
    /// The shared feed [`Self::observe_cycle`] pushes recovered SAST leads into; the same `Arc` the
    /// `sast_anchor` gate drains.
    sast_leads: SastLeadSink,
    max_turns: usize,
    fast: bool,
    cycle: usize,
}

impl ReviewGates {
    /// Compose the gates for one review — full parity between fast and deep (fast-tier-parity change):
    /// the coverage/refute/SAST-anchor gates no longer take a `fast` flag at all, so both tiers bounce,
    /// refute, and disclose identically; `fast` is kept here only as a pass-through label for logging/
    /// disclosure banners (see [`Self::is_fast`]) — no gate behavior branches on it any more. The
    /// [`SastAnchorGate`] is always composed — it stays inert (no leads) unless the model actually calls
    /// `run_sast`, so no separate "is SAST on" flag is needed here; whether the tool is even offered is
    /// decided upstream where the MCP surface is built.
    #[must_use]
    pub fn new(
        diff_files: Vec<String>,
        max_coverage_bounces: usize,
        max_turns: usize,
        fast: bool,
    ) -> Self {
        let (coverage, coverage_state) =
            CoverageGate::new(diff_files, max_coverage_bounces, max_turns);
        let sast_leads: SastLeadSink = Arc::new(Mutex::new(Vec::new()));
        let sast_anchor = SastAnchorGate::new(Arc::clone(&sast_leads));
        Self {
            coverage,
            coverage_state,
            refute: RefuteGate::new(),
            sast_anchor,
            sast_leads,
            max_turns,
            fast,
            cycle: 0,
        }
    }

    /// Recover the SAST leads a cycle's `run_sast` call(s) produced and push them into `sink`. The tool
    /// ran in the separate `lci-review-mcp` process (ADR-0097), so its coordinates only reach us as the
    /// tool *result* — the [`lci_agent_sast::digest`] text the recorder captured. Parsing it back
    /// (`parse_digest_leads`, pinned as a true inverse of `digest`) reconstructs the exact `SastLead`s
    /// the native tool would have pushed in-process.
    fn ingest_sast_leads(outcome: &TurnOutcome, sink: &SastLeadSink) {
        let mut recovered: Vec<SastLead> = Vec::new();
        for result in &outcome.results {
            if result.call.function.name != RUN_SAST {
                continue;
            }
            let ToolOutcome::Continue(text) = &result.outcome else {
                continue;
            };
            recovered.extend(
                lci_agent_sast::parse_digest_leads(text)
                    .into_iter()
                    .map(|lead| SastLead {
                        file: lead.file,
                        line: lead.line,
                        rule_id: lead.rule_id,
                    }),
            );
        }
        if !recovered.is_empty() {
            sink.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .extend(recovered);
        }
    }

    /// Observe one completed OpenCode cycle. Always feeds the outcome to both gates (so engagement /
    /// findings accumulate on every cycle, not just finish cycles), then — only when the cycle
    /// requested finish — returns whether a gate bounced it (coverage first, then refute, matching
    /// native registration order) or accepted it with any coverage disclosure to append.
    pub fn observe_cycle(&mut self, outcome: &TurnOutcome) -> GateDecision {
        let turn = self.cycle;
        // Recover any SAST leads this cycle's `run_sast` produced, BEFORE the gate runs — the anchor
        // gate drains the sink at the top of its `after_turn_actions`, so a same-cycle
        // `run_sast` → misanchored `add_review_comment` sequence is still caught (native parity).
        Self::ingest_sast_leads(outcome, &self.sast_leads);
        // One `TurnState` for all after-turn calls (gemini #442): it borrows nothing of `self`, and
        // `after_turn_actions` takes it by shared ref, so the gates can share it.
        let state = turn_state(turn, self.max_turns);
        let coverage_actions = self.coverage.after_turn_actions(&state, outcome);
        let refute_actions = self.refute.after_turn_actions(&state, outcome);
        let sast_actions = self.sast_anchor.after_turn_actions(&state, outcome);
        self.cycle += 1;

        // Aborts aren't gated (the run ends with the model's reason); a non-finish cycle just
        // accumulated engagement, nothing to decide.
        if outcome.abort_reason.is_some() || !outcome.finish_requested {
            return GateDecision::Proceed;
        }

        // Native registration order = evaluation order (flows.rs): coverage → refute → SAST anchor.
        let nudges: Vec<String> = coverage_actions
            .iter()
            .chain(refute_actions.iter())
            .chain(sast_actions.iter())
            .filter_map(|action| match action {
                PolicyAction::RejectFinish(Nudge(text)) => Some(text.clone()),
                _ => None,
            })
            .collect();
        if !nudges.is_empty() {
            return GateDecision::RejectFinish(nudges.join("\n\n"));
        }

        // Accepted — flush any coverage disclosure via the same finish path the native loop runs.
        let state = turn_state(turn, self.max_turns);
        let _ = self.coverage.finish_actions(&state, outcome);
        GateDecision::Accept {
            disclosure: self.coverage_state.amended_summary(),
        }
    }

    /// The run ended without an accepted finish (OpenCode stopped, or a budget/turn ceiling tripped).
    /// Runs the coverage exhaustion path so a truncated run still discloses what it never examined
    /// rather than reading as a clean pass. Returns the disclosure note, if any.
    pub fn exhausted(&mut self) -> Option<String> {
        let state = turn_state(self.cycle, self.max_turns);
        let _ = self.coverage.exhausted_actions(&state);
        self.coverage_state.amended_summary()
    }

    /// Whether these gates are running in FAST-tier (no bouncing, no disclosure) — exposed so a host
    /// can assert the tier it configured.
    #[must_use]
    pub fn is_fast(&self) -> bool {
        self.fast
    }
}

#[cfg(test)]
mod tests {
    use super::super::recorder::cycle_turn_outcome;
    use super::super::test_support::{after, before};
    use super::*;

    /// Coverage parity: a diff with one source file, a first cycle that finishes without touching it,
    /// must be bounced; a second cycle that reads it is accepted with no disclosure (fully covered).
    #[test]
    fn coverage_bounces_a_premature_finish_then_accepts() {
        let mut gates = ReviewGates::new(vec!["a.rs".into()], 3, 40, false);

        let premature = cycle_turn_outcome(&[
            before(
                "lightbridge_finish",
                "c1",
                serde_json::json!({"summary": "lgtm"}),
            ),
            after("lightbridge_finish", "c1", "finalize"),
        ]);
        match gates.observe_cycle(&premature) {
            GateDecision::RejectFinish(nudge) => assert!(nudge.contains("a.rs")),
            other => panic!("expected a coverage RejectFinish, got {other:?}"),
        }

        let covered = cycle_turn_outcome(&[
            before(
                "lightbridge_read_file",
                "c2",
                serde_json::json!({"path": "a.rs"}),
            ),
            after("lightbridge_read_file", "c2", "source"),
            before(
                "lightbridge_finish",
                "c3",
                serde_json::json!({"summary": "done"}),
            ),
            after("lightbridge_finish", "c3", "finalize"),
        ]);
        assert_eq!(
            gates.observe_cycle(&covered),
            GateDecision::Accept { disclosure: None }
        );
    }

    /// Refute parity: a P1 finding + finish in one cycle is bounced once; a follow-up cycle that
    /// retracts it and finishes is accepted.
    #[test]
    fn refute_bounces_an_outstanding_p1_once() {
        // No diff files → coverage never bounces, isolating the refute gate.
        let mut gates = ReviewGates::new(vec![], 3, 40, false);

        let record_and_finish = cycle_turn_outcome(&[
            before(
                "lightbridge_add_review_comment",
                "c1",
                serde_json::json!({"file": "a.rs", "line": 2, "priority": "P1", "title": "bug"}),
            ),
            after(
                "lightbridge_add_review_comment",
                "c1",
                "recorded finding at a.rs:2",
            ),
            before(
                "lightbridge_finish",
                "c2",
                serde_json::json!({"summary": "one bug"}),
            ),
            after("lightbridge_finish", "c2", "finalize"),
        ]);
        match gates.observe_cycle(&record_and_finish) {
            GateDecision::RejectFinish(nudge) => {
                assert!(nudge.contains("re-verify") || nudge.contains("DISPROVE"));
            }
            other => panic!("expected a refute RejectFinish, got {other:?}"),
        }

        let retract_and_finish = cycle_turn_outcome(&[
            before(
                "lightbridge_retract_finding",
                "c3",
                serde_json::json!({"file": "a.rs", "line": 2}),
            ),
            after(
                "lightbridge_retract_finding",
                "c3",
                "retracted finding at a.rs:2",
            ),
            before(
                "lightbridge_finish",
                "c4",
                serde_json::json!({"summary": "no findings"}),
            ),
            after("lightbridge_finish", "c4", "finalize"),
        ]);
        assert_eq!(
            gates.observe_cycle(&retract_and_finish),
            GateDecision::Accept { disclosure: None }
        );
    }

    /// The real disclosure path (ADR-0069 / #306): once the coverage bounce budget is spent, the next
    /// finish is ACCEPTED and its summary is amended with a note naming the source file the run never
    /// examined — so a run that gave up on covering a file doesn't read as a clean pass.
    #[test]
    fn accepts_after_bounce_budget_but_discloses_unexamined_source() {
        // One bounce allowed.
        let mut gates = ReviewGates::new(vec!["a.rs".into()], 1, 40, false);

        let first_finish = cycle_turn_outcome(&[
            before(
                "lightbridge_finish",
                "c1",
                serde_json::json!({"summary": "lgtm"}),
            ),
            after("lightbridge_finish", "c1", "finalize"),
        ]);
        assert!(matches!(
            gates.observe_cycle(&first_finish),
            GateDecision::RejectFinish(_)
        ));

        // Second finish, a.rs still unexamined: the bounce budget is spent, so it's accepted — but the
        // summary carries the coverage note.
        let second_finish = cycle_turn_outcome(&[
            before(
                "lightbridge_finish",
                "c2",
                serde_json::json!({"summary": "still lgtm"}),
            ),
            after("lightbridge_finish", "c2", "finalize"),
        ]);
        match gates.observe_cycle(&second_finish) {
            GateDecision::Accept {
                disclosure: Some(note),
            } => {
                assert!(note.contains("examined 0 of 1"), "{note}");
                assert!(note.contains("a.rs"), "{note}");
            }
            other => panic!("expected Accept with a disclosure, got {other:?}"),
        }
    }

    /// Native parity for the no-summary case: a run that stops with no finish at all has no summary to
    /// amend, so exhaustion produces no disclosure (the caller posts its own truncation note).
    #[test]
    fn exhaustion_without_a_finish_produces_no_disclosure() {
        let mut gates = ReviewGates::new(vec!["a.rs".into()], 3, 40, false);
        let idle = cycle_turn_outcome(&[before(
            "lightbridge_report_progress",
            "c1",
            serde_json::json!({"note": "looking"}),
        )]);
        assert_eq!(gates.observe_cycle(&idle), GateDecision::Proceed);
        assert_eq!(gates.exhausted(), None);
    }

    /// SAST anchor parity over observed OpenCode cycles (ADR-0097): a cycle where `run_sast` returns a
    /// digest flagging `.env:216` and the model records a "false positive" verdict at the WRONG line
    /// (`.env:60`) must bounce the follow-up finish, naming the real flagged coordinate — exactly as the
    /// native `SastAnchorGate` does. This proves the digest → `SastLead` reconstruction feeds the reused
    /// gate correctly across the process boundary (the tool ran in `lci-review-mcp`, not in-process).
    #[test]
    fn sast_anchor_bounces_a_misanchored_verdict_recovered_from_the_run_sast_digest() {
        // No diff files → coverage never bounces, isolating the SAST anchor gate.
        let mut gates = ReviewGates::new(vec![], 3, 40, false);

        // A real `lci_agent_sast::digest` for one finding — the exact text `run_sast` returns as its MCP
        // result, which the recorder captures and we parse back into a lead.
        let digest = lci_agent_sast::digest(&[lci_agent_sast::SastFinding {
            file: ".env".into(),
            line: 216,
            rule_id: "generic.secrets.security.detected-generic-api-key".into(),
            message: "Hardcoded API key.".into(),
            priority: "P1".into(),
            help_uri: None,
        }])
        .expect("a non-empty digest");

        // Cycle 1: run_sast flags :216, then a "false positive" verdict is recorded at :60 (never :216).
        let scan_and_misanchor = cycle_turn_outcome(&[
            before("lightbridge_run_sast", "s1", serde_json::json!({})),
            after("lightbridge_run_sast", "s1", &digest),
            before(
                "lightbridge_add_review_comment",
                "c1",
                serde_json::json!({"file": ".env", "line": 60, "title": "False positive", "body": "just a dev password", "priority": "P2", "category": "security"}),
            ),
            after(
                "lightbridge_add_review_comment",
                "c1",
                "recorded finding at .env:60",
            ),
        ]);
        assert_eq!(
            gates.observe_cycle(&scan_and_misanchor),
            GateDecision::Proceed
        );

        // Cycle 2: the model tries to finish — the anchor gate bounces it and names the real line :216.
        let finish = cycle_turn_outcome(&[
            before(
                "lightbridge_finish",
                "f1",
                serde_json::json!({"summary": "lgtm"}),
            ),
            after("lightbridge_finish", "f1", "finalize"),
        ]);
        match gates.observe_cycle(&finish) {
            GateDecision::RejectFinish(nudge) => {
                assert!(
                    nudge.contains(".env:216"),
                    "names the real flagged line: {nudge}"
                );
                assert!(
                    nudge.contains(".env:60"),
                    "names the wrong line used: {nudge}"
                );
            }
            other => panic!("expected a SAST-anchor RejectFinish, got {other:?}"),
        }
    }

    /// A SAST verdict anchored to the REAL flagged line (recovered from the digest) is never bounced —
    /// the gate targets misanchored triage, not every comment near a lead.
    #[test]
    fn sast_anchor_allows_a_verdict_on_the_real_flagged_line() {
        let mut gates = ReviewGates::new(vec![], 3, 40, false);
        let digest = lci_agent_sast::digest(&[lci_agent_sast::SastFinding {
            file: ".env".into(),
            line: 216,
            rule_id: "generic.secrets.security.detected-generic-api-key".into(),
            message: "Hardcoded API key.".into(),
            priority: "P1".into(),
            help_uri: None,
        }])
        .expect("a non-empty digest");
        let scan_and_anchor = cycle_turn_outcome(&[
            before("lightbridge_run_sast", "s1", serde_json::json!({})),
            after("lightbridge_run_sast", "s1", &digest),
            before(
                "lightbridge_add_review_comment",
                "c1",
                serde_json::json!({"file": ".env", "line": 216, "title": "False positive", "body": "read it — placeholder", "priority": "P2", "category": "security"}),
            ),
            after(
                "lightbridge_add_review_comment",
                "c1",
                "recorded finding at .env:216",
            ),
        ]);
        assert_eq!(gates.observe_cycle(&scan_and_anchor), GateDecision::Proceed);
        let finish = cycle_turn_outcome(&[
            before(
                "lightbridge_finish",
                "f1",
                serde_json::json!({"summary": "done"}),
            ),
            after("lightbridge_finish", "f1", "finalize"),
        ]);
        assert_eq!(
            gates.observe_cycle(&finish),
            GateDecision::Accept { disclosure: None }
        );
    }

    /// Fast-tier-parity proof: `is_fast()` is a pure label now — fast bounces a premature finish and
    /// discloses unexamined coverage exactly like deep does in
    /// [`coverage_bounces_a_premature_finish_then_accepts`], the only difference being the tier flag
    /// itself. Replaces the old `fast_tier_accepts_immediately` test, whose assertion (fast skips the
    /// gates entirely) is now false by design.
    #[test]
    fn fast_tier_bounces_and_discloses_identically_to_deep() {
        let mut gates = ReviewGates::new(vec!["a.rs".into()], 3, 40, true);
        assert!(gates.is_fast());

        let premature = cycle_turn_outcome(&[before(
            "lightbridge_finish",
            "c1",
            serde_json::json!({"summary": "quick"}),
        )]);
        match gates.observe_cycle(&premature) {
            GateDecision::RejectFinish(nudge) => assert!(nudge.contains("a.rs")),
            other => panic!("expected fast tier to bounce a premature finish too, got {other:?}"),
        }

        let covered = cycle_turn_outcome(&[
            before(
                "lightbridge_read_file",
                "c2",
                serde_json::json!({"path": "a.rs"}),
            ),
            after("lightbridge_read_file", "c2", "source"),
            before(
                "lightbridge_finish",
                "c3",
                serde_json::json!({"summary": "done"}),
            ),
            after("lightbridge_finish", "c3", "finalize"),
        ]);
        assert_eq!(
            gates.observe_cycle(&covered),
            GateDecision::Accept { disclosure: None }
        );
    }

    /// Fast-tier-parity proof, refute half: a P1 finding bounces the finish on fast exactly as it does
    /// on deep in [`refute_bounces_an_outstanding_p1_once`].
    #[test]
    fn fast_tier_refutes_identically_to_deep() {
        let mut gates = ReviewGates::new(vec![], 3, 40, true);
        assert!(gates.is_fast());

        let record_and_finish = cycle_turn_outcome(&[
            before(
                "lightbridge_add_review_comment",
                "c1",
                serde_json::json!({"file": "a.rs", "line": 2, "priority": "P1", "title": "bug"}),
            ),
            after(
                "lightbridge_add_review_comment",
                "c1",
                "recorded finding at a.rs:2",
            ),
            before(
                "lightbridge_finish",
                "c2",
                serde_json::json!({"summary": "one bug"}),
            ),
            after("lightbridge_finish", "c2", "finalize"),
        ]);
        match gates.observe_cycle(&record_and_finish) {
            GateDecision::RejectFinish(nudge) => {
                assert!(nudge.contains("re-verify") || nudge.contains("DISPROVE"));
            }
            other => panic!("expected fast tier to refute-bounce too, got {other:?}"),
        }
    }
}
