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

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use lci_agent_clients::ratelimit::{self, RateLimitSnapshot};
use lci_agent_loop::{ChatMessage, ChatRequest, ModelClient, StreamOptions};
pub use lci_agent_types::{AssistantTurn, FunctionCall, FunctionDef, StepError, ToolCall, ToolDef};

/// Default per-request timeout (seconds) for one chat round-trip (ADR-0039). eaig can legitimately take
/// ~2 minutes per turn, so this is deliberately generous. Re-homed here (it was
/// `crate::bootstrap::config::DEFAULT_REQUEST_TIMEOUT_SECS`) so `review-agent` owns no dependency on the
/// runner's config module — the runner maps its `ReviewConfig` timeout onto [`ChatClient::with_timeout`]
/// at the call boundary.
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 180;

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
    /// **not** echoed back to the model on the next turn. See [`StreamDelta::reasoning_content`].
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

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
    /// The non-stream chain-of-thought (DeepSeek/GLM lineage), surfaced into [`Completion::reasoning`].
    /// Accept `reasoning` too: some OpenAI-compatible gateways emit the field under that name instead of
    /// `reasoning_content`, which otherwise reads back as empty (`reasoning_chars: 0`) despite the model
    /// thinking (#220 / ADR-0060).
    #[serde(default, alias = "reasoning")]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
}

// ── Streaming (SSE) chunk shapes (spike) ─────────────────────────────────────────────────────────
// A streamed completion arrives as `data: {chunk}\n\n` events; each chunk carries *deltas* in
// `choices[0].delta`: `content` fragments, and `tool_calls` whose `function.name`/`arguments` are
// split across chunks and reassembled by `index`. The final chunk carries `finish_reason` and (with
// `include_usage`) `usage`.

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
    /// A provider may report a mid-stream failure as a `data: {"error": …}` event (no `choices`).
    /// Surfaced so the collector fails the turn instead of finishing with an empty message.
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    /// Reasoning-model thinking deltas (DeepSeek/GLM lineage), reassembled across chunks into
    /// [`Completion::reasoning`] for the transcript/logs (epic #137 proof-of-work). Not echoed back to
    /// the model on the next turn. `reasoning` alias: some gateways stream the deltas under that key, so
    /// without it a streamed reasoning model logs `reasoning_chars: 0` (the deep-tier GLM-5.2 symptom).
    #[serde(default, alias = "reasoning")]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<StreamToolCall>,
}

#[derive(Deserialize)]
struct StreamToolCall {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<StreamFn>,
    /// Provider round-trip blob (Gemini's `thought_signature` envelope). Streamed on the tool-call
    /// delta — captured verbatim so it survives into the reassembled [`ToolCall::extra_content`] and is
    /// echoed back on the next turn. Arrives whole (not split like `arguments`), so last-writer-wins.
    #[serde(default)]
    extra_content: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct StreamFn {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// One tool call being reassembled across stream chunks.
#[derive(Default)]
struct ToolCallAcc {
    id: String,
    name: String,
    arguments: String,
    /// Provider round-trip blob (Gemini `thought_signature`), captured verbatim from the delta.
    extra_content: Option<serde_json::Value>,
}

/// Retry/backoff policy for one chat turn (ADR-0039). Retries fire **only** on transient failures
/// (connect/timeout, HTTP 429, HTTP 5xx); a 4xx other than 429 is deterministic and never retried.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Retries on a transient failure (total attempts = `max_retries + 1`).
    pub max_retries: u32,
    /// Base backoff; attempt *n* (0-indexed) sleeps roughly `base * 2^n` plus deterministic jitter.
    pub base_backoff: Duration,
    /// Ceiling on a single backoff so a high attempt count can't sleep absurdly long.
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            base_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(8),
        }
    }
}

impl RetryPolicy {
    /// Backoff for `attempt` (0 = the wait *before* the first retry). Exponential, capped at
    /// `max_backoff`, plus a small **deterministic** jitter seeded by the attempt index — so the
    /// schedule is reproducible in tests (no clock, no RNG) yet de-synchronises retries a little.
    fn backoff(&self, attempt: u32) -> Duration {
        let factor = 1u64 << attempt.min(16); // 2^attempt, clamped so the shift can't overflow
        let base = self
            .base_backoff
            .saturating_mul(factor.min(u32::MAX as u64) as u32);
        let capped = base.min(self.max_backoff);
        // Deterministic jitter in [0, 250ms): a cheap hash of the attempt index, no SystemTime/RNG.
        let jitter_ms = (attempt as u64).wrapping_mul(2_654_435_761) % 250;
        capped.saturating_add(Duration::from_millis(jitter_ms))
    }
}

/// Why a turn failed, so the loop can decide whether a transient error is worth a retry/failover vs.
/// a deterministic one that should fail fast.
#[derive(Debug)]
pub struct ChatError {
    pub error: anyhow::Error,
    /// `true` for connect/timeout, HTTP 429, or HTTP 5xx — the loop retries/fails over on these only.
    pub transient: bool,
    /// `Retry-After` seconds parsed off a 429, when present — the loop honours it over its own backoff.
    pub retry_after: Option<Duration>,
}

impl std::fmt::Display for ChatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.error)
    }
}

/// Chat Completions client for the review model.
pub struct ChatClient {
    url: String,
    api_key: String,
    model: String,
    http: reqwest::Client,
    /// Gateway attribution headers (epic #89), added to every request so token spend is billed to the
    /// right project. Empty unless set via [`ChatClient::with_attribution`].
    attribution: reqwest::header::HeaderMap,
    /// Provider-specific request fields merged verbatim into every chat-completion body — generation
    /// knobs the typed params don't cover, notably a **reasoning budget** (e.g. `thinking`,
    /// `reasoning_effort`) to stop a reasoning model over-thinking. From `review.extra`; empty by
    /// default. Per-model (set per client, like the model id + timeout). The operator owns correctness;
    /// fields the gateway/model doesn't recognise are ignored.
    extra: serde_json::Map<String, serde_json::Value>,
    /// Stream the response (SSE) and collect it ourselves (spike). Off by default. When on, the per-
    /// request total timeout is complemented by an inter-chunk **idle** timeout so a long-but-
    /// progressing turn isn't killed, while a true stall still fails fast.
    stream: bool,
    /// Inter-chunk idle timeout used on the streaming path — the max silence between SSE chunks before
    /// the turn is treated as stalled. Seeded from the per-request timeout.
    idle_timeout: Duration,
    /// Per-turn retry/backoff policy (ADR-0039) applied on the [`ModelClient::complete`] engine path, so
    /// a transient turn failure is retried before the loop's circuit breaker sees it — preserving the
    /// legacy `complete_with_retry` behaviour. From `review.resilience.max_retries` (default policy
    /// otherwise). The engine still classifies the *final* failure via [`StepError`].
    retry_policy: RetryPolicy,
    /// Interior-mutability side-channel for per-turn model telemetry (ADR-0034): [`AssistantTurn`] and
    /// the engine's `TranscriptEvent::Assistant` drop token/reasoning fields, so [`ModelClient::complete`]
    /// records the model + prompt/completion/reasoning tokens + reasoning text here per call. Sequential
    /// calls ⇒ index == turn; the host zips it with the sink's assistant messages into the transcript.
    /// Drained via [`ChatClient::telemetry_handle`].
    telemetry: Arc<Mutex<Vec<TurnTelemetry>>>,
}

/// One turn's model telemetry captured by [`ModelClient::complete`] (ADR-0034). The engine's
/// [`AssistantTurn`] carries only the model-visible content + tool calls, so the token/reasoning
/// fields ride this side-channel and are re-joined with the transcript host-side.
#[derive(Debug, Clone)]
pub struct TurnTelemetry {
    pub model: String,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub reasoning: Option<String>,
}

impl ChatClient {
    /// `base_url` is the LLM gateway base **including** the `/v1` segment (`LLM_BASE_URL`); the model
    /// is the chat model id (`LLM_MODEL`). Uses the default per-request timeout
    /// ([`DEFAULT_REQUEST_TIMEOUT_SECS`]).
    pub fn new(base_url: &str, api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::with_timeout(
            base_url,
            api_key,
            model,
            Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS),
        )
    }

    /// Like [`new`](Self::new) but with an explicit per-request timeout (ADR-0039). eaig can take
    /// ~2 minutes per turn, so callers pass a generous value (default 180s).
    pub fn with_timeout(
        base_url: &str,
        api_key: impl Into<String>,
        model: impl Into<String>,
        request_timeout: Duration,
    ) -> Self {
        Self {
            url: format!("{}/chat/completions", base_url.trim_end_matches('/')),
            api_key: api_key.into(),
            model: model.into(),
            http: build_http_client(Some(request_timeout)),
            attribution: reqwest::header::HeaderMap::new(),
            extra: serde_json::Map::new(),
            stream: false,
            idle_timeout: request_timeout,
            retry_policy: RetryPolicy::default(),
            telemetry: Arc::default(),
        }
    }

    /// Return a copy of this client that targets a different model id (same gateway/key/timeout/retry).
    /// Cheap: clones the shared `reqwest::Client`. The telemetry side-channel is fresh — a retargeted
    /// client is a distinct logical model, so its turns don't interleave with the original's.
    pub fn for_model(&self, model: impl Into<String>) -> Self {
        Self {
            url: self.url.clone(),
            api_key: self.api_key.clone(),
            model: model.into(),
            http: self.http.clone(),
            attribution: self.attribution.clone(),
            extra: self.extra.clone(),
            stream: self.stream,
            idle_timeout: self.idle_timeout,
            retry_policy: self.retry_policy,
            telemetry: Arc::default(),
        }
    }

    /// Set the per-turn retry/backoff policy applied on the engine [`ModelClient::complete`] path.
    #[must_use]
    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry_policy = policy;
        self
    }

    /// A shared handle to the per-turn telemetry captured by [`ModelClient::complete`]. Grab this
    /// **before** moving the client into the loop; drain it (`handle.lock()…drain(..)`) after the run to
    /// zip token/reasoning fields back into the transcript (sequential calls ⇒ index == turn).
    #[must_use]
    pub fn telemetry_handle(&self) -> Arc<Mutex<Vec<TurnTelemetry>>> {
        Arc::clone(&self.telemetry)
    }

    /// Set provider-specific passthrough request fields (e.g. a reasoning budget). Merged verbatim into
    /// every chat-completion body via `#[serde(flatten)]`. **Reserved structural keys are stripped with
    /// a warning** — the flattened map serializes *after* the named fields, so a colliding key would
    /// otherwise silently overwrite a structural field (`model`/`messages`/…). See [`ChatClient::extra`].
    pub fn with_extra(mut self, mut extra: serde_json::Map<String, serde_json::Value>) -> Self {
        const RESERVED: &[&str] = &[
            "model",
            "messages",
            "tools",
            "tool_choice",
            "temperature",
            "top_p",
            "max_tokens",
            "stream",
        ];
        for key in RESERVED {
            if extra.remove(*key).is_some() {
                tracing::warn!(
                    key,
                    "ignoring reserved key in review.extra (it would override a structural request field)"
                );
            }
        }
        self.extra = extra;
        self
    }

    /// Enable streaming (SSE) collection (spike). The reply is reassembled from `data:` chunks with an
    /// inter-chunk idle timeout, instead of one buffered response under the whole-request timeout.
    pub fn with_stream(mut self, stream: bool) -> Self {
        self.stream = stream;
        if stream {
            // Streaming: drop the whole-request total timeout so a long-but-progressing turn isn't
            // capped — the per-chunk `idle_timeout` in `collect_stream` is the stall detector instead.
            self.http = build_http_client(None);
        }
        self
    }

    /// The model id this client targets.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Attach gateway attribution headers (epic #89). Unparseable header names/values are skipped (the
    /// keys are our own controlled values, so that shouldn't happen).
    pub fn with_attribution(mut self, headers: &[(String, String)]) -> Self {
        use reqwest::header::{HeaderName, HeaderValue};
        for (name, value) in headers {
            match (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                (Ok(n), Ok(v)) => {
                    self.attribution.insert(n, v);
                }
                _ => tracing::warn!(header = %name, "skipping unparseable attribution header"),
            }
        }
        self
    }

    /// The provider-passthrough request fields in force — **after** [`with_extra`](Self::with_extra)
    /// stripped any reserved structural keys. On the engine path the request `extra` rides the
    /// conversation's `RequestOptions`, so the host reads this to carry the *sanitized* map through
    /// rather than re-flattening the raw operator config.
    #[must_use]
    pub fn extra(&self) -> &serde_json::Map<String, serde_json::Value> {
        &self.extra
    }

    /// Build the engine [`ChatRequest`] for one turn from this client's config + the turn's messages
    /// and advertised tools. The engine's request type is field-for-field the wire shape, so the legacy
    /// inherent helpers and the [`ModelClient`] impl POST the identical body.
    fn build_request<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        tools: &'a [ToolDef],
        params: ChatParams,
    ) -> ChatRequest<'a> {
        ChatRequest {
            model: &self.model,
            messages,
            tools,
            tool_choice: (!tools.is_empty()).then_some("auto"),
            temperature: params.temperature,
            top_p: params.top_p,
            max_tokens: params.max_tokens,
            stream: self.stream.then_some(true),
            stream_options: self.stream.then_some(StreamOptions {
                include_usage: true,
            }),
            extra: &self.extra,
        }
    }

    /// One completion turn: send the conversation so far + the advertised `tools`, return the
    /// assistant's reply. `tools` may be empty for a plain completion.
    ///
    /// On a non-2xx response the **response body** is read (bounded) and folded into the error, so a
    /// gateway rejection (bad model, quota, validation) surfaces a real reason instead of a bare
    /// status code — this is the key fix for "the review failed without saying why" (ADR-0039).
    pub async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDef],
        params: ChatParams,
    ) -> anyhow::Result<Completion> {
        self.complete_inner(messages, tools, params)
            .await
            .map_err(|e| e.error)
    }

    /// [`complete`](Self::complete) with retry/backoff on transient failures (ADR-0039). Retries up to
    /// `policy.max_retries` times on connect/timeout, HTTP 429, or HTTP 5xx — honouring a 429's
    /// `Retry-After` over the computed backoff — and returns immediately on success or a deterministic
    /// 4xx. The returned [`ChatError`] tells the caller whether the *final* failure was transient (so
    /// the loop can decide whether to keep going toward the circuit breaker).
    pub async fn complete_with_retry(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDef],
        params: ChatParams,
        policy: RetryPolicy,
    ) -> Result<Completion, ChatError> {
        let request = self.build_request(messages, tools, params);
        self.send_with_retry(&request, policy).await
    }

    /// Send a pre-built request under `policy`'s retry/backoff (ADR-0039). Shared by the legacy
    /// [`complete_with_retry`](Self::complete_with_retry) and the [`ModelClient`] engine path, so
    /// per-turn retry is identical on both.
    async fn send_with_retry(
        &self,
        request: &(impl Serialize + ?Sized),
        policy: RetryPolicy,
    ) -> Result<Completion, ChatError> {
        let mut attempt = 0u32;
        loop {
            match self.send_request(request).await {
                Ok(completion) => return Ok(completion),
                Err(err) => {
                    if !err.transient || attempt >= policy.max_retries {
                        return Err(err);
                    }
                    let wait = err
                        .retry_after
                        .map(|d| d.min(policy.max_backoff))
                        .unwrap_or_else(|| policy.backoff(attempt));
                    tracing::warn!(
                        model = %self.model,
                        attempt = attempt + 1,
                        max_retries = policy.max_retries,
                        backoff_ms = wait.as_millis() as u64,
                        retry_after = err.retry_after.is_some(),
                        error = %err,
                        "transient chat failure; retrying after backoff"
                    );
                    tokio::time::sleep(wait).await;
                    attempt += 1;
                }
            }
        }
    }

    /// The single-attempt request, returning a classified [`ChatError`] on failure.
    async fn complete_inner(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDef],
        params: ChatParams,
    ) -> Result<Completion, ChatError> {
        let request = self.build_request(messages, tools, params);
        self.send_request(&request).await
    }

    /// POST a pre-serialized request body, then parse the completion — classifying any failure as a
    /// [`ChatError`] (transient vs deterministic). The single home of the HTTP / stream / idle-timeout /
    /// rate-limit machinery, shared by the legacy helpers and the [`ModelClient`] impl.
    async fn send_request(
        &self,
        request: &(impl Serialize + ?Sized),
    ) -> Result<Completion, ChatError> {
        let response = self
            .http
            .post(&self.url)
            .bearer_auth(&self.api_key)
            .headers(self.attribution.clone())
            .json(request)
            .send()
            .await
            .map_err(|e| {
                // Only connect/timeout transport errors are worth a retry. A request-construction
                // error (`is_request`: bad URL, invalid headers, serialization) is deterministic — it
                // will fail identically every attempt, so don't burn retries on it.
                let transient = e.is_timeout() || e.is_connect();
                ChatError {
                    error: anyhow::Error::new(e).context("chat completions request failed"),
                    transient,
                    retry_after: None,
                }
            })?;

        let status = response.status();
        if !status.is_success() {
            // Read the body (bounded) so the failure is legible. 429 + 5xx are transient; other 4xx
            // are deterministic (bad request, auth, unknown model) and must NOT be retried.
            let retry_after = ratelimit::retry_after(response.headers());
            let body = response.text().await.unwrap_or_default();
            let snippet = truncate_on_boundary(&body, 1024);
            let transient =
                status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
            return Err(ChatError {
                error: anyhow::anyhow!(
                    "chat completions API returned {status}: {}",
                    if snippet.is_empty() {
                        "(empty body)"
                    } else {
                        snippet
                    }
                ),
                transient,
                retry_after: retry_after.filter(|_| transient),
            });
        }

        // Capture the gateway's advertised budget before the body consumes the response (Copy, so the
        // borrow on `headers()` ends here). Advisory only — see `lci_agent_clients::ratelimit`.
        // Captured once here so both the streaming and non-streaming paths carry it.
        let rate_limit = RateLimitSnapshot::from_headers(response.headers());

        // Streaming path (spike): collect the SSE chunks ourselves under a per-chunk idle timeout.
        if self.stream {
            return self.collect_stream(response, rate_limit).await;
        }

        let response: ChatResponse = response.json().await.map_err(|e| ChatError {
            // A malformed 2xx body is not a transport problem — don't retry it.
            error: anyhow::Error::new(e).context("parsing chat completions response"),
            transient: false,
            retry_after: None,
        })?;

        let usage = response.usage;
        let choice = response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ChatError {
                error: anyhow::anyhow!("chat completions response had no choices"),
                transient: false,
                retry_after: None,
            })?;

        Ok(Completion {
            finish_reason: choice.finish_reason,
            usage,
            rate_limit,
            reasoning: choice
                .message
                .reasoning_content
                .filter(|r| !r.trim().is_empty()),
            message: ChatMessage {
                role: choice
                    .message
                    .role
                    .unwrap_or_else(|| "assistant".to_string()),
                content: choice.message.content,
                tool_calls: choice.message.tool_calls,
                tool_call_id: None,
            },
        })
    }

    /// Collect a streamed (SSE) completion (spike): reassemble `content` + `tool_calls` deltas (and the
    /// final `usage`) from `data:` chunks, bounding the silence between chunks by `idle_timeout` so a
    /// stalled stream fails fast (transient → retryable) while a long-but-progressing one completes.
    async fn collect_stream(
        &self,
        response: reqwest::Response,
        rate_limit: RateLimitSnapshot,
    ) -> Result<Completion, ChatError> {
        use futures::StreamExt;
        let transient = |error: anyhow::Error| ChatError {
            error,
            transient: true,
            retry_after: None,
        };

        let mut stream = response.bytes_stream();
        // Raw byte buffer: HTTP chunks split at arbitrary byte boundaries, so we must NOT decode each
        // chunk on its own (a multi-byte UTF-8 char split across chunks would corrupt). We strip `\r`
        // as bytes arrive (normalising CRLF SSE `\r\n\r\n` → `\n\n`), then decode only *complete*
        // events. (Gemini/Codex review on #206.)
        let mut buf: Vec<u8> = Vec::new();
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut finish_reason: Option<String> = None;
        let mut usage: Option<Usage> = None;
        let mut tools: Vec<ToolCallAcc> = Vec::new();
        let mut done = false; // saw an explicit `data: [DONE]`

        loop {
            let chunk = match tokio::time::timeout(self.idle_timeout, stream.next()).await {
                Ok(Some(Ok(bytes))) => bytes,
                Ok(Some(Err(e))) => {
                    return Err(transient(
                        anyhow::Error::new(e).context("reading chat stream chunk"),
                    ));
                }
                Ok(None) => break, // stream closed — completeness checked after the loop
                Err(_) => {
                    return Err(transient(anyhow::anyhow!(
                        "chat stream idle for {:?} (no chunk) — treating as a stall",
                        self.idle_timeout
                    )));
                }
            };
            buf.extend(chunk.iter().copied().filter(|&b| b != b'\r'));

            // SSE events are separated by a blank line; drain + decode each *complete* event whole.
            while let Some(pos) = buf.windows(2).position(|w| w == b"\n\n") {
                let event = buf.drain(..pos + 2).collect::<Vec<u8>>();
                let event = String::from_utf8_lossy(&event);
                for line in event.lines() {
                    let Some(data) = line.strip_prefix("data:").map(str::trim) else {
                        continue;
                    };
                    if data == "[DONE]" {
                        done = true;
                        continue;
                    }
                    let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) else {
                        continue; // keep-alive / unparseable fragment — skip
                    };
                    // A mid-stream provider error (no `choices`) must fail the turn, not finish empty.
                    if let Some(err) = chunk.error {
                        return Err(transient(anyhow::anyhow!(
                            "chat stream returned an error event: {err}"
                        )));
                    }
                    if chunk.usage.is_some() {
                        usage = chunk.usage;
                    }
                    let Some(choice) = chunk.choices.into_iter().next() else {
                        continue;
                    };
                    if choice.finish_reason.is_some() {
                        finish_reason = choice.finish_reason;
                    }
                    if let Some(c) = choice.delta.content {
                        content.push_str(&c);
                    }
                    if let Some(r) = choice.delta.reasoning_content {
                        reasoning.push_str(&r);
                    }
                    for tc in choice.delta.tool_calls {
                        if tools.len() <= tc.index {
                            tools.resize_with(tc.index + 1, ToolCallAcc::default);
                        }
                        let acc = &mut tools[tc.index];
                        if let Some(id) = tc.id {
                            acc.id = id;
                        }
                        if let Some(ec) = tc.extra_content {
                            acc.extra_content = Some(ec);
                        }
                        if let Some(f) = tc.function {
                            if let Some(n) = f.name {
                                acc.name.push_str(&n);
                            }
                            if let Some(a) = f.arguments {
                                acc.arguments.push_str(&a);
                            }
                        }
                    }
                }
            }
        }

        // An upstream/proxy that closed the stream before a terminal signal left us with a partial
        // (possibly half-built tool call). Treat it as transient so the turn retries, rather than
        // returning a "successful" empty/partial completion. (Codex review on #206.)
        if !done && finish_reason.is_none() {
            return Err(transient(anyhow::anyhow!(
                "chat stream closed before completion (no finish_reason / [DONE])"
            )));
        }

        let tool_calls: Vec<ToolCall> = tools
            .into_iter()
            .filter(|a| !a.id.is_empty() || !a.name.is_empty())
            .map(|a| ToolCall {
                id: a.id,
                kind: "function".to_string(),
                function: FunctionCall {
                    name: a.name,
                    arguments: a.arguments,
                },
                extra_content: a.extra_content,
            })
            .collect();

        Ok(Completion {
            finish_reason,
            usage,
            rate_limit,
            reasoning: (!reasoning.trim().is_empty()).then_some(reasoning),
            message: ChatMessage {
                role: "assistant".to_string(),
                content: (!content.is_empty()).then_some(content),
                tool_calls,
                tool_call_id: None,
            },
        })
    }
}

impl ModelClient for ChatClient {
    /// Drive one engine turn: POST the engine's [`ChatRequest`] verbatim under this client's retry
    /// policy, record the turn's telemetry (model + tokens + reasoning) on the side-channel, and return
    /// the model-visible [`AssistantTurn`]. Failures map to [`StepError`]: transient
    /// (timeout/connect/429/5xx/stream-stall) preserves the `Retry-After` hint; deterministic
    /// (4xx/malformed/no-choices) folds the response body into the terminal reason so the loop's
    /// context-overflow detection ("context length", …) still matches.
    async fn complete(&self, request: ChatRequest<'_>) -> Result<AssistantTurn, StepError> {
        let completion = self
            .send_with_retry(&request, self.retry_policy)
            .await
            .map_err(chat_error_to_step)?;
        // Sequential per-turn push (calls are serialized by the loop ⇒ index == turn). The engine
        // records exactly one `TranscriptEvent::Assistant` per successful turn, so the host zips the
        // Nth telemetry entry with the Nth assistant message.
        self.telemetry
            .lock()
            .expect("telemetry mutex")
            .push(TurnTelemetry {
                model: self.model.clone(),
                prompt_tokens: completion.usage.and_then(|u| u.prompt_tokens),
                completion_tokens: completion.usage.and_then(|u| u.completion_tokens),
                reasoning_tokens: completion.usage.and_then(|u| u.reasoning_tokens()),
                reasoning: completion.reasoning,
            });
        Ok(AssistantTurn {
            content: completion.message.content,
            tool_calls: completion.message.tool_calls,
        })
    }
}

/// Map a classified transport failure onto the engine's [`StepError`]. Transient failures carry the
/// `Retry-After` hint through; a deterministic failure folds the (body-bearing) error text into the
/// terminal reason so the loop's context-overflow detection still matches the gateway's message.
fn chat_error_to_step(err: ChatError) -> StepError {
    if err.transient {
        StepError::transient(err.error, err.retry_after)
    } else {
        StepError::terminal(format!("{:#}", err.error))
    }
}

/// `s` truncated to at most `max` bytes, never slicing through a multi-byte char (mirrors the helper
/// in the agent loop; kept local so the transport layer has no cross-module dep).
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

/// Build the HTTP client, additionally trusting the internal CA PEM at `LLM_CA_CERT` (falling back to
/// `EMBEDDINGS_CA_CERT`, since the chat and embeddings endpoints share the eaig gateway and its
/// private CA — `ClusterIssuer/self-signed-ca`, which the default rustls/webpki roots don't include).
/// Absent both, the default client (public roots) is used. `add_root_certificate` augments the default
/// roots, it doesn't replace them.
fn build_http_client(total_timeout: Option<Duration>) -> reqwest::Client {
    // A connect timeout always applies. The whole-request `total_timeout` is set for the buffered
    // (non-stream) path (ADR-0039: generous — eaig can take ~2 min/turn); the streaming path passes
    // `None` so a long-but-progressing turn isn't capped — its bound is the per-chunk idle timeout.
    let mut builder = reqwest::Client::builder().connect_timeout(Duration::from_secs(10));
    if let Some(total) = total_timeout {
        builder = builder.timeout(total);
    }
    if let Some((path, cert)) = ca_cert() {
        match cert {
            Ok(cert) => {
                builder = builder.add_root_certificate(cert);
                tracing::info!(%path, "chat: trusting extra CA");
            }
            Err(error) => {
                tracing::warn!(%error, %path, "chat: could not load CA cert; using default roots");
            }
        }
    }
    builder
        .build()
        .expect("building the chat HTTP client with default roots cannot fail")
}

/// The first configured internal-CA path (`LLM_CA_CERT`, else `EMBEDDINGS_CA_CERT`) and its parsed
/// certificate, or `None` when neither is set.
fn ca_cert() -> Option<(String, anyhow::Result<reqwest::Certificate>)> {
    for var in ["LLM_CA_CERT", "EMBEDDINGS_CA_CERT"] {
        if let Ok(path) = std::env::var(var) {
            let cert = std::fs::read(&path)
                .map_err(anyhow::Error::from)
                .and_then(|pem| Ok(reqwest::Certificate::from_pem(&pem)?));
            return Some((path, cert));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{bearer_token, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn search_tool() -> ToolDef {
        ToolDef::function(
            "vector_semantic_search",
            "Search the repo by meaning.",
            serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"],
            }),
        )
    }

    #[tokio::test]
    async fn complete_sends_model_messages_tools_and_parses_tool_calls() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(bearer_token("key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "index": 0,
                    "finish_reason": "tool_calls",
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "vector_semantic_search",
                                "arguments": "{\"query\":\"session expiry\"}"
                            }
                        }]
                    }
                }]
            })))
            .mount(&server)
            .await;

        // base URL includes /v1, like LLM_BASE_URL.
        let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "qwen-coder");
        let out = client
            .complete(
                &[
                    ChatMessage::system("review the diff"),
                    ChatMessage::user("@lightbridge review"),
                ],
                &[search_tool()],
                ChatParams {
                    temperature: Some(0.2),
                    max_tokens: Some(4096),
                    ..ChatParams::default()
                },
            )
            .await
            .expect("complete");

        assert_eq!(out.finish_reason.as_deref(), Some("tool_calls"));
        assert!(out.message.content.is_none());
        assert_eq!(out.message.tool_calls.len(), 1);
        let call = &out.message.tool_calls[0];
        assert_eq!(call.id, "call_1");
        assert_eq!(call.function.name, "vector_semantic_search");
        assert_eq!(call.function.arguments, "{\"query\":\"session expiry\"}");

        // The request we sent carries the model, both messages, the advertised tool, tool_choice, and
        // the generation params; unset params are omitted.
        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["model"], "qwen-coder");
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(
            body["tools"][0]["function"]["name"],
            "vector_semantic_search"
        );
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["temperature"], serde_json::json!(0.2));
        assert_eq!(body["max_tokens"], serde_json::json!(4096));
        assert!(body.get("top_p").is_none(), "unset params are omitted");
    }

    // A passthrough (`review.extra`) — e.g. a reasoning budget to cap an over-reasoning model like
    // glm-5 — is flattened verbatim into the request body, so an operator can tune it without a code
    // change.
    #[tokio::test]
    async fn with_extra_flattens_passthrough_params_into_the_request_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_reply()))
            .mount(&server)
            .await;

        let mut extra = serde_json::Map::new();
        extra.insert(
            "thinking".to_string(),
            serde_json::json!({ "type": "disabled" }),
        );
        extra.insert("reasoning_effort".to_string(), serde_json::json!("low"));
        let client =
            ChatClient::new(&format!("{}/v1", server.uri()), "key", "glm-5").with_extra(extra);

        client
            .complete(&[ChatMessage::user("hi")], &[], ChatParams::default())
            .await
            .expect("complete");

        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        // Passthrough fields land at the TOP LEVEL of the body (flattened), beside model/messages.
        assert_eq!(body["thinking"], serde_json::json!({ "type": "disabled" }));
        assert_eq!(body["reasoning_effort"], "low");
        assert_eq!(body["model"], "glm-5");
    }

    // The default (no passthrough) adds nothing — an empty map flattens to zero fields.
    #[tokio::test]
    async fn empty_extra_adds_no_fields() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_reply()))
            .mount(&server)
            .await;
        let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m");
        client
            .complete(&[ChatMessage::user("hi")], &[], ChatParams::default())
            .await
            .expect("complete");
        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert!(body.get("thinking").is_none());
        assert!(body.get("reasoning_effort").is_none());
    }

    // A reserved structural key placed in `extra` (operator typo / footgun) is stripped, so it can NOT
    // override `model`/`temperature`/… via the flatten merge. A non-reserved key still passes through.
    #[tokio::test]
    async fn with_extra_strips_reserved_keys() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_reply()))
            .mount(&server)
            .await;

        let mut extra = serde_json::Map::new();
        extra.insert("model".to_string(), serde_json::json!("evil-override"));
        extra.insert("temperature".to_string(), serde_json::json!(1.9));
        extra.insert("reasoning_effort".to_string(), serde_json::json!("low"));
        let client =
            ChatClient::new(&format!("{}/v1", server.uri()), "key", "real-model").with_extra(extra);

        client
            .complete(
                &[ChatMessage::user("hi")],
                &[],
                ChatParams {
                    temperature: Some(0.2),
                    ..ChatParams::default()
                },
            )
            .await
            .expect("complete");

        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["model"], "real-model", "extra cannot override model");
        assert_eq!(
            body["temperature"],
            serde_json::json!(0.2),
            "extra cannot override the structural temperature"
        );
        assert_eq!(
            body["reasoning_effort"], "low",
            "non-reserved key passes through"
        );
    }

    // Streaming spike: a tool call whose `name`/`arguments` are split across SSE chunks is reassembled
    // by `index` into the same `Completion` the non-stream path would produce, with the final usage.
    #[tokio::test]
    async fn stream_reassembles_tool_call_deltas_and_usage() {
        let server = MockServer::start().await;
        let events = [
            serde_json::json!({"choices":[{"delta":{"role":"assistant","content":""}}]}),
            serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"vector_semantic_search","arguments":"{\"query\":"}}]}}]}),
            serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"auth\"}"}}]}}]}),
            serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}),
        ];
        let mut sse = String::new();
        for e in &events {
            sse.push_str(&format!("data: {e}\n\n"));
        }
        sse.push_str("data: [DONE]\n\n");

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&server)
            .await;

        let client =
            ChatClient::new(&format!("{}/v1", server.uri()), "key", "glm-5").with_stream(true);
        let out = client
            .complete(
                &[ChatMessage::user("hi")],
                &[search_tool()],
                ChatParams::default(),
            )
            .await
            .expect("stream completes");

        // The request asked for a stream + usage.
        let body: serde_json::Value =
            serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);

        // The fragmented tool call is reassembled verbatim.
        assert_eq!(out.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(out.message.tool_calls.len(), 1);
        let call = &out.message.tool_calls[0];
        assert_eq!(call.id, "call_1");
        assert_eq!(call.function.name, "vector_semantic_search");
        assert_eq!(call.function.arguments, r#"{"query":"auth"}"#);
        // Usage from the final chunk is captured.
        assert_eq!(out.usage.and_then(|u| u.prompt_tokens), Some(10));
    }

    // Non-stream: a GLM/DeepSeek `reasoning_content` on the message is surfaced into
    // `Completion::reasoning`, and a top-level `usage.reasoning_tokens` (the shape camer.digital's
    // gateway returns) is read even though it isn't nested under `completion_tokens_details`.
    #[tokio::test]
    async fn non_stream_captures_reasoning_content_and_top_level_reasoning_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "index": 0,
                    "finish_reason": "stop",
                    "message": {
                        "role": "assistant",
                        "content": "Final answer.",
                        "reasoning_content": "Let me think step by step: 1, 2, 3."
                    }
                }],
                "usage": { "prompt_tokens": 19, "completion_tokens": 219, "reasoning_tokens": 0 }
            })))
            .mount(&server)
            .await;

        let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "glm-5");
        let out = client
            .complete(&[ChatMessage::user("hi")], &[], ChatParams::default())
            .await
            .expect("completes");

        assert_eq!(
            out.reasoning.as_deref(),
            Some("Let me think step by step: 1, 2, 3.")
        );
        assert_eq!(out.message.content.as_deref(), Some("Final answer."));
        // Top-level reasoning_tokens is found by the accessor (here it's the gateway's `0`, not absent).
        assert_eq!(out.usage.and_then(|u| u.reasoning_tokens()), Some(0));
    }

    // Streaming: `reasoning_content` deltas are reassembled into `Completion::reasoning`, separate from
    // the visible `content`, and not echoed into the assistant message.
    #[tokio::test]
    async fn stream_reassembles_reasoning_content_deltas() {
        let server = MockServer::start().await;
        let events = [
            serde_json::json!({"choices":[{"delta":{"role":"assistant","reasoning_content":"think "}}]}),
            serde_json::json!({"choices":[{"delta":{"reasoning_content":"harder"}}]}),
            serde_json::json!({"choices":[{"delta":{"content":"the answer"}}]}),
            serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":9}}),
        ];
        let mut sse = String::new();
        for e in &events {
            sse.push_str(&format!("data: {e}\n\n"));
        }
        sse.push_str("data: [DONE]\n\n");

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&server)
            .await;

        let client =
            ChatClient::new(&format!("{}/v1", server.uri()), "key", "glm-5").with_stream(true);
        let out = client
            .complete(&[ChatMessage::user("hi")], &[], ChatParams::default())
            .await
            .expect("stream completes");

        assert_eq!(out.reasoning.as_deref(), Some("think harder"));
        assert_eq!(out.message.content.as_deref(), Some("the answer"));
    }

    // CRLF SSE (standards-compliant gateways) must parse identically: the byte buffer strips `\r`, so
    // `\r\n\r\n` normalises to the `\n\n` event boundary instead of never matching. (#206 review.)
    #[tokio::test]
    async fn stream_handles_crlf_line_endings() {
        let server = MockServer::start().await;
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hello \"}}]}\r\n\r\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"world\"},\"finish_reason\":\"stop\"}]}\r\n\r\n\
                   data: [DONE]\r\n\r\n";
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&server)
            .await;
        let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m").with_stream(true);
        let out = client
            .complete(&[ChatMessage::user("hi")], &[], ChatParams::default())
            .await
            .expect("crlf stream completes");
        assert_eq!(out.message.content.as_deref(), Some("hello world"));
        assert_eq!(out.finish_reason.as_deref(), Some("stop"));
    }

    // The streaming path must carry the gateway's rate-limit headers onto the Completion too — the
    // headers are on the response before the SSE body is consumed. Regression guard alongside the
    // non-streaming `complete_parses_rate_limit_headers`: the #206 streaming refactor dropped this
    // wiring on both paths, so each path needs its own guard (lightbridge review on #209).
    #[tokio::test]
    async fn stream_parses_rate_limit_headers() {
        let server = MockServer::start().await;
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n\
                   data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .insert_header("x-ratelimit-limit", "1000")
                    .insert_header("x-ratelimit-remaining", "40")
                    .insert_header("x-ratelimit-reset", "12")
                    .set_body_string(sse),
            )
            .mount(&server)
            .await;
        let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m").with_stream(true);
        let out = client
            .complete(&[ChatMessage::user("hi")], &[], ChatParams::default())
            .await
            .expect("stream completes");
        assert_eq!(out.message.content.as_deref(), Some("ok"));
        assert_eq!(out.rate_limit.limit, Some(1000));
        assert_eq!(out.rate_limit.remaining, Some(40));
        assert_eq!(out.rate_limit.reset, Some(Duration::from_secs(12)));
        assert!(
            out.rate_limit.is_low(0.1),
            "40/1000 is below the 10% threshold"
        );
    }

    // A stream that closes before a terminal signal ([DONE] / finish_reason) is a truncated response —
    // surfaced as a transient error so the turn retries, not a "successful" partial completion. (#206.)
    #[tokio::test]
    async fn stream_truncated_without_finish_is_transient_error() {
        let server = MockServer::start().await;
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"partial...\"}}]}\n\n";
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;
        let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m").with_stream(true);
        let err = client
            .complete(&[ChatMessage::user("hi")], &[], ChatParams::default())
            .await
            .expect_err("a truncated stream is an error");
        assert!(
            format!("{err:#}").contains("closed before completion"),
            "got: {err:#}"
        );
    }

    #[tokio::test]
    async fn complete_parses_a_plain_text_reply_and_omits_tools_when_none() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "finish_reason": "stop",
                    "message": { "role": "assistant", "content": "looks good" }
                }]
            })))
            .mount(&server)
            .await;

        let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m");
        let out = client
            .complete(&[ChatMessage::user("hi")], &[], ChatParams::default())
            .await
            .expect("complete");
        assert_eq!(out.message.content.as_deref(), Some("looks good"));
        assert!(out.message.tool_calls.is_empty());

        // With no tools, neither `tools` nor `tool_choice` is sent.
        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    // The gateway's draft-03 rate-limit headers are parsed onto the completion (advisory telemetry).
    // Regression guard: the #206 streaming refactor dropped this wiring; keep a test so it can't
    // silently vanish again.
    #[tokio::test]
    async fn complete_parses_rate_limit_headers() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-ratelimit-limit", "1000")
                    .insert_header("x-ratelimit-remaining", "40")
                    .insert_header("x-ratelimit-reset", "12")
                    .set_body_json(serde_json::json!({
                        "choices": [{ "finish_reason": "stop",
                            "message": { "role": "assistant", "content": "ok" } }]
                    })),
            )
            .mount(&server)
            .await;

        let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m");
        let out = client
            .complete(&[ChatMessage::user("hi")], &[], ChatParams::default())
            .await
            .expect("complete");
        assert_eq!(out.rate_limit.limit, Some(1000));
        assert_eq!(out.rate_limit.remaining, Some(40));
        assert_eq!(out.rate_limit.reset, Some(Duration::from_secs(12)));
        assert!(
            out.rate_limit.is_low(0.1),
            "40/1000 is below the 10% threshold"
        );
    }

    #[tokio::test]
    async fn complete_surfaces_an_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m");
        let err = client
            .complete(&[ChatMessage::user("hi")], &[], ChatParams::default())
            .await
            .expect_err("500 is an error");
        assert!(format!("{err:#}").contains("returned 500"), "got: {err:#}");
    }

    // ── ADR-0039 resilience tests ───────────────────────────────────────────────────────────────

    fn ok_reply() -> serde_json::Value {
        serde_json::json!({
            "choices": [{ "finish_reason": "stop",
                "message": { "role": "assistant", "content": "ok" } }]
        })
    }

    fn fast_policy() -> RetryPolicy {
        // Tiny backoff so tests don't actually sleep meaningfully.
        RetryPolicy {
            max_retries: 2,
            base_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
        }
    }

    // A 5xx is transient → retried; once the gateway recovers, the turn succeeds.
    #[tokio::test]
    async fn retries_on_5xx_then_succeeds() {
        let server = MockServer::start().await;
        // First call: 503. Then: 200.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_reply()))
            .mount(&server)
            .await;

        let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m");
        let out = client
            .complete_with_retry(
                &[ChatMessage::user("hi")],
                &[],
                ChatParams::default(),
                fast_policy(),
            )
            .await
            .expect("recovers after a transient 503");
        assert_eq!(out.message.content.as_deref(), Some("ok"));
    }

    // A 400 is deterministic → NOT retried (exactly one request hits the server) and the body surfaces.
    #[tokio::test]
    async fn does_not_retry_on_400_and_surfaces_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": { "message": "unknown model 'm'" }
            })))
            .mount(&server)
            .await;

        let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m");
        let err = client
            .complete_with_retry(
                &[ChatMessage::user("hi")],
                &[],
                ChatParams::default(),
                fast_policy(),
            )
            .await
            .expect_err("400 is deterministic");
        assert!(!err.transient, "400 is not transient");
        let msg = format!("{err}");
        assert!(msg.contains("returned 400"), "status surfaced: {msg}");
        assert!(msg.contains("unknown model"), "body surfaced: {msg}");

        // Exactly one request — no retry.
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1, "400 must not be retried");
    }

    // The HTTP error body is folded into the error returned by the plain `complete`.
    #[tokio::test]
    async fn error_body_is_surfaced_in_complete() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(502).set_body_string("upstream connect error or disconnect"),
            )
            .mount(&server)
            .await;

        let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m");
        let err = client
            .complete(&[ChatMessage::user("hi")], &[], ChatParams::default())
            .await
            .expect_err("502");
        let msg = format!("{err:#}");
        assert!(msg.contains("502"), "status: {msg}");
        assert!(msg.contains("upstream connect error"), "body: {msg}");
    }

    // A per-request timeout aborts a slow response and classifies it transient.
    #[tokio::test]
    async fn times_out_a_slow_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(ok_reply())
                    .set_delay(Duration::from_millis(300)),
            )
            .mount(&server)
            .await;

        // 50ms timeout < 300ms delay → times out.
        let client = ChatClient::with_timeout(
            &format!("{}/v1", server.uri()),
            "key",
            "m",
            Duration::from_millis(50),
        );
        let err = client
            .complete_inner(&[ChatMessage::user("hi")], &[], ChatParams::default())
            .await
            .expect_err("should time out");
        assert!(err.transient, "a timeout is transient");
        assert!(format!("{err}").contains("request failed"), "got: {err}");
    }

    // `for_model` retargets the model id while sharing the gateway/key/timeout.
    #[tokio::test]
    async fn for_model_retargets_the_model_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_reply()))
            .mount(&server)
            .await;

        let primary = ChatClient::new(&format!("{}/v1", server.uri()), "key", "primary-model");
        let secondary = primary.for_model("fallback-model");
        assert_eq!(secondary.model(), "fallback-model");
        secondary
            .complete(&[ChatMessage::user("hi")], &[], ChatParams::default())
            .await
            .expect("fallback client works");

        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["model"], "fallback-model");
    }

    // Backoff is exponential, capped, and jittered deterministically (no clock/RNG in tests).
    #[test]
    fn backoff_is_exponential_capped_and_deterministic() {
        let p = RetryPolicy {
            max_retries: 5,
            base_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(2),
        };
        // attempt 0 ≈ 100ms, attempt 1 ≈ 200ms, attempt 2 ≈ 400ms (plus <250ms jitter).
        assert!(p.backoff(0) >= Duration::from_millis(100));
        assert!(p.backoff(0) < Duration::from_millis(350));
        assert!(p.backoff(2) >= Duration::from_millis(400));
        // High attempt is capped near max_backoff (+ jitter), never unbounded.
        assert!(p.backoff(20) <= Duration::from_secs(2) + Duration::from_millis(250));
        // Deterministic: same input → same output.
        assert_eq!(p.backoff(3), p.backoff(3));
    }

    #[test]
    fn tool_messages_serialize_with_id_and_assistant_tool_calls_round_trip() {
        // A tool-result message carries role + content + tool_call_id, no tool_calls.
        let tool_msg = ChatMessage::tool("call_1", "results...");
        let v = serde_json::to_value(&tool_msg).unwrap();
        assert_eq!(v["role"], "tool");
        assert_eq!(v["tool_call_id"], "call_1");
        assert!(v.get("tool_calls").is_none(), "empty tool_calls omitted");

        // An assistant turn with tool_calls round-trips (we echo it back into the next request).
        let assistant = ChatMessage {
            role: "assistant".to_string(),
            content: None,
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                kind: "function".to_string(),
                function: FunctionCall {
                    name: "submit_findings".to_string(),
                    arguments: "{}".to_string(),
                },
                extra_content: None,
            }],
            tool_call_id: None,
        };
        let round: ChatMessage =
            serde_json::from_value(serde_json::to_value(&assistant).unwrap()).unwrap();
        assert_eq!(round, assistant);
    }

    // Gemini 3 attaches an opaque `thought_signature` to each tool call under
    // `extra_content.google.thought_signature`, then **400s the *next* request** if it isn't echoed
    // back verbatim ("Function call is missing a thought_signature in functionCall parts" — RunID
    // 0a210c73). The client must parse the blob off the response tool call and re-serialize it
    // unchanged when that assistant turn is sent again. Verifies both halves.
    #[tokio::test]
    async fn tool_call_extra_content_is_captured_and_echoed_back() {
        let server = MockServer::start().await;
        let signature = serde_json::json!({ "google": { "thought_signature": "abc123==" } });
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "finish_reason": "tool_calls",
                    "message": {
                        "role": "assistant",
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": { "name": "read_file", "arguments": "{}" },
                            "extra_content": { "google": { "thought_signature": "abc123==" } }
                        }]
                    }
                }]
            })))
            .mount(&server)
            .await;

        let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "gemini-3-pro");
        let out = client
            .complete(
                &[ChatMessage::user("hi")],
                &[search_tool()],
                ChatParams::default(),
            )
            .await
            .expect("complete");

        // Parsed off the response verbatim.
        assert_eq!(
            out.message.tool_calls[0].extra_content.as_ref(),
            Some(&signature)
        );

        // And re-serialized verbatim when the assistant turn is echoed back into the next request —
        // the exact round-trip Gemini requires (missing → 400).
        let echoed = serde_json::to_value(&out.message).unwrap();
        assert_eq!(echoed["tool_calls"][0]["extra_content"], signature);
    }

    // A tool call with no provider blob (any non-Gemini provider, or the non-first call of a Gemini
    // parallel batch, which Gemini leaves unsigned) must NOT emit `extra_content: null` — the field is
    // simply absent from the wire, so it can't inject a spurious `null` a strict gateway might reject.
    #[test]
    fn tool_call_without_extra_content_omits_the_field() {
        let call = ToolCall {
            id: "c".to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: "read_file".to_string(),
                arguments: "{}".to_string(),
            },
            extra_content: None,
        };
        let v = serde_json::to_value(&call).unwrap();
        assert!(
            v.get("extra_content").is_none(),
            "None must be omitted, not serialized as null"
        );
    }

    // Streaming: Gemini streams the `thought_signature` envelope on the tool-call delta. It must be
    // captured into the reassembled `ToolCall::extra_content` (alongside the fragmented arguments) so
    // it survives the echo-back, exactly as the non-stream path does.
    #[tokio::test]
    async fn stream_captures_tool_call_extra_content() {
        let server = MockServer::start().await;
        let events = [
            serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"{}"},"extra_content":{"google":{"thought_signature":"sig=="}}}]}}]}),
            serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
        ];
        let mut sse = String::new();
        for e in &events {
            sse.push_str(&format!("data: {e}\n\n"));
        }
        sse.push_str("data: [DONE]\n\n");

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&server)
            .await;

        let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "gemini-3-pro")
            .with_stream(true);
        let out = client
            .complete(
                &[ChatMessage::user("hi")],
                &[search_tool()],
                ChatParams::default(),
            )
            .await
            .expect("stream completes");

        assert_eq!(
            out.message.tool_calls[0].extra_content,
            Some(serde_json::json!({ "google": { "thought_signature": "sig==" } }))
        );
    }

    // The signature usually arrives on the *first* tool-call delta (with `id`/`name`), while later
    // deltas carry only `arguments` fragments and omit `extra_content` entirely. A later delta must not
    // clobber the captured signature. This also covers a provider that emits an explicit
    // `"extra_content": null` on a follow-up chunk: `Option<Value>` deserializes JSON `null` to `None`
    // (serde_json's `deserialize_option` maps `null` → none), so the `if let Some(ec)` guard skips it —
    // no defensive `is_null()` check needed (gemini-code-assist review on #262). Regression guard.
    #[tokio::test]
    async fn stream_extra_content_survives_later_deltas() {
        let server = MockServer::start().await;
        let events = [
            serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"{\"pa"},"extra_content":{"google":{"thought_signature":"sig=="}}}]}}]}),
            // A follow-up delta: more argument bytes, and an explicit null extra_content.
            serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"x\"}"},"extra_content":null}]}}]}),
            serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
        ];
        let mut sse = String::new();
        for e in &events {
            sse.push_str(&format!("data: {e}\n\n"));
        }
        sse.push_str("data: [DONE]\n\n");

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&server)
            .await;

        let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "gemini-3-pro")
            .with_stream(true);
        let out = client
            .complete(
                &[ChatMessage::user("hi")],
                &[search_tool()],
                ChatParams::default(),
            )
            .await
            .expect("stream completes");

        // Arguments reassembled across the two deltas, and the signature from delta 1 is intact.
        let call = &out.message.tool_calls[0];
        assert_eq!(call.function.arguments, r#"{"path":"x"}"#);
        assert_eq!(
            call.extra_content,
            Some(serde_json::json!({ "google": { "thought_signature": "sig==" } })),
            "a later delta (incl. explicit null) must not clobber the captured signature"
        );
    }

    // ── ModelClient (engine) path ─────────────────────────────────────────────────────────────────

    /// Build the engine request the loop presents to a [`ModelClient`], borrowing `messages`/`tools`.
    fn engine_request<'a>(
        messages: &'a [ChatMessage],
        tools: &'a [ToolDef],
        extra: &'a serde_json::Map<String, serde_json::Value>,
    ) -> ChatRequest<'a> {
        ChatRequest {
            model: "m",
            messages,
            tools,
            tool_choice: (!tools.is_empty()).then_some("auto"),
            temperature: None,
            top_p: None,
            max_tokens: None,
            stream: None,
            stream_options: None,
            extra,
        }
    }

    // The engine path returns the model-visible `AssistantTurn` and records the turn's token/reasoning
    // telemetry on the side-channel (ADR-0034), keyed positionally to the turn.
    #[tokio::test]
    async fn model_client_complete_returns_turn_and_records_telemetry() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "finish_reason": "tool_calls",
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "reasoning_content": "thinking...",
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": { "name": "read_file", "arguments": "{}" }
                        }]
                    }
                }],
                "usage": { "prompt_tokens": 11, "completion_tokens": 7,
                    "completion_tokens_details": { "reasoning_tokens": 3 } }
            })))
            .mount(&server)
            .await;

        let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m");
        let telemetry = client.telemetry_handle();
        let messages = [ChatMessage::user("hi")];
        let tools = [search_tool()];
        let extra = serde_json::Map::new();
        let turn = ModelClient::complete(&client, engine_request(&messages, &tools, &extra))
            .await
            .expect("engine turn");

        assert!(turn.content.is_none());
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].function.name, "read_file");

        let recorded = telemetry.lock().unwrap();
        assert_eq!(recorded.len(), 1, "one turn ⇒ one telemetry row");
        assert_eq!(recorded[0].model, "m");
        assert_eq!(recorded[0].prompt_tokens, Some(11));
        assert_eq!(recorded[0].completion_tokens, Some(7));
        assert_eq!(recorded[0].reasoning_tokens, Some(3));
        assert_eq!(recorded[0].reasoning.as_deref(), Some("thinking..."));
    }

    // A deterministic 4xx maps to a TERMINAL StepError with the response body folded into the reason —
    // the exact text the loop's context-overflow detection matches on. No telemetry is recorded.
    #[tokio::test]
    async fn model_client_maps_deterministic_failure_to_terminal_error_with_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string("This model's maximum context length is 8192 tokens"),
            )
            .mount(&server)
            .await;

        let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m");
        let telemetry = client.telemetry_handle();
        let messages = [ChatMessage::user("hi")];
        let extra = serde_json::Map::new();
        let err = ModelClient::complete(&client, engine_request(&messages, &[], &extra))
            .await
            .expect_err("400 is terminal");
        assert!(!err.is_transient(), "a 400 is deterministic");
        assert!(
            err.to_string().to_lowercase().contains("context length"),
            "body folded into the terminal reason for overflow detection: {err}"
        );
        assert!(
            telemetry.lock().unwrap().is_empty(),
            "a failed turn records no telemetry"
        );
    }

    // A transient 5xx is retried under the client's own retry policy (as the legacy `complete_with_retry`
    // did) and succeeds once the gateway recovers — the loop never sees the transient failure.
    #[tokio::test]
    async fn model_client_retries_transient_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_reply()))
            .mount(&server)
            .await;

        let client = ChatClient::new(&format!("{}/v1", server.uri()), "key", "m")
            .with_retry_policy(fast_policy());
        let messages = [ChatMessage::user("hi")];
        let extra = serde_json::Map::new();
        let turn = ModelClient::complete(&client, engine_request(&messages, &[], &extra))
            .await
            .expect("recovers after a transient 503");
        assert_eq!(turn.content.as_deref(), Some("ok"));
    }
}
