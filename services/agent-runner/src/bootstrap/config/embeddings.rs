//! [`EmbeddingsConfig`] — connection + tuning for the OpenAI-compatible embeddings API (ADR-0018).

use super::defaults::DEFAULT_REQUEST_TIMEOUT_SECS;
use super::env::{parse_env_u64, require, require_field};
use super::file::FileConfig;

/// Configuration for the OpenAI-compatible embeddings API (ADR-0018). All three fields are
/// required — no default model, so a misconfigured Job fails loudly with a named variable.
#[derive(Debug, Clone)]
pub struct EmbeddingsConfig {
    /// Base URL of the OpenAI-compatible endpoint (no trailing `/v1`).
    /// Prod: `https://core-gateway-internal.envoy-gateway-system.svc.cluster.local`
    pub base_url: String,
    /// API key presented as `Authorization: Bearer`. Prod key: `converse_openai_api_key`.
    pub api_key: String,
    /// Model identifier, e.g. `text-embedding-3-small`. The schema expects 1536-dim vectors
    /// matching that model; choosing a different-dimension model requires a migration (ADR-0018).
    pub model: String,
    /// Per-request timeout (seconds) for one embeddings call (ADR-0051). From `embeddings.config
    /// .request_timeout_secs` / `EMBEDDINGS_REQUEST_TIMEOUT_SECS`, else [`DEFAULT_REQUEST_TIMEOUT_SECS`].
    pub request_timeout_secs: u64,
}

impl EmbeddingsConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            base_url: require("EMBEDDINGS_BASE_URL")?,
            api_key: require("EMBEDDINGS_API_KEY")?,
            model: require("EMBEDDINGS_MODEL")?,
            request_timeout_secs: parse_env_u64("EMBEDDINGS_REQUEST_TIMEOUT_SECS")
                .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS),
        })
    }

    /// Resolve from the file config when it carries an `embeddings` block, else from env. The three
    /// connection fields are required either way (no default model — a misconfig fails loud); the
    /// `config` block is optional.
    pub fn resolve(file: Option<&FileConfig>) -> anyhow::Result<Self> {
        match file.and_then(|f| f.embeddings.as_ref()) {
            Some(e) => Ok(Self {
                base_url: require_field("embeddings", "base_url", &e.base_url)?,
                api_key: require_field("embeddings", "api_key", &e.api_key)?,
                model: require_field("embeddings", "model", &e.model)?,
                request_timeout_secs: e
                    .config
                    .as_ref()
                    .and_then(|c| c.request_timeout_secs)
                    .or_else(|| parse_env_u64("EMBEDDINGS_REQUEST_TIMEOUT_SECS"))
                    .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS),
            }),
            None => Self::from_env(),
        }
    }
}
