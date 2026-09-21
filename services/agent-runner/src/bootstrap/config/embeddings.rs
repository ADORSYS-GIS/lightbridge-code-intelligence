//! [`EmbeddingsConfig`] — connection + tuning for the OpenAI-compatible embeddings API (ADR-0018).

use lci_agent_clients::DEFAULT_MAX_INPUT_BYTES;

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
    /// Ceiling on the bytes of one input string sent to the model. From
    /// `EMBEDDINGS_MAX_INPUT_BYTES`, else [`DEFAULT_MAX_INPUT_BYTES`]. Chunk shape is bounded in
    /// lines, so a slice of long unwrapped lines can be far larger than its line count suggests;
    /// this keeps a request inside what the model accepts.
    pub max_input_bytes: usize,
}

impl EmbeddingsConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            base_url: require("EMBEDDINGS_BASE_URL")?,
            api_key: require("EMBEDDINGS_API_KEY")?,
            model: require("EMBEDDINGS_MODEL")?,
            request_timeout_secs: parse_env_u64("EMBEDDINGS_REQUEST_TIMEOUT_SECS")
                .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS),
            max_input_bytes: max_input_bytes_from_env(),
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
                max_input_bytes: max_input_bytes_from_env(),
            }),
            None => Self::from_env(),
        }
    }
}

/// Read `EMBEDDINGS_MAX_INPUT_BYTES`, falling back to [`DEFAULT_MAX_INPUT_BYTES`] when unset or
/// unparseable. The client clamps to ≥1, so a `0` here cannot empty every input.
fn max_input_bytes_from_env() -> usize {
    parse_env_u64("EMBEDDINGS_MAX_INPUT_BYTES")
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(DEFAULT_MAX_INPUT_BYTES)
}
