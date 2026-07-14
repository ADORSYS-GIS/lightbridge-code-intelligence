//! One-shot P0/P1 self-verification: the first time the model tries to `finish` while holding an
//! outstanding P0/P1 finding, the gate bounces it once to re-verify (and retract if it doesn't hold) —
//! a confidently-wrong blocker costs more trust than a missed nit.

use lci_agent_loop::{Nudge, PolicyAction, TurnOutcome, TurnPolicy, TurnState};
use lci_agent_types::ToolOutcome;

use super::arg_field;
use crate::tools::{ADD_REVIEW_COMMENT, RETRACT_FINDING};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policies::test_support::{call, state};
    use lci_agent_loop::ChatMessage;

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
}
