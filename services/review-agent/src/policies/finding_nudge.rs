//! Nudges the model toward `finish` once it has recorded at least one finding, so it doesn't keep
//! investigating past the point of useful work. Fires at most once per run and always reports the
//! running finding count via [`PolicyAction::SetFindings`].

use lci_agent_loop::{Nudge, PolicyAction, TurnOutcome, TurnPolicy, TurnState};
use lci_agent_types::ToolOutcome;

use crate::tools::ADD_REVIEW_COMMENT;

pub struct FindingFinishNudge {
    findings: usize,
    nudged: bool,
}

impl FindingFinishNudge {
    #[must_use]
    pub fn new() -> Self {
        Self {
            findings: 0,
            nudged: false,
        }
    }
}

impl Default for FindingFinishNudge {
    fn default() -> Self {
        Self::new()
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
        // ADR-0103: one nudge text for every preset — the numeric turn/read budgets a preset
        // configures are what actually shorten a tight-budget run, not a different prompt.
        let nudge = "You have recorded at least one finding. When your investigation is complete, call `finish` with your overall verdict to post everything you've buffered — don't keep investigating past the point of useful work.";
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
