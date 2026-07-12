//! Open-mode loop policies. `open` reuses the **shared** generic budget policies
//! ([`lci_agent_loop::policy`]) — there is nothing review-specific (coverage gates, refute gates, fast
//! tier) about a coding agent, so the policy set is just the budgets that bound investigation and steer
//! the loop toward a terminal `propose_pr`/`abort`. Keeping the set here (not inline in the flow) gives
//! open-mode budget tuning one home.

use lci_agent_loop::TurnPolicy;
use lci_agent_loop::policy::{ContextWindowTrim, ReadBudgets, TurnBudget, WindDown};

/// The numeric envelope for one `open` run. Mirrors the shape of `review`'s `ReviewRunParams` (a plain
/// param bag the host fills in), minus the review-only knobs. `context_window` is `None` to disable
/// context budgeting.
#[derive(Clone, Debug)]
pub struct OpenBudgets {
    /// Turn ceiling for the whole run.
    pub max_turns: usize,
    /// Max read-only tool calls run concurrently within one turn.
    pub max_batch_size: usize,
    /// Investigation-batch budget: once spent, wind-down narrowing fires.
    pub max_batches: usize,
    /// Cumulative `read_file` budget.
    pub max_files_read: usize,
    /// Cumulative retrieval (`grep`/`find_files`) budget.
    pub max_searches: usize,
    /// Per-run circuit-breaker threshold on consecutive transient turn failures.
    pub circuit_breaker_threshold: u32,
    /// Model context window in tokens; `None` disables budgeting.
    pub context_window: Option<usize>,
}

impl Default for OpenBudgets {
    fn default() -> Self {
        // Coding agents legitimately run longer than a review (investigate → edit → build → test →
        // iterate), so the turn ceiling is generous; the pod's wall-clock deadline is the hard cap.
        Self {
            max_turns: 60,
            max_batch_size: 4,
            max_batches: 12,
            max_files_read: 60,
            max_searches: 40,
            circuit_breaker_threshold: 3,
            context_window: None,
        }
    }
}

/// Compose the open-mode policy vector in registration (= evaluation) order: context trim → wind-down →
/// read budgets → turn budget. All are the shared generic policies.
#[must_use]
pub fn build_policies(budgets: &OpenBudgets) -> Vec<Box<dyn TurnPolicy>> {
    vec![
        Box::new(ContextWindowTrim::new(budgets.context_window)),
        Box::new(WindDown::new(budgets.max_turns, budgets.max_batches)),
        Box::new(ReadBudgets::new(
            budgets.max_files_read,
            budgets.max_searches,
        )),
        Box::new(TurnBudget::new(budgets.max_turns)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_set_is_the_four_generic_budgets_in_order() {
        let policies = build_policies(&OpenBudgets::default());
        let names: Vec<_> = policies.iter().map(|p| p.name()).collect();
        assert_eq!(
            names,
            ["context_trim", "wind_down", "read_budgets", "halfway"]
        );
    }
}
