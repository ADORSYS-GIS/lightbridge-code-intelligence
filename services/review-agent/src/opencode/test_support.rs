//! Test-only builders shared by the `opencode` submodules' `#[cfg(test)]` blocks — recorder-event
//! constructors so the recorder / gate / driver tests don't each hand-roll the same JSONL shapes
//! (mirrors `crate::policies::test_support`).

use serde_json::Value;

use super::recorder::RecorderEvent;

/// A `tool.before` event for `tool` (its OpenCode id, prefix and all) with the given call id + input.
pub(super) fn before(tool: &str, call_id: &str, args: Value) -> RecorderEvent {
    RecorderEvent {
        kind: "tool.before".into(),
        tool: Some(tool.into()),
        call_id: Some(call_id.into()),
        args: Some(args),
        result: None,
        part: None,
    }
}

/// An MCP-shaped `tool.after` (`{content:[{type,text}],isError}`) — the shape the mediated review
/// tools actually return at runtime (recorder ADR-0095 note), carrying the dispatch message the gates
/// key on.
pub(super) fn after(tool: &str, call_id: &str, text: &str) -> RecorderEvent {
    RecorderEvent {
        kind: "tool.after".into(),
        tool: Some(tool.into()),
        call_id: Some(call_id.into()),
        args: None,
        result: Some(serde_json::json!({
            "content": [{ "type": "text", "text": text }],
            "isError": false,
        })),
        part: None,
    }
}

/// A `reasoning.part` event (ADR-0060) carrying one reasoning slice.
pub(super) fn reasoning(text: &str) -> RecorderEvent {
    RecorderEvent {
        kind: "reasoning.part".into(),
        tool: None,
        call_id: None,
        args: None,
        result: None,
        part: Some(serde_json::json!({ "type": "reasoning", "text": text })),
    }
}
