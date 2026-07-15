//! Per-turn model telemetry (ADR-0034): token counts and the model's chain-of-thought, carried
//! alongside an [`crate::AssistantTurn`] so it journals and replays with the turn it describes
//! (ADR-0087) instead of riding a separate, non-durable side-channel.

use serde::{Deserialize, Serialize};

/// One turn's model telemetry: which model answered, its token usage, and its reasoning
/// (`reasoning_content` — DeepSeek/GLM lineage), if the provider emitted one. Attached to
/// [`crate::AssistantTurn::telemetry`] by the [`crate::AssistantTurn`]'s originating `ModelClient`
/// impl — never echoed back to the model (nothing here rides `ChatMessage`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TurnTelemetry {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}
