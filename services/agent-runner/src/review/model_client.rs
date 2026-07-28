//! Building the review LLM's [`ChatClient`] (ADR-0039) and the one-line "review agent starting" log
//! that accompanies it.

use std::time::Duration;

use lci_review_agent::model::{ChatClient, RetryPolicy};
use uuid::Uuid;

use crate::bootstrap::config::ReviewConfig;

/// Build the model client for one run. Streaming (ADR-0039 / #206) is opt-in via `review.stream`.
/// `with_extra` strips reserved structural keys; the sanitized map is carried into the conversation's
/// `RequestOptions` by the caller (the engine flattens the request `extra` from there).
/// `with_retry_policy` preserves the per-turn retry the legacy `complete_with_retry` applied before the
/// loop's circuit breaker sees a transient failure.
///
/// Also emits the run's "starting" telemetry log line — grouped here because it's the same handful of
/// resilience/model fields the client itself was just built from.
pub(crate) fn build_chat_client(
    review: &ReviewConfig,
    attribution: &[(String, String)],
    task_id: Uuid,
) -> ChatClient {
    let chat = ChatClient::with_timeout(
        &review.base_url,
        &review.api_key,
        &review.model,
        Duration::from_secs(review.resilience.request_timeout_secs),
    )
    .with_attribution(attribution)
    .with_extra(review.extra.clone())
    .with_stream(review.stream)
    .with_retry_policy(RetryPolicy {
        max_retries: review.resilience.max_retries,
        ..RetryPolicy::default()
    });

    tracing::info!(
        task_id = %task_id,
        model = %review.model,
        base_url_host = %base_url_host(&review.base_url),
        request_timeout_secs = review.resilience.request_timeout_secs,
        max_retries = review.resilience.max_retries,
        circuit_breaker_threshold = review.resilience.circuit_breaker_threshold,
        stream = review.stream,
        extra = %serde_json::Value::Object(review.extra.clone()),
        "review agent starting"
    );

    chat
}

/// Host of a base URL for logging (never the path/key). Falls back to the whole string when there's no
/// scheme separator, so a schemeless URL still logs its host rather than "(unparseable)".
fn base_url_host(base_url: &str) -> String {
    let without_scheme = base_url.split("://").nth(1).unwrap_or(base_url);
    without_scheme
        .split(['/', '?', '#'])
        .next()
        .map(|hostport| hostport.to_string())
        .unwrap_or_else(|| "(unparseable)".to_string())
}
