//! Chat wire types: messages, requests, and the model boundary.

use lci_agent_tools::TurnFilter;
use lci_agent_types::{AssistantTurn, StepError, ToolCallReq, ToolSpec};
use serde::{Deserialize, Serialize};

/// One OpenAI-compatible conversation message.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallReq>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self::text("system", content)
    }

    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self::text("user", content)
    }

    #[must_use]
    pub fn assistant(turn: AssistantTurn) -> Self {
        Self {
            role: "assistant".into(),
            content: turn.content,
            tool_calls: turn.tool_calls,
            tool_call_id: None,
        }
    }

    #[must_use]
    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }

    fn text(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
}

fn slice_is_empty<T>(slice: &&[T]) -> bool {
    slice.is_empty()
}

/// Exact request presented to a model implementation.
#[derive(Debug, Serialize)]
pub struct ChatRequest<'a> {
    pub model: &'a str,
    pub messages: &'a [ChatMessage],
    #[serde(skip_serializing_if = "slice_is_empty")]
    pub tools: &'a [ToolSpec],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    #[serde(flatten)]
    pub extra: &'a serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

/// Static model boundary: one model implementation is selected by the assembly.
pub trait ModelClient: Send + Sync {
    async fn complete(&self, request: ChatRequest<'_>) -> Result<AssistantTurn, StepError>;
}

#[derive(Clone, Debug, Default)]
pub struct RequestOptions {
    pub model: String,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: Option<i64>,
    pub stream: Option<bool>,
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Debug)]
pub struct Conversation {
    pub messages: Vec<ChatMessage>,
    pub request: RequestOptions,
    pub initial_filter: TurnFilter,
}

impl Conversation {
    #[must_use]
    pub fn new(messages: Vec<ChatMessage>, request: RequestOptions) -> Self {
        Self {
            messages,
            request,
            initial_filter: TurnFilter::all(),
        }
    }

    #[must_use]
    pub fn with_filter(mut self, filter: TurnFilter) -> Self {
        self.initial_filter = filter;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_request_preserves_usage_options() {
        let messages = [ChatMessage::system("system")];
        let extra = serde_json::Map::new();
        let request = ChatRequest {
            model: "m",
            messages: &messages,
            tools: &[],
            tool_choice: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            stream: Some(true),
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
            extra: &extra,
        };
        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json["stream"], true);
        assert_eq!(json["stream_options"]["include_usage"], true);
        assert!(json.get("tools").is_none());
    }
}
