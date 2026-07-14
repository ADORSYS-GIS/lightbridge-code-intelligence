//! One-shot P0/P1 self-verification (ADR-0043, refined by ADR-0091): the first time the model tries to
//! `finish` while holding an outstanding P0/P1 finding, the gate bounces it once to re-verify (and
//! retract if it doesn't hold) — a confidently-wrong blocker costs more trust than a missed nit.
//!
//! ADR-0091: re-reading the finding's own cited evidence structurally can only ever confirm it, never
//! refute it (`ADORSYS-GIS/webank-mobile#145`, run `e82f7c4b-50ec-4bc4-942f-48cfc404b603` — a false-
//! positive P1 survived because the refute pass re-read the two files the finding already cited). For a
//! finding that claims something is *absent* ("X is never sent/set/present"), the gate now directs the
//! model outward — at the transport/interceptor/middleware/config-default layer, in files it has not yet
//! engaged — since that is structurally where this bug class's disconfirming evidence lives.

use std::collections::{BTreeMap, BTreeSet};

use lci_agent_loop::{Nudge, PolicyAction, TurnOutcome, TurnPolicy, TurnState};
use lci_agent_types::ToolOutcome;

use super::{arg_field, arg_int_field, normalize_repo_path};
use crate::tools::{ADD_REVIEW_COMMENT, READ_FILE, RETRACT_FINDING};

/// Phrases marking a finding as an *absence* claim. Deliberately broad, bare (article-free) substring
/// matching over the finding's own text (title/body/evidence) — a false negative here just falls back to
/// the pre-existing generic re-verify nudge, so over-matching is far cheaper than under-matching. Bare
/// forms ("without", "missing", "lacks", "omits") are intentional, not "without the"/"without a": the
/// motivating incident's own finding read "Cancel request rejected without Idempotency-Key" — no
/// article — and a qualifier-anchored marker would have missed the exact case this exists to catch.
const ABSENCE_MARKERS: &[&str] = &[
    "never sent",
    "never set",
    "never included",
    "never present",
    "never populated",
    "never applied",
    "never has",
    "not sent",
    "not set",
    "not present",
    "not included",
    "not populated",
    "not applied",
    "not have",
    "no longer sent",
    "no longer set",
    "no longer included",
    "without",
    "missing",
    "lacks",
    "omits",
    "omitted",
    "sends no",
    "sent no",
    "send no",
    "includes no",
    "contains no",
    "doesn't send",
    "does not send",
    "doesn't set",
    "does not set",
    "doesn't include",
    "does not include",
    "doesn't have",
    "does not have",
    "fails to send",
    "fails to set",
    "fails to include",
];

fn is_absence_claim(text: &str) -> bool {
    let lower = text.to_lowercase();
    ABSENCE_MARKERS.iter().any(|marker| lower.contains(marker))
}

/// Cap on the already-engaged files named in the directive — a long deep-tier run can have engaged
/// dozens of files, and listing all of them would bloat the nudge for no benefit. Mirrors
/// `CoverageGate`'s `COVERAGE_MAX_LISTED`.
const ENGAGED_MAX_LISTED: usize = 15;

#[derive(Clone)]
struct Finding {
    file: String,
    line: i64,
    title: String,
    absence_claim: bool,
}

/// Directive appended to the base re-verify nudge when at least one outstanding finding is an absence
/// claim. Names the finding(s), names the files already engaged (so the model doesn't waste the bounce
/// re-reading them), and points at the specific layer this bug class hides in.
fn absence_directive(findings: &[&Finding], engaged: &BTreeSet<String>) -> String {
    let cited = findings
        .iter()
        .map(|finding| {
            format!(
                "- \"{}\" at {}:{}",
                finding.title, finding.file, finding.line
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let already_read = if engaged.is_empty() {
        "(no files read yet)".to_string()
    } else {
        let listed: Vec<&str> = engaged
            .iter()
            .map(String::as_str)
            .take(ENGAGED_MAX_LISTED)
            .collect();
        let more = engaged.len() - listed.len();
        let mut rendered = listed.join(", ");
        if more > 0 {
            rendered.push_str(&format!(", … and {more} more"));
        }
        rendered
    };
    format!(
        "\n\nThe following finding(s) claim something is never sent/set/present:\n{cited}\n\nThis bug class is almost always disproven by a file you have NOT opened — a transport interceptor, \
middleware, base-options builder, or config default that injects the value upstream of the call site \
you cited — not by the call site itself. Re-reading what you've already engaged ({already_read}) will \
only re-confirm what you already believe; it proves nothing. Before keeping any of these, search for \
the identifier across the client/transport layer (read_file, lightbridge_graph_find_symbol, \
lightbridge_graph_get_callers, lightbridge_vector_semantic_search) in files outside that set. If a real \
search turns up no disconfirming file, keep the finding; if it does, retract it."
    )
}

pub struct RefuteGate {
    findings: BTreeMap<(String, i64), Finding>,
    engaged: BTreeSet<String>,
    bounced: bool,
    fast: bool,
}

impl RefuteGate {
    #[must_use]
    pub fn new(fast: bool) -> Self {
        Self {
            findings: BTreeMap::new(),
            engaged: BTreeSet::new(),
            bounced: false,
            fast,
        }
    }
}

impl TurnPolicy for RefuteGate {
    fn name(&self) -> &'static str {
        "refute"
    }

    fn before_turn(&mut self, _state: &TurnState<'_>) -> Vec<PolicyAction> {
        Vec::new()
    }

    fn after_turn_actions(
        &mut self,
        _state: &TurnState<'_>,
        outcome: &TurnOutcome,
    ) -> Vec<PolicyAction> {
        for result in &outcome.results {
            let arguments = &result.call.function.arguments;
            match result.call.function.name.as_str() {
                READ_FILE => {
                    if let Some(path) = arg_field(arguments, "path") {
                        self.engaged.insert(normalize_repo_path(&path));
                    }
                }
                ADD_REVIEW_COMMENT if matches!(&result.outcome, ToolOutcome::Continue(message) if message.starts_with("recorded finding")) =>
                {
                    let file = arg_field(arguments, "file")
                        .map(|path| normalize_repo_path(&path))
                        .unwrap_or_default();
                    self.engaged.insert(file.clone());
                    if matches!(
                        arg_field(arguments, "priority").as_deref(),
                        Some("P0") | Some("P1")
                    ) {
                        let line = arg_int_field(arguments, "line").unwrap_or(0);
                        let title = arg_field(arguments, "title").unwrap_or_default();
                        let body = arg_field(arguments, "body").unwrap_or_default();
                        let evidence = arg_field(arguments, "evidence").unwrap_or_default();
                        let absence_claim = is_absence_claim(&title)
                            || is_absence_claim(&body)
                            || is_absence_claim(&evidence);
                        self.findings.insert(
                            (file.clone(), line),
                            Finding {
                                file,
                                line,
                                title,
                                absence_claim,
                            },
                        );
                    }
                }
                RETRACT_FINDING if matches!(&result.outcome, ToolOutcome::Continue(message) if message.starts_with("retracted finding")) => {
                    if let (Some(file), Some(line)) = (
                        arg_field(arguments, "file").map(|path| normalize_repo_path(&path)),
                        arg_int_field(arguments, "line"),
                    ) {
                        self.findings.remove(&(file, line));
                    }
                }
                _ => {}
            }
        }
        if self.fast || self.bounced || self.findings.is_empty() || !outcome.finish_requested {
            return Vec::new();
        }
        self.bounced = true;
        let absence_findings: Vec<&Finding> =
            self.findings.values().filter(|f| f.absence_claim).collect();
        let mut message = "Before you finish: you recorded P0/P1 finding(s). Re-verify each one — but \
don't just re-read the evidence you already cited, since that only ever confirms itself; actively look \
for evidence that would DISPROVE the claim, not just evidence that supports it. For any whose claim does \
NOT hold, call `retract_finding(file, line)`. A confidently-wrong blocker costs more trust than a missed \
nit."
            .to_string();
        if !absence_findings.is_empty() {
            message.push_str(&absence_directive(&absence_findings, &self.engaged));
        }
        message.push_str(" Keep only what you can prove, then call `finish`.");
        vec![
            PolicyAction::Record {
                name: None,
                detail: serde_json::json!({
                    "p0p1": self.findings.len(),
                    "absence_claims": absence_findings.len(),
                }),
            },
            PolicyAction::RejectFinish(Nudge(message)),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policies::test_support::{call, state};
    use lci_agent_loop::ChatMessage;

    fn finding_turn(arguments: &str) -> TurnOutcome {
        TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![call(
                ADD_REVIEW_COMMENT,
                arguments,
                ToolOutcome::Continue("recorded finding at a.rs:2".into()),
            )],
            finish_requested: false,
            abort_reason: None,
        }
    }

    fn finish_turn() -> TurnOutcome {
        TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![],
            finish_requested: true,
            abort_reason: None,
        }
    }

    #[test]
    fn refute_gate_is_one_shot_and_retracts_monotonically() {
        let mut gate = RefuteGate::new(false);
        gate.after_turn_actions(
            &state(0),
            &finding_turn(r#"{"priority":"P1","file":"a.rs","line":2}"#),
        );
        assert!(
            gate.after_turn_actions(&state(1), &finish_turn())
                .iter()
                .any(|action| matches!(action, PolicyAction::RejectFinish(_)))
        );
        assert!(
            gate.after_turn_actions(&state(2), &finish_turn())
                .is_empty()
        );
    }

    #[test]
    fn retracting_a_finding_by_file_and_line_clears_the_gate() {
        let mut gate = RefuteGate::new(false);
        gate.after_turn_actions(
            &state(0),
            &finding_turn(r#"{"priority":"P1","file":"a.rs","line":2}"#),
        );
        let retract = TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![call(
                RETRACT_FINDING,
                r#"{"file":"a.rs","line":2}"#,
                ToolOutcome::Continue("retracted finding at a.rs:2".into()),
            )],
            finish_requested: false,
            abort_reason: None,
        };
        gate.after_turn_actions(&state(1), &retract);
        assert!(
            gate.after_turn_actions(&state(2), &finish_turn())
                .is_empty()
        );
    }

    /// Regression for #304 / ADR-0091 — an absence claim ("Idempotency-Key ... never sent") anchored at
    /// the call site the model already read must get the outward-search directive, naming the already-
    /// engaged file so the model doesn't waste the bounce re-reading it.
    #[test]
    fn absence_claim_gets_the_outward_search_directive() {
        let mut gate = RefuteGate::new(false);
        gate.after_turn_actions(
            &state(0),
            &finding_turn(
                r#"{"priority":"P1","file":"bff/cmd/server/main.go","line":580,"title":"Cancel rejected without Idempotency-Key","body":"pending_p2p_repository.dart sends no header","evidence":"call site never sets the Idempotency-Key header"}"#,
            ),
        );
        let actions = gate.after_turn_actions(&state(1), &finish_turn());
        let nudge = actions
            .iter()
            .find_map(|action| match action {
                PolicyAction::RejectFinish(Nudge(text)) => Some(text.clone()),
                _ => None,
            })
            .expect("expected a RejectFinish nudge");
        assert!(nudge.contains("bff/cmd/server/main.go:580"));
        assert!(nudge.contains("transport interceptor"));
        assert!(nudge.contains("bff/cmd/server/main.go"));
    }

    /// Regression for a codex finding on #403: the marker list must catch the *bare* (article-free)
    /// phrasing the motivating incident actually used — "without Idempotency-Key", not "without the/a
    /// Idempotency-Key" — with no other matching language anywhere else in the finding to lean on.
    #[test]
    fn bare_without_phrasing_alone_is_detected_as_absence_claim() {
        let mut gate = RefuteGate::new(false);
        gate.after_turn_actions(
            &state(0),
            &finding_turn(
                r#"{"priority":"P1","file":"bff/cmd/server/main.go","line":580,"title":"Cancel request rejected without Idempotency-Key","body":"every cancel attempt will be rejected with 400","evidence":"pending_p2p_repository.dart:30"}"#,
            ),
        );
        let actions = gate.after_turn_actions(&state(1), &finish_turn());
        let nudge = actions
            .iter()
            .find_map(|action| match action {
                PolicyAction::RejectFinish(Nudge(text)) => Some(text.clone()),
                _ => None,
            })
            .expect("expected a RejectFinish nudge");
        assert!(nudge.contains("transport interceptor"));
    }

    /// The engaged-files list in the outward-search directive is capped (mirrors `CoverageGate`'s
    /// `COVERAGE_MAX_LISTED`) so a long deep-tier run doesn't bloat the nudge with dozens of paths.
    #[test]
    fn absence_directive_caps_the_engaged_files_list() {
        let mut gate = RefuteGate::new(false);
        for i in 0..20 {
            gate.after_turn_actions(
                &state(0),
                &TurnOutcome {
                    assistant: ChatMessage::user(""),
                    results: vec![call(
                        READ_FILE,
                        &format!(r#"{{"path":"file{i}.rs"}}"#),
                        ToolOutcome::Continue("source".into()),
                    )],
                    finish_requested: false,
                    abort_reason: None,
                },
            );
        }
        gate.after_turn_actions(
            &state(1),
            &finding_turn(
                r#"{"priority":"P1","file":"a.rs","line":2,"title":"header never sent","body":"","evidence":""}"#,
            ),
        );
        let actions = gate.after_turn_actions(&state(2), &finish_turn());
        let nudge = actions
            .iter()
            .find_map(|action| match action {
                PolicyAction::RejectFinish(Nudge(text)) => Some(text.clone()),
                _ => None,
            })
            .expect("expected a RejectFinish nudge");
        assert!(nudge.contains("… and"));
        assert!(nudge.contains("more"));
    }

    /// A finding with no absence-marker language gets only the generic re-verify nudge — the outward-
    /// search directive is scoped to absence claims (ticket's "Out of Scope").
    #[test]
    fn non_absence_claim_skips_the_outward_search_directive() {
        let mut gate = RefuteGate::new(false);
        gate.after_turn_actions(
            &state(0),
            &finding_turn(
                r#"{"priority":"P1","file":"a.rs","line":2,"title":"SQL injection","body":"string-concatenates user input into a query","evidence":"format!(\"SELECT * FROM t WHERE id = {id}\")"}"#,
            ),
        );
        let actions = gate.after_turn_actions(&state(1), &finish_turn());
        let nudge = actions
            .iter()
            .find_map(|action| match action {
                PolicyAction::RejectFinish(Nudge(text)) => Some(text.clone()),
                _ => None,
            })
            .expect("expected a RejectFinish nudge");
        assert!(!nudge.contains("transport interceptor"));
    }

    #[test]
    fn fast_tier_never_bounces() {
        let mut gate = RefuteGate::new(true);
        gate.after_turn_actions(
            &state(0),
            &finding_turn(r#"{"priority":"P1","file":"a.rs","line":2}"#),
        );
        assert!(
            gate.after_turn_actions(&state(1), &finish_turn())
                .is_empty()
        );
    }
}
