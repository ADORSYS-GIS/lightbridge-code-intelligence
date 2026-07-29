//! Per-turn state and the `TurnPolicy` contract that policy implementations satisfy.

use lci_agent_tools::{ToolKind, TurnFilter};
use lci_agent_types::{ToolCallReq, ToolOutcome, ToolSpec};

use crate::chat::ChatMessage;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LoopStats {
    pub files_read: usize,
    pub searches: usize,
    pub batches: usize,
    pub successful_writes: usize,
    pub findings_recorded: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCallResult {
    pub call: ToolCallReq,
    pub kind: Option<ToolKind>,
    pub outcome: ToolOutcome,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnOutcome {
    pub assistant: ChatMessage,
    pub results: Vec<ToolCallResult>,
    pub finish_requested: bool,
    pub abort_reason: Option<String>,
}

pub struct TurnState<'a> {
    pub turn: usize,
    pub max_turns: usize,
    pub messages: &'a [ChatMessage],
    pub base_tools: &'a [ToolSpec],
    pub stats: &'a LoopStats,
    /// True once an earlier policy in registration order has forced convergence this turn.
    pub converging: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Nudge(pub String);

impl From<&str> for Nudge {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}

impl From<String> for Nudge {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// Ordered policy effects. Every narrowing is intersected with the accumulated filter.
#[derive(Clone, Debug, PartialEq)]
pub enum PolicyAction {
    Narrow(TurnFilter),
    Inject(Nudge),
    TrimHistory {
        target_tokens: usize,
        convergence: Option<(TurnFilter, Option<Nudge>, Option<serde_json::Value>)>,
    },
    Converge {
        filter: TurnFilter,
        nudge: Nudge,
    },
    GuardOffered,
    RejectFinish(Nudge),
    Record {
        name: Option<&'static str>,
        detail: serde_json::Value,
    },
    ForceFinish {
        reason: &'static str,
    },
    SetFindings(usize),
}

/// Dynamic dispatch is intentional here: an assembly composes heterogeneous policies.
pub trait TurnPolicy: Send {
    fn name(&self) -> &'static str;
    fn before_turn(&mut self, state: &TurnState<'_>) -> Vec<PolicyAction>;
    fn after_turn(&mut self, _state: &TurnState<'_>, _outcome: &TurnOutcome) {}
    fn after_turn_actions(
        &mut self,
        state: &TurnState<'_>,
        outcome: &TurnOutcome,
    ) -> Vec<PolicyAction> {
        self.after_turn(state, outcome);
        Vec::new()
    }
    fn finish_actions(
        &mut self,
        _state: &TurnState<'_>,
        _outcome: &TurnOutcome,
    ) -> Vec<PolicyAction> {
        Vec::new()
    }
    fn exhausted_actions(&mut self, _state: &TurnState<'_>) -> Vec<PolicyAction> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nudge_from_str_and_string_both_wrap_the_message() {
        assert_eq!(Nudge::from("hello"), Nudge("hello".to_string()));
        assert_eq!(
            Nudge::from(String::from("world")),
            Nudge("world".to_string())
        );
    }
}
