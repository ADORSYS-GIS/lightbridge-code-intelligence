//! OpenAI-compatible embeddings client (ADR-0018). Talks to `POST {base}/v1/embeddings`; in prod
//! the endpoint is the eaig/core-gateway (Envoy AI Gateway), same base URL LibreChat's RAG uses.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ratelimit::{self, RateLimitSnapshot};

/// Reactive retry bounds for embeddings, mirroring the chat client's defaults (ADR-0039). Retries fire
/// **only** on transient failures (connect/timeout, HTTP 429, HTTP 5xx); a 4xx other than 429 is
/// deterministic and never retried. The indexer already batches conservatively to stay under the
/// gateway's token-per-minute budget — this is the safety net for the bursts that still slip through.
const MAX_RETRIES: u32 = 3;
const BASE_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(8);

#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: &'a [&'a str],
}

#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedData>,
}

#[derive(Deserialize)]
struct EmbedData {
    embedding: Vec<f32>,
    index: usize,
}

pub struct EmbeddingsClient {
    url: String,
    api_key: String,
    model: String,
    http: reqwest::Client,
    /// Attribution headers (epic #89) added to every request so the gateway can bill the right
    /// project. Empty by default; set via [`EmbeddingsClient::with_attribution`].
    attribution: reqwest::header::HeaderMap,
}

/// Build the HTTP client, additionally trusting the CA PEM at `EMBEDDINGS_CA_CERT` if set. The eaig
/// gateway's internal HTTPS endpoint is signed by a private CA (`ClusterIssuer/self-signed-ca`) that
/// the default rustls/webpki roots don't include; the Job mounts that CA and points the env at it.
/// Absent the env, the default client (public roots) is used — fine for plain-HTTP / public-cert
/// endpoints. `add_root_certificate` augments the default roots, it doesn't replace them.
fn build_http_client(timeout: Option<std::time::Duration>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder();
    if let Some(t) = timeout {
        builder = builder.timeout(t);
    }
    if let Ok(path) = std::env::var("EMBEDDINGS_CA_CERT") {
        match load_ca(&path) {
            Ok(cert) => {
                builder = builder.add_root_certificate(cert);
                tracing::info!(%path, "embeddings: trusting extra CA");
            }
            Err(error) => {
                tracing::warn!(%error, %path, "embeddings: could not load EMBEDDINGS_CA_CERT; using default roots");
            }
        }
    }
    builder
        .build()
        .expect("building the embeddings HTTP client with default roots cannot fail")
}

fn load_ca(path: &str) -> anyhow::Result<reqwest::Certificate> {
    let pem = std::fs::read(path)?;
    Ok(reqwest::Certificate::from_pem(&pem)?)
}

impl EmbeddingsClient {
    pub fn new(base_url: &str, api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            url: format!("{base_url}/v1/embeddings"),
            api_key: api_key.into(),
            model: model.into(),
            http: build_http_client(None),
            attribution: reqwest::header::HeaderMap::new(),
        }
    }

    /// Apply a per-request timeout (ADR-0051; from `embeddings.config.request_timeout_secs`). Rebuilds
    /// the transport with the timeout set, preserving the CA-trust behaviour.
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.http = build_http_client(Some(timeout));
        self
    }

    /// Attach gateway attribution headers (epic #89). Invalid header names/values are skipped (these
    /// are our own controlled keys, so that shouldn't happen).
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

    /// Embed a batch of texts. Returns one vector per input, in the same order.
    ///
    /// The OpenAI spec does not guarantee response order matches input order, so we sort by the
    /// returned `index` field before returning.
    ///
    /// Retries transient failures (connect/timeout, HTTP 429, HTTP 5xx) up to [`MAX_RETRIES`] times
    /// with exponential backoff, honouring a 429's `Retry-After` over the computed backoff (ADR-0039) —
    /// the same reactive policy the chat client uses. A deterministic 4xx (bad request, auth, unknown
    /// model) fails immediately.
    pub async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut attempt = 0u32;
        loop {
            match self.embed_once(texts).await {
                Ok(vectors) => return Ok(vectors),
                Err(err) => {
                    if !err.transient || attempt >= MAX_RETRIES {
                        return Err(err.error);
                    }
                    let wait = err
                        .retry_after
                        .map(|d| d.min(MAX_BACKOFF))
                        .unwrap_or_else(|| backoff(attempt));
                    tracing::warn!(
                        model = %self.model,
                        attempt = attempt + 1,
                        max_retries = MAX_RETRIES,
                        backoff_ms = wait.as_millis() as u64,
                        retry_after = err.retry_after.is_some(),
                        error = %err.error,
                        "transient embeddings failure; retrying after backoff"
                    );
                    tokio::time::sleep(wait).await;
                    attempt += 1;
                }
            }
        }
    }

    /// One embeddings request, returning a classified [`EmbedError`] on failure so [`embed`](Self::embed)
    /// can decide whether to retry. Also reads the gateway's rate-limit budget off the response headers
    /// and soft-warns when it's nearly spent or this response was itself rate-limited (advisory only;
    /// the budget is shared across runners, see [`crate::ratelimit`]).
    async fn embed_once(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let response = self
            .http
            .post(&self.url)
            .bearer_auth(&self.api_key)
            .headers(self.attribution.clone())
            .json(&EmbedRequest {
                model: &self.model,
                input: texts,
            })
            .send()
            .await
            .map_err(|e| {
                // Only connect/timeout transport errors are worth a retry; a request-construction error
                // is deterministic and would fail identically every attempt.
                let transient = e.is_timeout() || e.is_connect();
                EmbedError {
                    error: anyhow::Error::new(e).context("embeddings request failed"),
                    transient,
                    retry_after: None,
                }
            })?;

        let status = response.status();
        let rate_limit = RateLimitSnapshot::from_headers(response.headers());
        if rate_limit.limited || rate_limit.is_low(0.1) {
            tracing::warn!(
                model = %self.model,
                ratelimit_remaining = rate_limit.remaining.map(|r| r as i64).unwrap_or(-1),
                ratelimit_limit = rate_limit.limit.map(|l| l as i64).unwrap_or(-1),
                reset_secs = rate_limit.reset.map(|d| d.as_secs() as i64).unwrap_or(-1),
                limited = rate_limit.limited,
                "gateway rate-limit budget low (embeddings)"
            );
        }

        if !status.is_success() {
            // 429 + 5xx are transient; other 4xx are deterministic. Read the body (bounded) so the
            // failure is legible instead of a bare status code.
            let retry_after = ratelimit::retry_after(response.headers());
            let body = response.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(512).collect();
            let transient =
                status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
            return Err(EmbedError {
                error: anyhow::anyhow!(
                    "embeddings API returned {status}: {}",
                    if snippet.is_empty() {
                        "(empty body)"
                    } else {
                        &snippet
                    }
                ),
                transient,
                retry_after: retry_after.filter(|_| transient),
            });
        }

        let mut data = response
            .json::<EmbedResponse>()
            .await
            .map_err(|e| EmbedError {
                // A malformed 2xx body is not a transport problem — don't retry it.
                error: anyhow::Error::new(e).context("parsing embeddings response"),
                transient: false,
                retry_after: None,
            })?
            .data;
        data.sort_by_key(|d| d.index);
        if data.len() != texts.len() {
            return Err(EmbedError {
                error: anyhow::anyhow!(
                    "embeddings API returned {} vectors for {} inputs",
                    data.len(),
                    texts.len()
                ),
                transient: false,
                retry_after: None,
            });
        }
        Ok(data.into_iter().map(|d| d.embedding).collect())
    }
}

/// Why an embeddings request failed, so [`EmbeddingsClient::embed`] can decide whether to retry.
struct EmbedError {
    error: anyhow::Error,
    /// `true` for connect/timeout, HTTP 429, or HTTP 5xx — the only failures worth a retry.
    transient: bool,
    /// `Retry-After` seconds parsed off a 429, when present — honoured over the computed backoff.
    retry_after: Option<Duration>,
}

/// Backoff for `attempt` (0 = the wait before the first retry): exponential off [`BASE_BACKOFF`],
/// capped at [`MAX_BACKOFF`], plus small deterministic jitter (no clock/RNG, so tests are stable).
/// Mirrors the chat client's `RetryPolicy::backoff`.
fn backoff(attempt: u32) -> Duration {
    let factor = 1u64 << attempt.min(16);
    let base = BASE_BACKOFF.saturating_mul(factor.min(u32::MAX as u64) as u32);
    let capped = base.min(MAX_BACKOFF);
    let jitter_ms = (attempt as u64).wrapping_mul(2_654_435_761) % 250;
    capped.saturating_add(Duration::from_millis(jitter_ms))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{bearer_token, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn embed_sends_bearer_and_returns_ordered_vectors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .and(bearer_token("key"))
            .and(header("x-code-intelligence-task-id", "task-7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [
                    {"index": 1, "embedding": [0.0_f32, 1.0_f32], "object": "embedding"},
                    {"index": 0, "embedding": [1.0_f32, 0.0_f32], "object": "embedding"},
                ],
                "model": "test-model"
            })))
            .mount(&server)
            .await;

        let client = EmbeddingsClient::new(&server.uri(), "key", "test-model")
            .with_timeout(Duration::from_secs(2))
            .with_attribution(&[
                (
                    "x-code-intelligence-task-id".to_string(),
                    "task-7".to_string(),
                ),
                ("invalid header".to_string(), "ignored".to_string()),
            ]);
        let vecs = client.embed(&["hello", "world"]).await.expect("embed");
        assert_eq!(vecs.len(), 2);
        // Index 0 should come first (sorted by `index` field).
        assert_eq!(vecs[0], vec![1.0_f32, 0.0_f32]);
        assert_eq!(vecs[1], vec![0.0_f32, 1.0_f32]);
    }

    #[tokio::test]
    async fn embed_empty_slice_returns_empty_vec() {
        // No HTTP call is made for an empty input.
        let client = EmbeddingsClient::new("http://unused", "key", "model");
        let vecs = client.embed(&[]).await.expect("embed empty");
        assert!(vecs.is_empty());
    }

    // A 429 is transient → retried, honouring `Retry-After` (here `0`, so the test doesn't sleep).
    // Once the gateway recovers the batch succeeds.
    #[tokio::test]
    async fn embed_retries_on_429_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{"index": 0, "embedding": [1.0_f32], "object": "embedding"}],
                "model": "test-model"
            })))
            .mount(&server)
            .await;

        let client = EmbeddingsClient::new(&server.uri(), "key", "test-model");
        let vecs = client.embed(&["hello"]).await.expect("recovers after 429");
        assert_eq!(vecs, vec![vec![1.0_f32]]);
    }

    // A 400 is deterministic → NOT retried (exactly one request) and the body surfaces.
    #[tokio::test]
    async fn embed_does_not_retry_on_400_and_surfaces_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(400).set_body_string("unknown model 'test-model'"))
            .mount(&server)
            .await;

        let client = EmbeddingsClient::new(&server.uri(), "key", "test-model");
        let err = client
            .embed(&["hi"])
            .await
            .expect_err("400 is deterministic");
        let msg = format!("{err:#}");
        assert!(msg.contains("returned 400"), "status surfaced: {msg}");
        assert!(msg.contains("unknown model"), "body surfaced: {msg}");

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1, "400 must not be retried");
    }

    #[tokio::test]
    async fn embed_retries_on_500_with_computed_backoff() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(500).set_body_string("gateway unavailable"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"index": 0, "embedding": [0.5_f32]}]
            })))
            .mount(&server)
            .await;

        let vectors = EmbeddingsClient::new(&server.uri(), "key", "model")
            .embed(&["recover"])
            .await
            .expect("recovers after 500");
        assert_eq!(vectors, vec![vec![0.5]]);
    }

    #[tokio::test]
    async fn embed_rejects_malformed_and_wrong_sized_successes() {
        let malformed = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not-json"))
            .mount(&malformed)
            .await;
        let error = EmbeddingsClient::new(&malformed.uri(), "key", "model")
            .embed(&["one"])
            .await
            .expect_err("malformed 2xx must fail");
        assert!(format!("{error:#}").contains("parsing embeddings response"));

        let wrong_size = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": []
            })))
            .mount(&wrong_size)
            .await;
        let error = EmbeddingsClient::new(&wrong_size.uri(), "key", "model")
            .embed(&["one"])
            .await
            .expect_err("wrong vector count must fail");
        assert!(format!("{error:#}").contains("returned 0 vectors for 1 inputs"));
    }
}
