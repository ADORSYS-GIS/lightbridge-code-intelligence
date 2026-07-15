//! Per-turn output types: the assembled [`Completion`] the transport returns and its [`Usage`]
//! breakdown. Kept apart from the transport (`client`/`stream`) and wire (`wire`) modules — these are
//! the shapes callers actually consume, independent of how the reply was parsed. Per-turn telemetry
//! (`lci_agent_types::TurnTelemetry`) rides `AssistantTurn::telemetry` instead of a type here — see
//! `client.rs`'s `ModelClient` impl.

use lci_agent_clients::ratelimit::RateLimitSnapshot;
use lci_agent_loop::ChatMessage;
use serde::Deserialize;

/// Per-request generation parameters. All optional — `None` leaves the provider/model default. Mirrors
/// the runner's `ReviewConfig` generation knobs (#71), mapped in at the call boundary.
#[derive(Debug, Clone, Copy, Default)]
pub struct ChatParams {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: Option<i64>,
}

/// The assistant's reply for one turn: its message (text and/or `tool_calls`), the provider's
/// `finish_reason` (e.g. `tool_calls`, `stop`, `length`) so the loop can detect truncation, the token
/// `usage` for the turn (for the transcript/observability, ADR-0034), and the gateway's advertised
/// rate-limit budget at the time of the response ([`RateLimitSnapshot`] — advisory telemetry; empty
/// unless the gateway has the draft-03 headers enabled).
#[derive(Debug, Clone)]
pub struct Completion {
    pub message: ChatMessage,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
    pub rate_limit: RateLimitSnapshot,
    /// The model's chain-of-thought for this turn (`reasoning_content` — DeepSeek/GLM lineage),
    /// reassembled from the stream or read off the non-stream message. `None` when the model/gateway
    /// doesn't emit it. Kept off [`ChatMessage`] on purpose: it is for the transcript/logs only and is
    /// **not** echoed back to the model on the next turn. See `StreamDelta::reasoning_content`.
    pub reasoning: Option<String>,
}

/// Token usage for one completion, as reported by the OpenAI-compatible API. All optional — some
/// gateways omit it.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: Option<i64>,
    #[serde(default)]
    pub completion_tokens: Option<i64>,
    /// Reasoning-model breakdown. `reasoning_tokens` is a SUBSET of `completion_tokens` (the API
    /// already counts it there), surfaced separately so the transcript can split input/output/
    /// reasoning. Absent on non-reasoning models / gateways that omit it.
    #[serde(default)]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
    /// Some gateways (e.g. camer.digital's, observed in prod) report the reasoning slice at the **top
    /// level** of `usage` rather than nested under `completion_tokens_details`. Read both so we don't
    /// silently lose the count. Note: GLM-5.2 via that gateway folds its thinking into
    /// `completion_tokens` and reports this as `0`, so the *text length* of [`Completion::reasoning`]
    /// is the more reliable "how much did it think" signal.
    #[serde(default)]
    pub reasoning_tokens: Option<i64>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct CompletionTokensDetails {
    #[serde(default)]
    pub reasoning_tokens: Option<i64>,
}

impl Usage {
    /// Reasoning tokens for the turn, if the model reported the breakdown. Prefers the OpenAI-style
    /// nested field, falling back to the top-level one some gateways use.
    pub fn reasoning_tokens(&self) -> Option<i64> {
        self.completion_tokens_details
            .and_then(|d| d.reasoning_tokens)
            .or(self.reasoning_tokens)
    }
}
