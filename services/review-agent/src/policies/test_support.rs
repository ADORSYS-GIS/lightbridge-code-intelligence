//! Test-only helpers shared by the individual policy submodules' own `#[cfg(test)]` blocks — a scripted
//! [`ToolCallResult`] and a minimal [`TurnState`], so each policy's tests don't hand-roll the same
//! boilerplate.

use lci_agent_loop::{ChatMessage, LoopStats, ToolCallResult, TurnState};
use lci_agent_types::{FunctionCallReq, ToolCallReq, ToolOutcome, ToolSpec};

pub(super) fn call(name: &str, arguments: &str, outcome: ToolOutcome) -> ToolCallResult {
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

pub(super) fn state(turn: usize) -> TurnState<'static> {
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
