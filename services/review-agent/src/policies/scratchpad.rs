//! Same-line `add_review_comment` loop breaker: three consecutive findings recorded at the same
//! file/line is a stuck model overwriting its own buffered finding, not investigation — so the guard
//! nudges it toward a real tool and suppresses `add_review_comment` for exactly one turn.

use lci_agent_loop::{Nudge, PolicyAction, TurnOutcome, TurnPolicy, TurnState};
use lci_agent_tools::TurnFilter;
use lci_agent_types::{ToolOutcome, ToolSpec};

use super::arg_field;
use crate::tools::ADD_REVIEW_COMMENT;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policies::test_support::{call, state};
    use lci_agent_loop::ChatMessage;

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
