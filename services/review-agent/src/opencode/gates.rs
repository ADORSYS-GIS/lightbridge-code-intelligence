//! Drive the reused review quality gates over OpenCode cycles.
//!
//! [`ReviewGates`] owns the same [`CoverageGate`] + [`RefuteGate`] the native flow composes
//! ([`crate::flows::run_review`]); their internal counters (coverage bounces, the refute one-shot)
//! advance exactly as native because each cycle's reconstructed outcome is fed through the identical
//! `after_turn_actions`. Only the *finish-time* gates live here — the per-turn budget/wind-down
//! policies are OpenCode's own loop concern (see the module doc).

use lci_agent_loop::{
    ChatMessage, LoopStats, Nudge, PolicyAction, TurnOutcome, TurnPolicy, TurnState,
};
use lci_agent_types::ToolSpec;

use crate::policies::{CoverageGate, CoverageState, RefuteGate};

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
/// turns. Owns the same [`CoverageGate`] + [`RefuteGate`] the native flow composes.
pub struct ReviewGates {
    coverage: CoverageGate,
    coverage_state: CoverageState,
    refute: RefuteGate,
    max_turns: usize,
    fast: bool,
    cycle: usize,
}

impl ReviewGates {
    /// Compose the gates for one review, mirroring [`crate::flows::run_review`]'s construction (same
    /// coverage denominator, bounce cap, and fast-tier disabling).
    #[must_use]
    pub fn new(
        diff_files: Vec<String>,
        max_coverage_bounces: usize,
        max_turns: usize,
        fast: bool,
    ) -> Self {
        let (coverage, coverage_state) =
            CoverageGate::new(diff_files, max_coverage_bounces, max_turns, fast);
        Self {
            coverage,
            coverage_state,
            refute: RefuteGate::new(fast),
            max_turns,
            fast,
            cycle: 0,
        }
    }

    /// Observe one completed OpenCode cycle. Always feeds the outcome to both gates (so engagement /
    /// findings accumulate on every cycle, not just finish cycles), then — only when the cycle
    /// requested finish — returns whether a gate bounced it (coverage first, then refute, matching
    /// native registration order) or accepted it with any coverage disclosure to append.
    pub fn observe_cycle(&mut self, outcome: &TurnOutcome) -> GateDecision {
        let turn = self.cycle;
        // One `TurnState` for both after-turn calls (gemini #442): it borrows nothing of `self`, and
        // `after_turn_actions` takes it by shared ref, so the two gates can share it.
        let state = turn_state(turn, self.max_turns);
        let coverage_actions = self.coverage.after_turn_actions(&state, outcome);
        let refute_actions = self.refute.after_turn_actions(&state, outcome);
        self.cycle += 1;

        // Aborts aren't gated (the run ends with the model's reason); a non-finish cycle just
        // accumulated engagement, nothing to decide.
        if outcome.abort_reason.is_some() || !outcome.finish_requested {
            return GateDecision::Proceed;
        }

        let nudges: Vec<String> = coverage_actions
            .iter()
            .chain(refute_actions.iter())
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

    /// Fast tier never bounces and never discloses (parity with `run_review`'s fast path).
    #[test]
    fn fast_tier_accepts_immediately() {
        let mut gates = ReviewGates::new(vec!["a.rs".into()], 3, 40, true);
        assert!(gates.is_fast());
        let finish = cycle_turn_outcome(&[before(
            "lightbridge_finish",
            "c1",
            serde_json::json!({"summary": "quick"}),
        )]);
        assert_eq!(
            gates.observe_cycle(&finish),
            GateDecision::Accept { disclosure: None }
        );
    }
}
