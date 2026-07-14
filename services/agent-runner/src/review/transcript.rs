//! Reconstructing the ADR-0034 transcript from the loop's sink events plus the model client's per-turn
//! telemetry side-channel.

use lci_agent_clients::TranscriptEntry;
use lci_agent_loop::TranscriptEvent;
use lci_agent_types::ToolOutcome;
use lci_review_agent::model::TurnTelemetry;
use uuid::Uuid;

/// Reconstruct the ADR-0034 transcript rows from the loop's sink events + the model client's per-turn
/// telemetry (sequential model calls ⇒ index == the Nth assistant event). Assistant turns carry their
/// tokens/model from the telemetry side-channel; tool results carry the (bounded) outcome text — the
/// finish/abort terminal outcomes record no tool row, matching the legacy loop. Policy events are not
/// transcript rows.
pub(crate) fn append_transcript(
    transcript: &mut Vec<TranscriptEntry>,
    events: &[TranscriptEvent],
    telemetry: &[TurnTelemetry],
    task_id: Uuid,
) {
    let mut assistant_index = 0usize;
    for event in events {
        match event {
            TranscriptEvent::Assistant { turn, message } => {
                let telemetry = telemetry.get(assistant_index);
                assistant_index += 1;
                // Proof-of-work (epic #137): one concise per-turn line, including the chain-of-thought
                // length (the reliable "how far did it think" signal even when the gateway folds
                // reasoning into `completion_tokens`).
                tracing::info!(
                    task_id = %task_id,
                    turn,
                    model = telemetry.map(|entry| entry.model.as_str()).unwrap_or("?"),
                    prompt_tokens = telemetry.and_then(|entry| entry.prompt_tokens).unwrap_or(-1),
                    completion_tokens = telemetry
                        .and_then(|entry| entry.completion_tokens)
                        .unwrap_or(-1),
                    reasoning_tokens = telemetry
                        .and_then(|entry| entry.reasoning_tokens)
                        .unwrap_or(-1),
                    reasoning_chars = telemetry
                        .and_then(|entry| entry.reasoning.as_deref())
                        .map(|reasoning| reasoning.chars().count())
                        .unwrap_or(0),
                    "agent turn complete"
                );
                transcript.push(TranscriptEntry {
                    role: "assistant".to_string(),
                    content: message.content.clone(),
                    tool_calls: (!message.tool_calls.is_empty())
                        .then(|| serde_json::to_value(&message.tool_calls).unwrap_or_default()),
                    tool_name: None,
                    prompt_tokens: telemetry.and_then(|entry| entry.prompt_tokens),
                    completion_tokens: telemetry.and_then(|entry| entry.completion_tokens),
                    reasoning_tokens: telemetry.and_then(|entry| entry.reasoning_tokens),
                    model: telemetry.map(|entry| entry.model.clone()),
                });
            }
            TranscriptEvent::Tool { call, outcome, .. } => {
                if let ToolOutcome::Continue(result) = outcome {
                    transcript.push(TranscriptEntry {
                        role: "tool".to_string(),
                        content: Some(truncate_on_boundary(result, 2048).to_string()),
                        tool_calls: None,
                        tool_name: Some(call.function.name.clone()),
                        prompt_tokens: None,
                        completion_tokens: None,
                        reasoning_tokens: None,
                        model: None,
                    });
                }
            }
            TranscriptEvent::Policy { .. } => {}
        }
    }
}

/// `s` truncated to at most `max` bytes, never slicing through a multi-byte char.
fn truncate_on_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
