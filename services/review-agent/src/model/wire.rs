//! Non-stream Chat Completions response wire shapes — the raw `serde` DTOs [`client::send_request`]
//! deserializes a buffered `200` body into, before it is folded into the public [`Completion`]
//! (`crate::model::completion`). Crate-private: nothing outside the transport should see these.

use serde::Deserialize;

use lci_agent_types::ToolCall;

use super::completion::Usage;
use super::serde_ext::null_as_default;

#[derive(Deserialize)]
pub(super) struct ChatResponse {
    pub(super) choices: Vec<Choice>,
    #[serde(default)]
    pub(super) usage: Option<Usage>,
}

#[derive(Deserialize)]
pub(super) struct Choice {
    pub(super) message: ResponseMessage,
    #[serde(default)]
    pub(super) finish_reason: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ResponseMessage {
    #[serde(default)]
    pub(super) role: Option<String>,
    #[serde(default)]
    pub(super) content: Option<String>,
    /// The non-stream chain-of-thought (DeepSeek/GLM lineage), surfaced into [`Completion::reasoning`].
    /// Accept `reasoning` too: some OpenAI-compatible gateways emit the field under that name instead of
    /// `reasoning_content`, which otherwise reads back as empty (`reasoning_chars: 0`) despite the model
    /// thinking (#220 / ADR-0060).
    #[serde(default, alias = "reasoning")]
    pub(super) reasoning_content: Option<String>,
    /// Null-tolerant for the same reason as the streaming path (#411): a backend that sends an
    /// explicit `"tool_calls": null` on a text-only response would otherwise hard-fail the whole
    /// non-stream body. `null_as_default` collapses an absent key and an explicit `null` to an empty
    /// vec alike.
    #[serde(default, deserialize_with = "null_as_default")]
    pub(super) tool_calls: Vec<ToolCall>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_tool_calls_still_parse() {
        // Backend that omits the key on a text answer — must keep working (no regression).
        let resp: ChatResponse = serde_json::from_str(
            r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"hi","reasoning_content":"think"},"finish_reason":"stop"}]}"#,
        )
        .expect("parses");
        let msg = &resp.choices[0].message;
        assert_eq!(msg.content.as_deref(), Some("hi"));
        assert_eq!(msg.reasoning_content.as_deref(), Some("think"));
        assert!(msg.tool_calls.is_empty());
    }

    #[test]
    fn null_tool_calls_no_longer_hard_fail() {
        // Explicit `"tool_calls": null` on a text answer used to fail the WHOLE response
        // ("invalid type: null, expected a sequence"); it must now collapse to an empty vec.
        let resp: ChatResponse = serde_json::from_str(
            r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"Hello","reasoning_content":"reasoned","tool_calls":null},"finish_reason":"stop"}],"usage":null}"#,
        )
        .expect("parses");
        let msg = &resp.choices[0].message;
        assert_eq!(msg.content.as_deref(), Some("Hello"));
        assert_eq!(msg.reasoning_content.as_deref(), Some("reasoned"));
        assert!(msg.tool_calls.is_empty());
    }

    #[test]
    fn real_tool_calls_still_deserialize() {
        let resp: ChatResponse = serde_json::from_str(
            r#"{"choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
        )
        .expect("parses");
        let calls = &resp.choices[0].message.tool_calls;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "read_file");
    }
}
