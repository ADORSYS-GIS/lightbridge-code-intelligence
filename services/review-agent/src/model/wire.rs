//! Non-stream Chat Completions response wire shapes — the raw `serde` DTOs [`client::send_request`]
//! deserializes a buffered `200` body into, before it is folded into the public [`Completion`]
//! (`crate::model::completion`). Crate-private: nothing outside the transport should see these.

use serde::Deserialize;

use lci_agent_types::ToolCall;

use super::completion::Usage;

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
    #[serde(default)]
    pub(super) tool_calls: Vec<ToolCall>,
}
