//! Review-flavoured policies kept above the generic runtime loop.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use lci_agent_loop::{
    Nudge, PolicyAction, TurnOutcome, TurnPolicy, TurnState, convergence_filter, winddown_turn,
};
use lci_agent_tools::{DispatchRefusal, TurnFilter};
use lci_agent_types::{ToolOutcome, ToolSpec};

use crate::tools::{ADD_REVIEW_COMMENT, READ_FILE, RETRACT_FINDING, fast_refusal};

fn arg_field(arguments: &str, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()?
        .get(key)?
        .as_str()
        .map(str::to_string)
}

fn normalize_repo_path(path: &str) -> String {
    path.replace('\\', "/")
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

/// Exact assembly-owned rendering for strict fast-tier dispatch.
#[must_use]
pub fn render_fast_refusal(refusal: DispatchRefusal) -> ToolOutcome {
    match refusal {
        DispatchRefusal::NotOffered { tool_name } => {
            ToolOutcome::Continue(fast_refusal(&tool_name))
        }
        DispatchRefusal::MissingCallId { tool_name } => ToolOutcome::Continue(format!(
            "error: tool {tool_name:?} requires a non-empty call id for deduplication. Re-call the tool."
        )),
    }
}

pub struct FastTierGuard {
    enabled: bool,
}

impl FastTierGuard {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }
}

impl TurnPolicy for FastTierGuard {
    fn name(&self) -> &'static str {
        "fast_tier_guard"
    }

    fn before_turn(&mut self, _state: &TurnState<'_>) -> Vec<PolicyAction> {
        if self.enabled {
            vec![
                PolicyAction::Narrow(convergence_filter()),
                PolicyAction::GuardOffered,
            ]
        } else {
            Vec::new()
        }
    }

    fn after_turn_actions(
        &mut self,
        _state: &TurnState<'_>,
        outcome: &TurnOutcome,
    ) -> Vec<PolicyAction> {
        if !self.enabled {
            return Vec::new();
        }
        outcome
            .results
            .iter()
            .filter(|result| {
                matches!(&result.outcome, ToolOutcome::Continue(message) if *message == fast_refusal(&result.call.function.name))
            })
            .map(|result| PolicyAction::Record {
                name: Some("fast_refusal"),
                detail: serde_json::json!({"tool": result.call.function.name}),
            })
            .collect()
    }
}

const COVERAGE_MAX_LISTED: usize = 15;

fn coverage_file_list(uncovered: &[&str]) -> String {
    let listed = uncovered
        .iter()
        .take(COVERAGE_MAX_LISTED)
        .map(|file| format!("- {file}"))
        .collect::<Vec<_>>()
        .join("\n");
    let more = if uncovered.len() > COVERAGE_MAX_LISTED {
        format!("\n- … and {} more", uncovered.len() - COVERAGE_MAX_LISTED)
    } else {
        String::new()
    };
    format!("{listed}{more}")
}

fn coverage_nudge(uncovered: &[&str], stalled: bool) -> String {
    let listed = coverage_file_list(uncovered);
    if stalled {
        format!(
            "You called `finish` again without opening ANY of the files you were just asked to review. A summary claiming these files were reviewed would be false — do not write one. These changed files are still unexamined:\n{listed}\n\nOpen them with read_file (or record a finding on them with add_review_comment) before you finish. If a file is genuinely not reviewable in depth (a lockfile, a generated artifact), leave it unopened but name it as NOT reviewed in your final summary. Only claim work you actually did."
        )
    } else {
        format!(
            "Before you finish: these changed files don't yet have a finding and you haven't opened them:\n{listed}\n\nReview each one across all relevant dimensions — correctness, security, quality, style, performance — not only the first issue you found. Open each with read_file, record anything worth raising with add_review_comment, then call `finish`. If a file is genuinely not reviewable in depth (a lockfile, a generated artifact), you may leave it unopened — but then name it as NOT reviewed in your final summary; never claim coverage you did not do."
        )
    }
}

fn coverage_disclosure(engaged: usize, changed: usize, uncovered: &[&str]) -> String {
    format!(
        "> ⚠️ **Coverage note (automated):** this run examined {engaged} of {changed} changed files. Never opened or commented on:\n{}",
        coverage_file_list(uncovered)
            .lines()
            .map(|line| format!("> {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

#[derive(Clone, Default)]
pub struct CoverageState(Arc<Mutex<Option<String>>>);

impl CoverageState {
    #[must_use]
    pub fn amended_summary(&self) -> Option<String> {
        self.0.lock().expect("coverage state mutex").clone()
    }
}

pub struct CoverageGate {
    changed: BTreeSet<String>,
    engaged: BTreeSet<String>,
    max_bounces: usize,
    bounces: usize,
    engaged_at_last_bounce: usize,
    winddown: usize,
    fast: bool,
    summary: Option<String>,
    state: CoverageState,
}

impl CoverageGate {
    #[must_use]
    pub fn new(
        changed_files: impl IntoIterator<Item = impl Into<String>>,
        max_bounces: usize,
        max_turns: usize,
        fast: bool,
    ) -> (Self, CoverageState) {
        let state = CoverageState::default();
        (
            Self {
                changed: changed_files
                    .into_iter()
                    .map(Into::into)
                    .map(|file| normalize_repo_path(&file))
                    .collect(),
                engaged: BTreeSet::new(),
                max_bounces,
                bounces: 0,
                engaged_at_last_bounce: 0,
                winddown: winddown_turn(max_turns),
                fast,
                summary: None,
                state: state.clone(),
            },
            state,
        )
    }

    fn update(&mut self, outcome: &TurnOutcome) {
        for result in &outcome.results {
            match result.call.function.name.as_str() {
                READ_FILE => {
                    if let Some(path) = arg_field(&result.call.function.arguments, "path") {
                        self.engaged.insert(normalize_repo_path(&path));
                    }
                }
                ADD_REVIEW_COMMENT => {
                    if let Some(path) = arg_field(&result.call.function.arguments, "file") {
                        self.engaged.insert(normalize_repo_path(&path));
                    }
                }
                "finish" if matches!(result.outcome, ToolOutcome::Finish) => {
                    self.summary = arg_field(&result.call.function.arguments, "summary");
                }
                _ => {}
            }
        }
    }

    fn uncovered(&self) -> Vec<&str> {
        self.changed
            .difference(&self.engaged)
            .map(String::as_str)
            .collect()
    }

    fn disclose(&self) -> bool {
        let Some(summary) = self.summary.as_deref() else {
            return false;
        };
        let uncovered = self.uncovered();
        if uncovered.is_empty() {
            return false;
        }
        let amended = format!(
            "{summary}\n\n{}",
            coverage_disclosure(
                self.changed.len() - uncovered.len(),
                self.changed.len(),
                &uncovered,
            )
        );
        *self.state.0.lock().expect("coverage state mutex") = Some(amended);
        true
    }
}

impl TurnPolicy for CoverageGate {
    fn name(&self) -> &'static str {
        "coverage_gate"
    }

    fn before_turn(&mut self, _state: &TurnState<'_>) -> Vec<PolicyAction> {
        Vec::new()
    }

    fn after_turn_actions(
        &mut self,
        state: &TurnState<'_>,
        outcome: &TurnOutcome,
    ) -> Vec<PolicyAction> {
        self.update(outcome);
        if self.fast
            || !outcome.finish_requested
            || state.turn >= self.winddown
            || self.bounces >= self.max_bounces
        {
            return Vec::new();
        }
        let uncovered: Vec<String> = self.uncovered().into_iter().map(str::to_string).collect();
        if uncovered.is_empty() {
            return Vec::new();
        }
        let stalled = self.bounces > 0 && self.engaged.len() == self.engaged_at_last_bounce;
        self.bounces += 1;
        self.engaged_at_last_bounce = self.engaged.len();
        let uncovered_refs: Vec<&str> = uncovered.iter().map(String::as_str).collect();
        vec![
            PolicyAction::Record {
                name: Some("coverage_bounce"),
                detail: serde_json::json!({
                    "bounce": self.bounces,
                    "uncovered": uncovered,
                    "stalled": stalled,
                }),
            },
            PolicyAction::RejectFinish(Nudge(coverage_nudge(&uncovered_refs, stalled))),
        ]
    }

    fn finish_actions(
        &mut self,
        _state: &TurnState<'_>,
        _outcome: &TurnOutcome,
    ) -> Vec<PolicyAction> {
        if !self.fast && self.disclose() {
            vec![PolicyAction::Record {
                name: Some("coverage_disclosure"),
                detail: serde_json::json!({}),
            }]
        } else {
            Vec::new()
        }
    }

    fn exhausted_actions(&mut self, _state: &TurnState<'_>) -> Vec<PolicyAction> {
        if !self.fast && self.disclose() {
            vec![PolicyAction::Record {
                name: Some("coverage_disclosure"),
                detail: serde_json::json!({}),
            }]
        } else {
            Vec::new()
        }
    }
}

pub struct ScratchpadLoopGuard {
    last_location: Option<(String, i32)>,
    repeats: usize,
    suppress_next: bool,
}

impl ScratchpadLoopGuard {
    #[must_use]
    pub fn new() -> Self {
        Self {
            last_location: None,
            repeats: 0,
            suppress_next: false,
        }
    }
}

impl Default for ScratchpadLoopGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl TurnPolicy for ScratchpadLoopGuard {
    fn name(&self) -> &'static str {
        "scratchpad_guard"
    }

    fn before_turn(&mut self, state: &TurnState<'_>) -> Vec<PolicyAction> {
        if !self.suppress_next {
            return Vec::new();
        }
        self.suppress_next = false;
        vec![PolicyAction::Narrow(TurnFilter::only_names(
            state
                .base_tools
                .iter()
                .map(ToolSpec::name)
                .filter(|name| *name != ADD_REVIEW_COMMENT),
        ))]
    }

    fn after_turn_actions(
        &mut self,
        _state: &TurnState<'_>,
        outcome: &TurnOutcome,
    ) -> Vec<PolicyAction> {
        for result in &outcome.results {
            let ToolOutcome::Continue(message) = &result.outcome else {
                continue;
            };
            if result.call.function.name != ADD_REVIEW_COMMENT
                || !message.starts_with("recorded finding")
            {
                continue;
            }
            let location = arg_field(&result.call.function.arguments, "file").map(|file| {
                let line =
                    serde_json::from_str::<serde_json::Value>(&result.call.function.arguments)
                        .ok()
                        .and_then(|value| value.get("line").and_then(serde_json::Value::as_i64))
                        .unwrap_or(0) as i32;
                (file, line)
            });
            if location.is_some() && location == self.last_location {
                self.repeats += 1;
            } else {
                self.repeats = 0;
                self.last_location = location;
            }
        }
        if self.repeats < 2 {
            return Vec::new();
        }
        self.repeats = 0;
        self.suppress_next = true;
        vec![
            PolicyAction::Record {
                name: None,
                detail: serde_json::json!({}),
            },
            PolicyAction::Inject(Nudge("You've recorded on the same line several times — that's a loop, and the buffer keeps only the last one. `add_review_comment` is for a FINAL finding you can prove, not for notes. Investigate with `read_file` (or `report_progress` to jot a note), then record the finding once — or call `finish`. (add_review_comment is unavailable next turn.)".into())),
        ]
    }
}

pub struct RefuteGate {
    p0p1: usize,
    bounced: bool,
    fast: bool,
}

impl RefuteGate {
    #[must_use]
    pub fn new(fast: bool) -> Self {
        Self {
            p0p1: 0,
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
            if result.call.function.name == ADD_REVIEW_COMMENT
                && matches!(&result.outcome, ToolOutcome::Continue(message) if message.starts_with("recorded finding"))
                && matches!(
                    arg_field(&result.call.function.arguments, "priority").as_deref(),
                    Some("P0") | Some("P1")
                )
            {
                self.p0p1 += 1;
            }
            if result.call.function.name == RETRACT_FINDING
                && matches!(&result.outcome, ToolOutcome::Continue(message) if message.starts_with("retracted finding"))
            {
                self.p0p1 = self.p0p1.saturating_sub(1);
            }
        }
        if self.fast || self.bounced || self.p0p1 == 0 || !outcome.finish_requested {
            return Vec::new();
        }
        self.bounced = true;
        vec![
            PolicyAction::Record {
                name: None,
                detail: serde_json::json!({"p0p1": self.p0p1}),
            },
            PolicyAction::RejectFinish(Nudge("Before you finish: you recorded P0/P1 finding(s). Re-verify each one against the exact evidence you cited — look at the real code, not your memory. For any whose claim does NOT hold (the cited lines don't actually show the bug), call `retract_finding(file, line)`. A confidently-wrong blocker costs more trust than a missed nit. Keep only what you can prove, then call `finish`.".into())),
        ]
    }
}

pub struct FindingFinishNudge {
    findings: usize,
    nudged: bool,
    fast: bool,
}

impl FindingFinishNudge {
    #[must_use]
    pub fn new(fast: bool) -> Self {
        Self {
            findings: 0,
            nudged: false,
            fast,
        }
    }
}

impl TurnPolicy for FindingFinishNudge {
    fn name(&self) -> &'static str {
        "finding_finish_nudge"
    }

    fn before_turn(&mut self, _state: &TurnState<'_>) -> Vec<PolicyAction> {
        Vec::new()
    }

    fn after_turn_actions(
        &mut self,
        _state: &TurnState<'_>,
        outcome: &TurnOutcome,
    ) -> Vec<PolicyAction> {
        self.findings += outcome
            .results
            .iter()
            .filter(|result| {
                result.call.function.name == ADD_REVIEW_COMMENT
                    && matches!(&result.outcome, ToolOutcome::Continue(message) if message.starts_with("recorded finding"))
            })
            .count();
        let mut actions = vec![PolicyAction::SetFindings(self.findings)];
        if self.nudged || self.findings == 0 || outcome.finish_requested {
            return actions;
        }
        self.nudged = true;
        let nudge = if self.fast {
            "You've recorded a finding. Record any others on changed lines with add_review_comment, then call `finish` with your overall verdict to post the review."
        } else {
            "You have recorded at least one finding. When your investigation is complete, call `finish` with your overall verdict to post everything you've buffered — don't keep investigating past the point of useful work."
        };
        actions.extend([
            PolicyAction::Record {
                name: None,
                detail: serde_json::json!({"findings": self.findings}),
            },
            PolicyAction::Inject(Nudge(nudge.into())),
        ]);
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lci_agent_loop::{ChatMessage, LoopStats, ToolCallResult};
    use lci_agent_types::{FunctionCallReq, ToolCallReq};

    fn call(name: &str, arguments: &str, outcome: ToolOutcome) -> ToolCallResult {
        ToolCallResult {
            call: ToolCallReq {
                id: "id".into(),
                kind: "function".into(),
                function: FunctionCallReq {
                    name: name.into(),
                    arguments: arguments.into(),
                },
                extra_content: None,
            },
            kind: None,
            outcome,
        }
    }

    fn state(turn: usize) -> TurnState<'static> {
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
            max_turns: 5,
            messages: &MESSAGES,
            base_tools: &TOOLS,
            stats: &STATS,
            converging: false,
        }
    }

    #[test]
    fn coverage_requires_real_engagement_before_accepting_finish() {
        let (mut gate, coverage) = CoverageGate::new(["./a.rs"], 1, 5, false);
        let early = TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![call(
                "finish",
                r#"{"summary":"early"}"#,
                ToolOutcome::Finish,
            )],
            finish_requested: true,
            abort_reason: None,
        };
        assert!(
            gate.after_turn_actions(&state(0), &early)
                .iter()
                .any(|action| matches!(action, PolicyAction::RejectFinish(_)))
        );
        let read = TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![call(
                READ_FILE,
                r#"{"path":"a.rs"}"#,
                ToolOutcome::Continue("source".into()),
            )],
            finish_requested: false,
            abort_reason: None,
        };
        gate.after_turn_actions(&state(1), &read);
        assert!(gate.after_turn_actions(&state(2), &early).is_empty());
        assert!(gate.finish_actions(&state(2), &early).is_empty());
        assert!(coverage.amended_summary().is_none());
    }

    #[test]
    fn refute_gate_is_one_shot_and_retracts_monotonically() {
        let mut gate = RefuteGate::new(false);
        let finding = TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![call(
                ADD_REVIEW_COMMENT,
                r#"{"priority":"P1"}"#,
                ToolOutcome::Continue("recorded finding at a.rs:2".into()),
            )],
            finish_requested: false,
            abort_reason: None,
        };
        gate.after_turn_actions(&state(0), &finding);
        let finish = TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![],
            finish_requested: true,
            abort_reason: None,
        };
        assert!(
            gate.after_turn_actions(&state(1), &finish)
                .iter()
                .any(|action| matches!(action, PolicyAction::RejectFinish(_)))
        );
        assert!(gate.after_turn_actions(&state(2), &finish).is_empty());
    }

    #[test]
    fn scratchpad_guard_fires_after_three_same_location_records() {
        let mut guard = ScratchpadLoopGuard::new();
        let finding = TurnOutcome {
            assistant: ChatMessage::user(""),
            results: vec![call(
                ADD_REVIEW_COMMENT,
                r#"{"file":"a.rs","line":2}"#,
                ToolOutcome::Continue("recorded finding at a.rs:2".into()),
            )],
            finish_requested: false,
            abort_reason: None,
        };
        assert!(guard.after_turn_actions(&state(0), &finding).is_empty());
        assert!(guard.after_turn_actions(&state(1), &finding).is_empty());
        assert!(
            guard
                .after_turn_actions(&state(2), &finding)
                .iter()
                .any(|action| matches!(action, PolicyAction::Inject(_)))
        );
    }
}
