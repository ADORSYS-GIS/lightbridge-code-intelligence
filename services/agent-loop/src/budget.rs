//! Generic budget arithmetic (companion doc §3.6, `ContextWindowTrim`) — the wind-down turn index
//! plus the conservative token estimate and history trim.
//!
//! These are deliberate *over-estimates* used only to decide when to trim old tool output or wind the
//! run down — never reported as truth. Moved verbatim from the pre-extraction loop so the numbers,
//! and therefore every downstream decision, are byte-identical (ADR-0045).

use lci_agent_types::{ChatMessage, ToolSpec};

/// Reserve `max(WINDDOWN_MIN_TURNS, max_turns / 10)` turns at the tail of the budget for wind-down.
const WINDDOWN_MIN_TURNS: usize = 2;

/// Enter wind-down once the conversation reaches this fraction of the configured context window
/// (ADR-0045), leaving headroom for estimator error and the final verdict turn.
pub const WINDDOWN_TOKEN_FRACTION: f64 = 0.75;

/// The first turn index at which wind-down (reduced tool set + budget message) kicks in. Reserves
/// `max(WINDDOWN_MIN_TURNS, max_turns / 10)` turns at the tail, clamped so it never lands before turn
/// 1 (there is always at least one full-toolset turn). A `max_turns == 1` budget is degenerate — one
/// turn cannot both investigate and wind down — so it returns `1`, which the only turn (`turn == 0`)
/// never reaches; that run gets no wind-down and the exhausted backstop catches it.
#[must_use]
pub fn winddown_turn(max_turns: usize) -> usize {
    if max_turns <= WINDDOWN_MIN_TURNS {
        return max_turns.saturating_sub(1).max(1);
    }
    let reserve = WINDDOWN_MIN_TURNS.max(max_turns / 10);
    max_turns
        .saturating_sub(reserve)
        .clamp(1, max_turns.saturating_sub(1))
}

/// A deliberately conservative token estimate for the messages + advertised tools (ADR-0045). The
/// gateway model isn't OpenAI-tokenized, so an exact tokenizer would be false precision and a heavy
/// dependency; ~chars/4 plus a small per-message overhead over-estimates slightly — which is exactly
/// what a safety budget wants. Used only to decide when to wind down / trim, never reported as truth.
#[must_use]
pub fn estimate_tokens(messages: &[ChatMessage], tools: &[ToolSpec]) -> usize {
    const PER_MESSAGE_OVERHEAD: usize = 4;
    let msgs: usize = messages
        .iter()
        .map(|m| {
            let content = m.content.as_deref().map_or(0, str::len);
            let calls: usize = m
                .tool_calls
                .iter()
                .map(|c| c.function.name.len() + c.function.arguments.len())
                .sum();
            PER_MESSAGE_OVERHEAD + (content + calls) / 4
        })
        .sum();
    // The tool schemas are re-sent every turn, so they count against the window too.
    let tools: usize = tools
        .iter()
        .map(|t| {
            (t.function.name.len()
                + t.function.description.len()
                + t.function.parameters.to_string().len())
                / 4
        })
        .sum();
    msgs + tools
}

/// Shrink the content of the OLDEST `tool`-result messages to a stub until the estimate fits `target`
/// tokens (ADR-0045). Keeps each message and its `tool_call_id` so the assistant↔tool-result pairing
/// the protocol requires stays valid; leaves the most recent few messages untouched (the agent may
/// still be acting on them). Returns how many messages were trimmed.
#[must_use]
pub fn trim_tool_history(messages: &mut [ChatMessage], tools: &[ToolSpec], target: usize) -> usize {
    const KEEP_RECENT: usize = 2;
    const STUB: &str = "[earlier tool output elided to fit the context budget]";
    let cutoff = messages.len().saturating_sub(KEEP_RECENT);
    // Estimate once, then decrement a running total as we trim — `estimate_tokens` JSON-serializes
    // every tool schema, so calling it per iteration would be O(N²) on a long conversation.
    let mut est = estimate_tokens(messages, tools);
    let mut trimmed = 0usize;
    for msg in messages.iter_mut().take(cutoff) {
        if est <= target {
            break;
        }
        let old_len = match msg.content.as_deref() {
            Some(c) if msg.role == "tool" && c.len() > STUB.len() && c != STUB => c.len(),
            _ => continue,
        };
        est = est.saturating_sub((old_len - STUB.len()) / 4);
        msg.content = Some(STUB.to_string());
        trimmed += 1;
    }
    trimmed
}

#[cfg(test)]
mod tests {
    use super::{estimate_tokens, trim_tool_history, winddown_turn};
    use lci_agent_types::{ChatMessage, ToolSpec};

    #[test]
    fn winddown_turn_reserves_a_tail_and_clamps_tiny_budgets() {
        // Degenerate single-turn budget: winddown at 1, unreachable by the only turn (0).
        assert_eq!(winddown_turn(1), 1);
        // Two turns: wind down on the final turn, one investigation turn first.
        assert_eq!(winddown_turn(2), 1);
        // Generous budget: reserve max(2, n/10) at the tail.
        assert_eq!(winddown_turn(40), 36);
        assert_eq!(winddown_turn(10), 8);
    }

    #[test]
    fn estimate_tokens_counts_messages_and_resent_tool_schemas() {
        let messages = vec![ChatMessage::user("x".repeat(40).as_str())];
        let tools = vec![ToolSpec::function(
            "search",
            "d".repeat(40).as_str(),
            serde_json::json!({}),
        )];
        // Non-zero and monotonic in content length — the exact value is an over-estimate by design.
        let base = estimate_tokens(&messages, &tools);
        assert!(base > 0);
        let bigger = estimate_tokens(&[ChatMessage::user("x".repeat(400).as_str())], &tools);
        assert!(bigger > base);
    }

    #[test]
    fn trim_tool_history_stubs_oldest_tool_output_until_it_fits() {
        let mut messages = vec![
            ChatMessage::tool("c1", "A".repeat(4000).as_str()),
            ChatMessage::tool("c2", "B".repeat(4000).as_str()),
            ChatMessage::tool("c3", "C".repeat(4000).as_str()),
            ChatMessage::user("recent"),
        ];
        let tools: Vec<ToolSpec> = vec![];
        let before = estimate_tokens(&messages, &tools);
        let trimmed = trim_tool_history(&mut messages, &tools, before / 2);
        assert!(trimmed >= 1);
        // The two most recent messages are never trimmed (KEEP_RECENT).
        assert_eq!(messages[3].content.as_deref(), Some("recent"));
        assert!(estimate_tokens(&messages, &tools) < before);
    }
}
