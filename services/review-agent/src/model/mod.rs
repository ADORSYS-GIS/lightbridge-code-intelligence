//! OpenAI-compatible **Chat Completions** client with function/tool calling (ADR-0026).
//!
//! Talks to `POST {base}/chat/completions` on the eaig gateway — the same gateway and key the review
//! model already uses (`LLM_BASE_URL` / `LLM_API_KEY` / `LLM_MODEL`, mapped from the runner's
//! `ReviewConfig` at the call boundary). Unlike the embeddings base URL, the LLM base URL already
//! includes the `/v1` segment, so we only append `/chat/completions`.
//!
//! This is the [`ModelClient`] the [`lci_agent_loop`] engine drives: it serializes the multi-turn
//! `messages` array (system / user / assistant-with-tool-calls / tool-result), advertises the available
//! `tools`, and returns the assistant's reply — either text or a set of `tool_calls` the loop then
//! dispatches. It deliberately knows nothing about *which* tools exist or *how* to run them; that is
//! the registry's job. It speaks the engine's [`ChatMessage`]/[`ChatRequest`] wire types directly.
//!
//! Split by seam (ADR-0086-style crate hygiene pass): [`client`] owns the [`ChatClient`] transport and
//! the request lifecycle; [`stream`] owns the SSE reassembly path; [`wire`] owns the raw non-stream
//! response DTOs; [`completion`] owns the output types callers consume ([`Completion`]/[`Usage`]/
//! [`TurnTelemetry`]); [`retry`] owns the backoff policy and transient/deterministic error
//! classification; [`http`] owns the `reqwest::Client` construction. This file only re-exports the
//! public surface — no logic lives here.

mod client;
mod completion;
mod http;
mod retry;
mod stream;
mod wire;

pub use client::ChatClient;
pub use completion::{ChatParams, Completion, CompletionTokensDetails, TurnTelemetry, Usage};
pub use lci_agent_types::{AssistantTurn, FunctionCall, FunctionDef, StepError, ToolCall, ToolDef};
pub use retry::{ChatError, RetryPolicy};

/// Default per-request timeout (seconds) for one chat round-trip (ADR-0039). eaig can legitimately take
/// ~2 minutes per turn, so this is deliberately generous. Re-homed here (it was
/// `crate::bootstrap::config::DEFAULT_REQUEST_TIMEOUT_SECS`) so `review-agent` owns no dependency on the
/// runner's config module — the runner maps its `ReviewConfig` timeout onto [`ChatClient::with_timeout`]
/// at the call boundary.
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 180;
