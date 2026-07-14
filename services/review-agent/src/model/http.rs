//! Transport-level helpers shared by the non-stream and streaming request paths: building the
//! `reqwest::Client` (with the internal CA trust, ADR-0039) and bounding an error-body snippet.

use std::time::Duration;

/// Build the HTTP client, additionally trusting the internal CA PEM at `LLM_CA_CERT` (falling back to
/// `EMBEDDINGS_CA_CERT`, since the chat and embeddings endpoints share the eaig gateway and its
/// private CA — `ClusterIssuer/self-signed-ca`, which the default rustls/webpki roots don't include).
/// Absent both, the default client (public roots) is used. `add_root_certificate` augments the default
/// roots, it doesn't replace them.
pub(super) fn build_http_client(total_timeout: Option<Duration>) -> reqwest::Client {
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

/// `s` truncated to at most `max` bytes, never slicing through a multi-byte char (mirrors the helper
/// in the agent loop; kept local so the transport layer has no cross-module dep).
pub(super) fn truncate_on_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
