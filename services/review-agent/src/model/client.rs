//! [`ChatClient`]: the OpenAI-compatible Chat Completions transport (ADR-0026). Owns the client
//! config (gateway URL/key/model, attribution, passthrough `extra`, retry policy, telemetry
//! side-channel) and the request lifecycle — building the wire request, sending it with retry
//! (ADR-0039), and parsing a buffered (non-stream) reply. The streaming reply path lives in
//! [`super::stream`]; the retry/error classification in [`super::retry`]; the raw non-stream response
//! DTOs in [`super::wire`].

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;

use lci_agent_clients::ratelimit::{self, RateLimitSnapshot};
use lci_agent_loop::{ChatMessage, ChatRequest, ModelClient, StreamOptions};
use lci_agent_types::{AssistantTurn, StepError, ToolDef};

use super::completion::{ChatParams, Completion, TurnTelemetry};
use super::http::{build_http_client, truncate_on_boundary};
use super::retry::{ChatError, RetryPolicy, chat_error_to_step};
use super::wire::ChatResponse;

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
    /// Inter-chunk idle timeout used on the streaming path (`super::stream`) — the max silence between
    /// SSE chunks before the turn is treated as stalled. Seeded from the per-request timeout.
    pub(super) idle_timeout: Duration,
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

impl ChatClient {
    /// `base_url` is the LLM gateway base **including** the `/v1` segment (`LLM_BASE_URL`); the model
    /// is the chat model id (`LLM_MODEL`). Uses the default per-request timeout
    /// ([`super::DEFAULT_REQUEST_TIMEOUT_SECS`]).
    pub fn new(base_url: &str, api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::with_timeout(
            base_url,
            api_key,
            model,
            Duration::from_secs(super::DEFAULT_REQUEST_TIMEOUT_SECS),
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

#[cfg(test)]
mod tests;
