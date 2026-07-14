//! FAST tier tool narrowing: forces the convergence-only tool set and records every refusal the strict
//! dispatcher issues for a not-offered tool, so the transcript shows exactly what the model tried and
//! was denied.

use lci_agent_loop::{PolicyAction, TurnOutcome, TurnPolicy, TurnState, convergence_filter};
use lci_agent_types::ToolOutcome;

use crate::tools::fast_refusal;

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
