//! Control-plane file configuration (RFC-0001 + ADR-0021).
//!
//! Like the agent runner, the control plane reads an optional JSON config file (mounted from a Helm
//! ConfigMap) with `{env:VAR:-default}` substitution, instead of a sprawl of env vars. **File when
//! present, else env** — an absent file means today's env vars + defaults still apply, so prod keeps
//! running until the ConfigMap is mounted.
//!
//! Scope: the agent-Job knobs the dispatcher stamps into each Job (namespace, image, deadline, the
//! agent ConfigMap to mount, …) and the dispatcher loop timings. Each field is optional and falls
//! back to its prior env/default individually.

use std::path::Path;

use serde::Deserialize;

/// Where the control plane looks for its config file; overridable via `CONTROL_PLANE_CONFIG`.
const DEFAULT_CONFIG_PATH: &str = "/etc/lightbridge/control-plane.json";

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    pub agent: AgentSection,
    pub dispatcher: DispatcherSection,
    pub review: ReviewSection,
    pub embeddings: EmbeddingsSection,
    pub knowledge_tools: KnowledgeToolsSection,
}

/// External-knowledge MCP servers (ADR-0066) the review agent can dynamically discover and call as
/// the `mcp_tools` mediated tool, on **either** tier — availability is governed purely by the
/// normal per-tier `review.<tier>.tools` allowlist ([`crate::db`] doesn't gate this; there is no
/// tier check here or in the internal handlers). Each entry is an already-deployed, in-cluster MCP
/// server (e.g. `converse-mcp` namespace) that holds its own upstream provider credentials — the
/// control plane only needs its in-cluster Service URL, reached over plain in-cluster HTTP (no
/// OAuth, no secrets held here). Empty by default: no servers configured means no tools discovered,
/// a safe degrade rather than an error. Adding a new server (any MCP server, not just
/// brave-search/context7) is a config change, not a code change.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KnowledgeToolsSection {
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
}

/// One configured MCP server. Its tools are exposed to the agent prefixed `mcp__<name>__<tool>` so
/// names can't collide across servers and the control plane can route a call back without a
/// separate lookup table.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// Short, unique identifier, e.g. `brave-search`, `context7`. Must not contain `__` (would break
    /// the `mcp__<name>__<tool>` prefix's unambiguous split) — enforced at deserialize, so a
    /// misconfigured name fails config load loud rather than silently misrouting calls at runtime.
    #[serde(deserialize_with = "deserialize_mcp_server_name")]
    pub name: String,
    /// Streamable-HTTP MCP endpoint, e.g.
    /// `http://brave-search.converse-mcp.svc.cluster.local:8080/mcp`.
    pub url: String,
}

/// Reject a server `name` containing `__`: `parse_knowledge_tool_name`
/// (`services/control-plane/src/http/internal.rs`) splits `mcp__<name>__<tool>` on the FIRST `__`
/// after the prefix, so a name with its own `__` would silently misroute every call to that server.
fn deserialize_mcp_server_name<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let name = String::deserialize(deserializer)?;
    if name.contains("__") {
        return Err(serde::de::Error::custom(format!(
            "mcp server name {name:?} must not contain \"__\" — it would break the \
             mcp__<name>__<tool> prefix's unambiguous split"
        )));
    }
    Ok(name)
}

/// Embedding-store safety. The `code_chunks.embedding` column is a fixed-width `vector(N)`; changing
/// the embedding model to a different dimension is **destructive** (every stored vector is the wrong
/// width). When `dimension` is set and differs from the live column, the control plane wipes
/// `code_chunks` and migrates the column — but **only if `allow_reindex_on_dim_change`** is true;
/// otherwise it fails loud (refuses to start) so a typo can't silently destroy the index.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmbeddingsSection {
    #[serde(default, deserialize_with = "lightbridge_config::de::opt_i64")]
    pub dimension: Option<i64>,
    pub allow_reindex_on_dim_change: bool,
}

/// Review-feedback knobs the control plane applies to the PR: lifecycle reactions and outcome labels.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReviewSection {
    /// React to the PR description across the review lifecycle (👀 started → 🎉 done / 😕 errored).
    /// Defaults to enabled when unset.
    pub reactions: Option<bool>,
    /// Label added whenever a review is posted (e.g. `lightbridge-reviewed`). `None` → not added.
    pub label_reviewed: Option<String>,
    /// Label added when the review has ≥1 finding of any severity (e.g. `needs-review`).
    pub label_findings: Option<String>,
    /// Label added when the review has ≥1 `error`-severity finding (e.g. `bug`).
    pub label_error: Option<String>,
    /// Skip the automatic fast-tier review when the PR author is a bot (RFC-0003). The `@mention`
    /// deep-review path is unaffected. Defaults to enabled when unset.
    pub skip_bot_authored_prs: Option<bool>,
}

impl ReviewSection {
    /// Reactions are on unless explicitly disabled.
    pub fn reactions_enabled(&self) -> bool {
        self.reactions.unwrap_or(true)
    }

    /// Bot-authored PRs skip the automatic fast-tier review unless explicitly disabled (RFC-0003).
    pub fn skip_bot_authored_prs(&self) -> bool {
        self.skip_bot_authored_prs.unwrap_or(true)
    }
}

/// Knobs for the per-task agent Job the dispatcher launches (mirrors `KubeLauncher`).
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentSection {
    pub namespace: Option<String>,
    /// The agent-runner container image. Acts as the shared fallback when a per-kind override below
    /// is unset.
    pub runner_image: Option<String>,
    /// Per-kind override for **index** Jobs (`command_text == "index"`) — the full image that
    /// bundles Python + Graphify for structural-graph extraction. Falls back to `runner_image`.
    pub indexer_runner_image: Option<String>,
    /// Per-kind override for **review** Jobs (everything else) — the leaner image without the
    /// Python/Graphify venv (the review path never spawns Graphify). Falls back to `runner_image`.
    pub review_runner_image: Option<String>,
    pub service_account: Option<String>,
    pub control_plane_url: Option<String>,
    /// Secret holding the internal CA (`ca.crt`) to mount into the Job.
    pub ca_secret: Option<String>,
    /// ConfigMap (in the agents namespace) holding the runner's `agent.json` + prompt templates, to
    /// mount at `/etc/lightbridge` so the runner reads its file config. `None` → not mounted.
    pub config_configmap: Option<String>,
    /// The Job's `activeDeadlineSeconds` runtime cap.
    #[serde(default, deserialize_with = "lightbridge_config::de::opt_i64")]
    pub job_deadline_seconds: Option<i64>,
    /// Legacy passthrough: inline reviewer prompt injected as `REVIEW_SYSTEM_PROMPT`. Prefer mounting
    /// the template via `config_configmap` instead.
    pub review_system_prompt: Option<String>,
    /// The runner container's k8s `resources` block (requests/limits), passed through verbatim into
    /// the Job's container spec. A raw object so operators can express any valid shape (and use
    /// `{env:…}` inside). `None` → no resources set (cluster defaults / LimitRange apply). Acts as the
    /// shared fallback when a per-kind override below is unset.
    pub resources: Option<serde_json::Value>,
    /// Per-kind override for **index** Jobs (`command_text == "index"`) — the heavy path (full
    /// tree-sitter parse + embeddings + Graphify), wants more CPU/RAM. Falls back to `resources`.
    pub indexer_resources: Option<serde_json::Value>,
    /// Per-kind override for **review** Jobs (everything else) — read-mostly (reuses the indexed
    /// snapshot, ADR-0050; LLM/network-bound), so it can run leaner. Falls back to `resources`.
    pub review_resources: Option<serde_json::Value>,
}

/// Dispatcher loop timings (seconds). Each falls back to its built-in default in `dispatcher.rs`.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DispatcherSection {
    #[serde(default, deserialize_with = "lightbridge_config::de::opt_u64")]
    pub claim_lease_seconds: Option<u64>,
    #[serde(default, deserialize_with = "lightbridge_config::de::opt_u64")]
    pub poll_fallback_seconds: Option<u64>,
    #[serde(default, deserialize_with = "lightbridge_config::de::opt_u64")]
    pub launch_backoff_seconds: Option<u64>,
    #[serde(default, deserialize_with = "lightbridge_config::de::opt_u64")]
    pub reap_interval_seconds: Option<u64>,
    /// How often the index sweeper prunes stale `(repo, commit)` snapshots (ADR-0052). The outbox
    /// sweeper (ADR-0059) shares this same GC tick.
    #[serde(default, deserialize_with = "lightbridge_config::de::opt_u64")]
    pub prune_interval_seconds: Option<u64>,
    /// How often the durable data-purge backstop re-checks for `disabled` repos with leftover index
    /// data (Epic #75). A rare recovery net (the prompt path is the spawned purge on disable/deny), so
    /// it runs on its own slow tick rather than the ~30s reaper cadence. Default 600.
    #[serde(default, deserialize_with = "lightbridge_config::de::opt_u64")]
    pub purge_reconcile_interval_seconds: Option<u64>,
    /// Days a delivered (`posted`) `github_outbox` row is kept before the outbox sweeper prunes it
    /// (ADR-0059). Default 7.
    #[serde(default, deserialize_with = "lightbridge_config::de::opt_i64")]
    pub outbox_posted_retention_days: Option<i64>,
    /// Days a dead-lettered (`failed`) `github_outbox` row is kept — longer, for inspection — before
    /// pruning (ADR-0059). Default 30.
    #[serde(default, deserialize_with = "lightbridge_config::de::opt_i64")]
    pub outbox_failed_retention_days: Option<i64>,
}

/// Load the control-plane config file if it exists. `Ok(None)` when absent (use env); `Err` when it
/// exists but is malformed.
pub fn load_file_config() -> anyhow::Result<Option<FileConfig>> {
    let path =
        std::env::var("CONTROL_PLANE_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_string());
    let path = Path::new(&path);
    if !path.exists() {
        return Ok(None);
    }
    lightbridge_config::load::<FileConfig>(path).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_server_name_with_double_underscore_fails_at_deserialize() {
        let json =
            r#"{"knowledge_tools":{"mcp_servers":[{"name":"context-7__eu","url":"http://x"}]}}"#;
        let err = serde_json::from_str::<FileConfig>(json)
            .expect_err("a `__`-containing server name must fail parsing");
        assert!(
            err.to_string().contains("__"),
            "error names the problem: {err}"
        );
    }

    #[test]
    fn mcp_server_name_without_double_underscore_parses() {
        let json = r#"{"knowledge_tools":{"mcp_servers":[{"name":"context7","url":"http://x"}]}}"#;
        let config: FileConfig = serde_json::from_str(json).expect("valid name parses");
        assert_eq!(config.knowledge_tools.mcp_servers[0].name, "context7");
    }
}
