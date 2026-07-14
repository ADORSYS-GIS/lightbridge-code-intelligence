//! Context-budget and wind-down arithmetic shared by the loop driver and the built-in policies.

use lci_agent_tools::{ReadKind, ToolKind, TurnFilter};
use lci_agent_types::ToolSpec;

use crate::chat::ChatMessage;

/// Conservative chars/4 estimator used only for context safety decisions.
#[must_use]
pub fn estimate_tokens(messages: &[ChatMessage], tools: &[ToolSpec]) -> usize {
    const PER_MESSAGE_OVERHEAD: usize = 4;
    let messages = messages
        .iter()
        .map(|message| {
            let content = message.content.as_deref().map_or(0, str::len);
            let calls = message
                .tool_calls
                .iter()
                .map(|call| call.function.name.len() + call.function.arguments.len())
                .sum::<usize>();
            PER_MESSAGE_OVERHEAD + (content + calls) / 4
        })
        .sum::<usize>();
    let tools = tools
        .iter()
        .map(|tool| {
            (tool.function.name.len()
                + tool.function.description.len()
                + tool.function.parameters.to_string().len())
                / 4
        })
        .sum::<usize>();
    messages + tools
}

/// Replace oldest consumed tool results while preserving assistant/call pairing.
pub fn trim_tool_history(messages: &mut [ChatMessage], tools: &[ToolSpec], target: usize) -> usize {
    const KEEP_RECENT: usize = 2;
    const STUB: &str = "[earlier tool output elided to fit the context budget]";
    let cutoff = messages.len().saturating_sub(KEEP_RECENT);
    let mut estimate = estimate_tokens(messages, tools);
    let mut trimmed = 0;
    for message in messages.iter_mut().take(cutoff) {
        if estimate <= target {
            break;
        }
        let old_len = match message.content.as_deref() {
            Some(content)
                if message.role == "tool" && content.len() > STUB.len() && content != STUB =>
            {
                content.len()
            }
            _ => continue,
        };
        estimate = estimate.saturating_sub((old_len - STUB.len()) / 4);
        message.content = Some(STUB.into());
        trimmed += 1;
    }
    trimmed
}

#[must_use]
pub fn convergence_filter() -> TurnFilter {
    TurnFilter::all()
        .without_kind(ToolKind::ReadOnly(ReadKind::Retrieval))
        .without_kind(ToolKind::ReadOnly(ReadKind::File))
        .without_kind(ToolKind::ReadOnly(ReadKind::Knowledge))
        .without_kind(ToolKind::Progress)
}

#[must_use]
pub fn winddown_turn(max_turns: usize) -> usize {
    const MIN_TURNS: usize = 2;
    if max_turns <= MIN_TURNS {
        return max_turns.saturating_sub(1).max(1);
    }
    let reserve = MIN_TURNS.max(max_turns / 10);
    max_turns
        .saturating_sub(reserve)
        .clamp(1, max_turns.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lci_agent_types::AssistantTurn;

    #[test]
    fn winddown_boundaries_match_the_legacy_contract() {
        assert_eq!(winddown_turn(1), 1);
        assert_eq!(winddown_turn(2), 1);
        assert_eq!(winddown_turn(5), 3);
        assert_eq!(winddown_turn(40), 36);
    }

    #[test]
    fn trim_preserves_recent_messages_and_call_pairing() {
        let mut messages = vec![
            ChatMessage::tool("old", "x".repeat(4_000)),
            ChatMessage::assistant(AssistantTurn {
                content: None,
                tool_calls: Vec::new(),
            }),
            ChatMessage::tool("new", "y".repeat(4_000)),
        ];
        let trimmed = trim_tool_history(&mut messages, &[], 10);
        assert_eq!(trimmed, 1);
        assert_eq!(messages[0].tool_call_id.as_deref(), Some("old"));
        assert!(messages[0].content.as_deref().unwrap().contains("elided"));
        assert_eq!(messages[2].content.as_deref().unwrap(), "y".repeat(4_000));
    }

    #[test]
    fn context_estimate_counts_tool_schemas_and_calls() {
        let base = estimate_tokens(&[ChatMessage::user("12345678")], &[]);
        let with_tool = estimate_tokens(
            &[ChatMessage::user("12345678")],
            &[ToolSpec::function(
                "search",
                "description",
                serde_json::json!({"type":"object"}),
            )],
        );
        assert_eq!(base, 6);
        assert!(with_tool > base);
    }
}
