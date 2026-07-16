//! Reconstruct the ADR-0034 run transcript from the OpenCode recorder JSONL.
//!
//! The native host builds the transcript from the loop's `TranscriptSink` events; the OpenCode host
//! has no such sink — the in-process recorder (ADR-0095) is its record of what happened. This maps the
//! recorder's reasoning + tool events onto [`TranscriptEntry`]s so a review run on OpenCode is
//! inspectable exactly like a native one: an assistant turn folds its buffered reasoning together with
//! the tool call it then made; each tool result is its own `tool` entry.
//!
//! Token usage isn't carried per-event by the recorder, so the token fields stay `None` (a known gap
//! vs the native `AssistantTurn` telemetry — the transcript is for inspection, not billing).

use lci_agent_clients::TranscriptEntry;
use serde_json::{Value, json};

use super::recorder::{RecorderEvent, normalize_tool_name, result_text};

/// The canonical tool name for a recorded event: the native name when recognized, else the raw id.
fn tool_display_name(event: &RecorderEvent) -> Option<String> {
    let raw = event.tool.as_deref()?;
    Some(
        normalize_tool_name(raw)
            .map(str::to_string)
            .unwrap_or_else(|| raw.to_string()),
    )
}

fn assistant_entry(
    content: Option<String>,
    tool_calls: Option<Value>,
    model: &str,
) -> TranscriptEntry {
    TranscriptEntry {
        role: "assistant".to_string(),
        content,
        tool_calls,
        tool_name: None,
        prompt_tokens: None,
        completion_tokens: None,
        reasoning_tokens: None,
        model: Some(model.to_string()),
    }
}

fn tool_entry(tool_name: Option<String>, content: Option<String>) -> TranscriptEntry {
    TranscriptEntry {
        role: "tool".to_string(),
        content,
        tool_calls: None,
        tool_name,
        prompt_tokens: None,
        completion_tokens: None,
        reasoning_tokens: None,
        model: None,
    }
}

/// Reconstruct transcript entries, in stream order, from the recorder events of a whole review run.
/// `model` stamps each assistant turn (ADR-0034). Reasoning slices coalesce and attach to the tool
/// call they precede; a trailing reasoning block with no following tool call becomes its own
/// assistant entry.
#[must_use]
pub fn transcript_from_recorder(events: &[RecorderEvent], model: &str) -> Vec<TranscriptEntry> {
    let mut entries = Vec::new();
    let mut reasoning = String::new();
    for event in events {
        match event.kind.as_str() {
            "reasoning.part" => {
                if let Some(text) = event
                    .part
                    .as_ref()
                    .and_then(|part| part.get("text"))
                    .and_then(Value::as_str)
                {
                    if !reasoning.is_empty() {
                        reasoning.push('\n');
                    }
                    reasoning.push_str(text);
                }
            }
            "tool.before" => {
                let tool_calls = json!([{
                    "id": event.call_id.clone().unwrap_or_default(),
                    "type": "function",
                    "function": {
                        "name": tool_display_name(event).unwrap_or_default(),
                        "arguments": event.args.clone().unwrap_or_else(|| json!({})).to_string(),
                    }
                }]);
                let content = (!reasoning.is_empty()).then(|| std::mem::take(&mut reasoning));
                entries.push(assistant_entry(content, Some(tool_calls), model));
            }
            "tool.after" => {
                let content = event.result.as_ref().map(result_text);
                entries.push(tool_entry(tool_display_name(event), content));
            }
            _ => {}
        }
    }
    if !reasoning.is_empty() {
        entries.push(assistant_entry(Some(reasoning), None, model));
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{after, before, reasoning};
    use super::*;

    #[test]
    fn folds_reasoning_into_the_following_tool_call_then_records_the_result() {
        let events = vec![
            reasoning("I should read a.rs"),
            before(
                "lightbridge_read_file",
                "c1",
                serde_json::json!({"path": "a.rs"}),
            ),
            after("lightbridge_read_file", "c1", "the source"),
        ];
        let entries = transcript_from_recorder(&events, "eaig/reviewer");

        assert_eq!(entries.len(), 2);
        // Assistant turn: reasoning + the tool call, stamped with the model.
        assert_eq!(entries[0].role, "assistant");
        assert_eq!(entries[0].content.as_deref(), Some("I should read a.rs"));
        assert_eq!(entries[0].model.as_deref(), Some("eaig/reviewer"));
        let call = &entries[0].tool_calls.as_ref().unwrap()[0];
        // Normalized to the native tool name.
        assert_eq!(call["function"]["name"], "read_file");
        assert!(
            call["function"]["arguments"]
                .as_str()
                .unwrap()
                .contains("a.rs")
        );
        // Tool result.
        assert_eq!(entries[1].role, "tool");
        assert_eq!(entries[1].tool_name.as_deref(), Some("read_file"));
        assert_eq!(entries[1].content.as_deref(), Some("the source"));
    }

    #[test]
    fn trailing_reasoning_without_a_tool_call_becomes_its_own_entry() {
        let events = vec![
            reasoning("first thought"),
            before(
                "lightbridge_finish",
                "c1",
                serde_json::json!({"summary": "done"}),
            ),
            after("lightbridge_finish", "c1", "finalize"),
            reasoning("a closing reflection with no tool"),
        ];
        let entries = transcript_from_recorder(&events, "m");
        // finish's assistant turn + its tool entry + the trailing assistant reasoning.
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[2].role, "assistant");
        assert_eq!(
            entries[2].content.as_deref(),
            Some("a closing reflection with no tool")
        );
        assert!(entries[2].tool_calls.is_none());
    }

    #[test]
    fn coalesces_consecutive_reasoning_slices() {
        let events = vec![
            reasoning("part one"),
            reasoning("part two"),
            before("lightbridge_finish", "c1", serde_json::json!({})),
            after("lightbridge_finish", "c1", "ok"),
        ];
        let entries = transcript_from_recorder(&events, "m");
        assert_eq!(entries[0].content.as_deref(), Some("part one\npart two"));
    }
}
