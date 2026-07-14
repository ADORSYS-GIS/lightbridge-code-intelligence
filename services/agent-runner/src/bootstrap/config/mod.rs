//! Runner configuration, read from the environment the dispatcher's Job manifest injects (see
//! `control-plane/src/k8s.rs`). Only the wiring the runner needs to *find* and *authenticate to* the
//! control plane lives here; the actual task context (repo, SHAs, command) is fetched from the
//! control plane at runtime rather than trusted from env, so the env stays minimal.
//!
//! Split by concern (quality pass, no behaviour change): [`defaults`] (the tunable-default
//! constants), [`file`] (the JSON ConfigMap shape + its `Deserialize` impls), [`env`] (small env-var
//! parsing helpers shared across the resolvers below), [`runner`] ([`RunnerConfig`]), [`embeddings`]
//! ([`EmbeddingsConfig`]), [`review`] (the review LLM config + two-tier resolution — the biggest
//! piece), [`redact`] (the audit-trail redaction pass, split out because it's a security-sensitive
//! concern in its own right), and [`sast`] ([`SastConfig`]). Every type that used to live in this one
//! file is re-exported here, so callers keep using `bootstrap::config::Whatever` unchanged. SAST's
//! config *value type* ([`SastConfig`]) now lives in `lci-agent-sast` (ADR-0073); [`sast`] here only
//! resolves it from the file config / environment.

mod defaults;
mod embeddings;
mod env;
mod file;
mod redact;
mod review;
mod runner;
mod sast;

pub use defaults::*;
pub use embeddings::EmbeddingsConfig;
pub use file::{
    EmbeddingsFile, EmbeddingsTuningFile, FileConfig, McpToolPattern, ReviewFile, ReviewTool,
    ReviewToolSelector, SastFile, load_file_config,
};
pub use lci_agent_sast::SastConfig;
pub use redact::REDACTED;
pub use review::{ResilienceConfig, ReviewConfig, ReviewConfigs};
pub use runner::RunnerConfig;
pub(crate) use sast::resolve_sast_config;
