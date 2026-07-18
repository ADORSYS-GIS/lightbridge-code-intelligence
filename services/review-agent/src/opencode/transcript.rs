//! Reconstruct the ADR-0034 run transcript from the OpenCode recorder JSONL.
//!
//! The native host builds the transcript from the loop's `TranscriptSink` events; the OpenCode host
//! has no such sink — the in-process recorder (ADR-0095) is its record of what happened. This maps the
//! recorder's reasoning + visible-content + tool events onto [`TranscriptEntry`]s so a review run on
//! OpenCode is inspectable exactly like a native one: an assistant turn folds its buffered
//! chain-of-thought (`reasoning`) and its visible answer (`content`) together with the tool call it
//! then made; each tool result is its own `tool` entry.
//!
//! The `content` vs `reasoning` split is identical to the native host (ADR-0034, epic #459 / #461):
//! `content` = the model's **visible message/answer**, `reasoning` = its **chain-of-thought**. The
//! recorder emits these as distinct `text.part` / `reasoning.part` events, so they never conflate.
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
    reasoning: Option<String>,
    tool_calls: Option<Value>,
    model: &str,
) -> TranscriptEntry {
    TranscriptEntry {
        role: "assistant".to_string(),
        content,
        reasoning,
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
        reasoning: None,
        tool_calls: None,
        tool_name,
        prompt_tokens: None,
        completion_tokens: None,
        reasoning_tokens: None,
        model: None,
    }
}

/// Append `text` to `buf`, newline-joining consecutive slices (the recorder streams a part in slices).
fn push_slice(buf: &mut String, text: &str) {
    if !buf.is_empty() {
        buf.push('\n');
    }
    buf.push_str(text);
}

/// Reconstruct transcript entries, in stream order, from the recorder events of a whole review run.
/// `model` stamps each assistant turn (ADR-0034). Reasoning (`reasoning.part`, chain-of-thought) and
/// content (`text.part`, visible answer) slices each coalesce into their OWN buffer and attach to the
/// tool call they precede — the assistant turn carries `content` = visible text, `reasoning` = CoT,
/// never conflating them (epic #459 / #461). A trailing block with no following tool call becomes its
/// own assistant entry.
#[must_use]
pub fn transcript_from_recorder(events: &[RecorderEvent], model: &str) -> Vec<TranscriptEntry> {
    let mut entries = Vec::new();
    let mut reasoning = String::new();
    let mut content = String::new();
    // Buffer consecutive `tool.before`s so parallel calls (ADR-0042 batching) group into ONE assistant
    // turn's `tool_calls`, flushed on the first following `tool.after` (gemini #444) — not one assistant
    // entry per call.
    let mut pending: Vec<Value> = Vec::new();
    let flush = |entries: &mut Vec<TranscriptEntry>,
                 content: &mut String,
                 reasoning: &mut String,
                 pending: &mut Vec<Value>| {
        if pending.is_empty() {
            return;
        }
        let content = (!content.is_empty()).then(|| std::mem::take(content));
        let reasoning = (!reasoning.is_empty()).then(|| std::mem::take(reasoning));
        entries.push(assistant_entry(
            content,
            reasoning,
            Some(Value::Array(std::mem::take(pending))),
            model,
        ));
    };
    for event in events {
        match event.kind.as_str() {
            "reasoning.part" => {
                if let Some(text) = event
                    .part
                    .as_ref()
                    .and_then(|part| part.get("text"))
                    .and_then(Value::as_str)
                {
                    push_slice(&mut reasoning, text);
                }
            }
            "text.part" => {
                if let Some(text) = event
                    .part
                    .as_ref()
                    .and_then(|part| part.get("text"))
                    .and_then(Value::as_str)
                {
                    push_slice(&mut content, text);
                }
            }
            "tool.before" => {
                // `Some(Null)` args must serialize to `{}`, not `"null"` (parity with
                // `cycle_turn_outcome`, gemini #444).
                let arguments = match event.args.as_ref() {
                    None | Some(Value::Null) => "{}".to_string(),
                    Some(args) => args.to_string(),
                };
                pending.push(json!({
                    "id": event.call_id.clone().unwrap_or_default(),
                    "type": "function",
                    "function": {
                        "name": tool_display_name(event).unwrap_or_default(),
                        "arguments": arguments,
                    }
                }));
            }
            "tool.after" => {
                flush(&mut entries, &mut content, &mut reasoning, &mut pending);
                let result = event.result.as_ref().map(result_text);
                entries.push(tool_entry(tool_display_name(event), result));
            }
            _ => {}
        }
    }
    // Trailing tool calls with no result (cycle cut), else a trailing content/reasoning block that had
    // no following tool call (e.g. the model's closing answer).
    if pending.is_empty() {
        if !content.is_empty() || !reasoning.is_empty() {
            entries.push(assistant_entry(
                (!content.is_empty()).then_some(content),
                (!reasoning.is_empty()).then_some(reasoning),
                None,
                model,
            ));
        }
    } else {
        flush(&mut entries, &mut content, &mut reasoning, &mut pending);
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{after, before, reasoning, text};
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
        // Assistant turn: chain-of-thought lands in `reasoning` (NOT `content`) + the tool call,
        // stamped with the model. With no visible `text.part`, `content` stays None (epic #461).
        assert_eq!(entries[0].role, "assistant");
        assert_eq!(entries[0].reasoning.as_deref(), Some("I should read a.rs"));
        assert_eq!(entries[0].content, None);
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
        // The trailing chain-of-thought lands in `reasoning`, not `content` (epic #461).
        assert_eq!(
            entries[2].reasoning.as_deref(),
            Some("a closing reflection with no tool")
        );
        assert_eq!(entries[2].content, None);
        assert!(entries[2].tool_calls.is_none());
    }

    /// The F1+F3 fix (epic #459 / #461): a turn's VISIBLE answer (`text.part`) lands in `content`
    /// and its chain-of-thought (`reasoning.part`) in `reasoning` — the two are captured and stored
    /// distinctly, never conflated. Before the fix, content was dropped and reasoning was written to
    /// `content`.
    #[test]
    fn separates_visible_content_from_reasoning_on_the_assistant_turn() {
        let events = vec![
            reasoning("Let me weigh the null path"),
            text("This looks safe; the guard covers it."),
            before(
                "lightbridge_finish",
                "c1",
                serde_json::json!({"summary": "ok"}),
            ),
            after("lightbridge_finish", "c1", "will finalize"),
        ];
        let entries = transcript_from_recorder(&events, "m");
        assert_eq!(entries[0].role, "assistant");
        assert_eq!(
            entries[0].content.as_deref(),
            Some("This looks safe; the guard covers it."),
            "visible text → content"
        );
        assert_eq!(
            entries[0].reasoning.as_deref(),
            Some("Let me weigh the null path"),
            "chain-of-thought → reasoning"
        );
        // They are genuinely distinct strings, not the same value copied into both columns.
        assert_ne!(entries[0].content, entries[0].reasoning);
    }

    /// A trailing visible answer (`text.part`) with no following tool call becomes its own assistant
    /// entry carrying `content` (not `reasoning`).
    #[test]
    fn trailing_visible_content_without_a_tool_call_becomes_its_own_entry() {
        let events = vec![text("Review complete. DONE.")];
        let entries = transcript_from_recorder(&events, "m");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].role, "assistant");
        assert_eq!(entries[0].content.as_deref(), Some("Review complete. DONE."));
        assert_eq!(entries[0].reasoning, None);
        assert!(entries[0].tool_calls.is_none());
    }

    /// Parallel tool calls (two `tool.before`s before any `tool.after`) group into ONE assistant turn
    /// with both `tool_calls` (gemini #444), not two consecutive assistant entries.
    #[test]
    fn groups_parallel_tool_calls_into_one_assistant_turn() {
        let events = vec![
            before(
                "lightbridge_read_file",
                "a",
                serde_json::json!({"path": "a.rs"}),
            ),
            before(
                "lightbridge_read_file",
                "b",
                serde_json::json!({"path": "b.rs"}),
            ),
            after("lightbridge_read_file", "a", "src a"),
            after("lightbridge_read_file", "b", "src b"),
        ];
        let entries = transcript_from_recorder(&events, "m");
        // One assistant turn (2 tool_calls) + two tool results.
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].role, "assistant");
        let calls = entries[0].tool_calls.as_ref().unwrap().as_array().unwrap();
        assert_eq!(calls.len(), 2, "both parallel calls in one turn: {calls:?}");
        assert_eq!(entries[1].role, "tool");
        assert_eq!(entries[2].role, "tool");
    }

    /// `Some(Value::Null)` args serialize to `{}`, not the string `"null"` (gemini #444).
    #[test]
    fn null_args_serialize_as_empty_object() {
        let mut ev = before("lightbridge_finish", "c1", serde_json::json!(null));
        ev.args = Some(Value::Null);
        let entries = transcript_from_recorder(&[ev], "m");
        let args = entries[0].tool_calls.as_ref().unwrap()[0]["function"]["arguments"]
            .as_str()
            .unwrap();
        assert_eq!(args, "{}");
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
        // Consecutive chain-of-thought slices coalesce into the `reasoning` buffer (epic #461).
        assert_eq!(entries[0].reasoning.as_deref(), Some("part one\npart two"));
    }

    /// Consecutive visible-answer slices coalesce into the `content` buffer, mirroring reasoning.
    #[test]
    fn coalesces_consecutive_content_slices() {
        let events = vec![
            text("part one"),
            text("part two"),
            before("lightbridge_finish", "c1", serde_json::json!({})),
            after("lightbridge_finish", "c1", "ok"),
        ];
        let entries = transcript_from_recorder(&events, "m");
        assert_eq!(entries[0].content.as_deref(), Some("part one\npart two"));
    }
}
