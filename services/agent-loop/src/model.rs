//! The model seam: one implementation per host assembly, statically dispatched.

use lci_agent_types::{AssistantTurn, ChatMessage, StepError, ToolSpec};

/// One model request for a single turn — the conversation so far plus the tools offered this turn.
///
/// Generation parameters (temperature, timeouts, retries, streaming) are owned by the concrete
/// [`ModelClient`] implementation, not carried here: they are host/config concerns invisible to the
/// engine (companion doc §3.5).
pub struct ChatRequest<'a> {
    pub messages: &'a [ChatMessage],
    pub tools: &'a [ToolSpec],
}

impl<'a> ChatRequest<'a> {
    #[must_use]
    pub fn new(messages: &'a [ChatMessage], tools: &'a [ToolSpec]) -> Self {
        Self { messages, tools }
    }
}

/// Produces one assistant turn from the conversation.
///
/// Static dispatch (native `async fn` in trait): each host assembly compiles exactly one
/// implementation (the review host's `ChatClient`), so no boxing is needed and the in-step transport
/// retries, rate-limit handling, and streaming stay invisible to the loop. The `async_fn_in_trait`
/// lint does not apply — every consumer is generic over a single concrete `M`.
#[allow(async_fn_in_trait)]
pub trait ModelClient: Send + Sync {
    /// Complete one turn, mapping any escaped transport failure to a [`StepError`] (`Transient` for
    /// retryable transport/5xx/429, `Terminal` for a refused or malformed request).
    async fn complete(&self, request: ChatRequest<'_>) -> Result<AssistantTurn, StepError>;
}
