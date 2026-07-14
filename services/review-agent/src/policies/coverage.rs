//! Coverage gate (ADR-0062-adjacent): bounces a premature `finish` until every changed file was either
//! opened or commented on, up to `max_bounces`, then — on a real finish/exhaustion — amends the model's
//! summary with an explicit disclosure of whatever still wasn't covered, so a truncated run never reads
//! as a clean pass.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use lci_agent_loop::{Nudge, PolicyAction, TurnOutcome, TurnPolicy, TurnState, winddown_turn};
use lci_agent_types::ToolOutcome;

use super::{arg_field, normalize_repo_path};
use crate::tools::{ADD_REVIEW_COMMENT, FINISH, READ_FILE};

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
                FINISH if matches!(result.outcome, ToolOutcome::Finish) => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policies::test_support::{call, state};
    use lci_agent_loop::ChatMessage;

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
}
