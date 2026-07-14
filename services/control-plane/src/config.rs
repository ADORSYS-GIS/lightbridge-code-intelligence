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
    pub egress: EgressSection,
    pub gitlab: GitlabSection,
}

/// GitLab projects configured from the mounted control-plane file only. There is intentionally no
/// `GITLAB_*` env fallback: each project carries its own API token, webhook secret, API URL, and
/// optional bot handle so multi-project routing is explicit and fail-closed.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitlabSection {
    /// Explicit opt-in. When true, at least one valid project must be configured.
    pub enabled: bool,
    /// Shared default for projects that omit `api_url`.
    pub default_api_url: Option<String>,
    /// Shared default for projects that omit `bot_handle`.
    pub default_bot_handle: Option<String>,
    #[serde(default)]
    pub projects: Vec<GitlabProjectConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitlabProjectConfig {
    /// Numeric GitLab project id; also used as `tasks.installation_id` for GitLab.
    pub project_id: i64,
    /// Optional per-project API URL, e.g. `https://gitlab.example.com/api/v4`.
    pub api_url: Option<String>,
    /// Project/group/PAT token sent as `PRIVATE-TOKEN`.
    pub access_token: String,
    /// Plain token expected in `X-Gitlab-Token` for this project.
    pub webhook_secret: String,
    /// Optional per-project bot handle for @mention deep-review requests.
    pub bot_handle: Option<String>,
}

impl GitlabSection {
    pub fn default_api_url(&self) -> &str {
        self.default_api_url
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("https://gitlab.com/api/v4")
    }

    pub fn default_bot_handle(&self) -> &str {
        self.default_bot_handle
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("lightbridge-bot")
    }

    pub fn resolved_api_url<'a>(&'a self, project: &'a GitlabProjectConfig) -> &'a str {
        project
            .api_url
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| self.default_api_url())
    }

    pub fn resolved_bot_handle<'a>(&'a self, project: &'a GitlabProjectConfig) -> &'a str {
        project
            .bot_handle
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| self.default_bot_handle())
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.projects.is_empty() {
            anyhow::bail!("gitlab.enabled=true requires at least one gitlab.projects[] entry");
        }

        let mut seen = std::collections::HashSet::new();
        for project in &self.projects {
            if !seen.insert(project.project_id) {
                anyhow::bail!("duplicate GitLab project_id {}", project.project_id);
            }
            if project.access_token.trim().is_empty() {
                anyhow::bail!("GitLab project {} has empty access_token", project.project_id);
            }
            if project.webhook_secret.trim().is_empty() {
                anyhow::bail!("GitLab project {} has empty webhook_secret", project.project_id);
            }
            if reqwest::header::HeaderValue::from_str(&project.access_token).is_err() {
                anyhow::bail!(
                    "GitLab project {} access_token is not a valid header value",
                    project.project_id
                );
            }
            if reqwest::header::HeaderValue::from_str(&project.webhook_secret).is_err() {
                anyhow::bail!(
                    "GitLab project {} webhook_secret is not a valid header value",
                    project.project_id
                );
            }
        }

        Ok(())
    }
}

/// Which path delivers `outbox` intents (RFC-0005 Phase A / ADR-0074). **Defaults to `Drain`** — the
/// pre-existing reconciler drain (ADR-0059) stays the active egress path, so merging the pilot changes
/// no production behavior. Flip to `Restate` to route egress through the `PlatformEgress` virtual object
/// (the pilot). The mode is read once at startup — it is deploy-scoped, not live-reloadable (a flip
/// takes effect on the next boot, like every other config knob). Both modes share the `outbox` table as
/// the ledger, so switching direction **across a deploy** is safe: any row not yet `posted` is picked up
/// by whichever consumer the new deploy runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EgressMode {
    /// The reconciler's `outbox` drain posts every intent (today's behavior). Default.
    #[default]
    Drain,
    /// Producers additionally `send` `PlatformEgress::post(outbox_id)` to Restate, which posts each
    /// intent through the per-`platform:installation` virtual object; the reconciler drain is disabled.
    Restate,
}

/// Egress routing (RFC-0005 Phase A / ADR-0074). Absent → `Drain` (no behavior change). When `mode =
/// restate`, `restate_ingress_url` must resolve (config or the `RESTATE_INGRESS_URL` env fallback) — the
/// base URL of the Restate server's HTTP ingress the producer `send`s invocations to.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EgressSection {
    /// `drain` (default) | `restate`. See [`EgressMode`].
    pub mode: EgressMode,
    /// Base URL of the Restate ingress (e.g. `http://restate.converse.svc.cluster.local:8080`). Only
    /// read in `restate` mode; falls back to the `RESTATE_INGRESS_URL` env var when unset.
    pub restate_ingress_url: Option<String>,
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
    #[serde(default, deserialize_with = "lci_config::de::opt_i64")]
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
    /// Per-kind override for **index** Jobs (`command_text == "index"`). Falls back to `runner_image`.
    /// Since Graphify was retired (ADR-0086) the index and review images share one lean runtime, but
    /// the override stays so operators can still pin the two Job kinds to different tags if needed.
    pub indexer_runner_image: Option<String>,
    /// Per-kind override for **review** Jobs (everything else). Falls back to `runner_image`. Same
    /// lean runtime as the index image now that Graphify is gone (ADR-0086); override kept for pinning.
    pub review_runner_image: Option<String>,
    pub service_account: Option<String>,
    pub control_plane_url: Option<String>,
    /// Secret holding the internal CA (`ca.crt`) to mount into the Job.
    pub ca_secret: Option<String>,
    /// ConfigMap (in the agents namespace) holding the runner's `agent.json` + prompt templates, to
    /// mount at `/etc/lightbridge` so the runner reads its file config. `None` → not mounted.
    pub config_configmap: Option<String>,
    /// The Job's `activeDeadlineSeconds` runtime cap.
    #[serde(default, deserialize_with = "lci_config::de::opt_i64")]
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
    /// tree-sitter parse + embeddings + in-process lci-codegraph), wants more CPU/RAM. Falls back to `resources`.
    pub indexer_resources: Option<serde_json::Value>,
    /// Per-kind override for **review** Jobs (everything else) — read-mostly (reuses the indexed
    /// snapshot, ADR-0050; LLM/network-bound), so it can run leaner. Falls back to `resources`.
    pub review_resources: Option<serde_json::Value>,
}

/// Dispatcher loop timings (seconds). Each falls back to its built-in default in `dispatcher.rs`.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DispatcherSection {
    #[serde(default, deserialize_with = "lci_config::de::opt_u64")]
    pub claim_lease_seconds: Option<u64>,
    #[serde(default, deserialize_with = "lci_config::de::opt_u64")]
    pub poll_fallback_seconds: Option<u64>,
    #[serde(default, deserialize_with = "lci_config::de::opt_u64")]
    pub launch_backoff_seconds: Option<u64>,
    #[serde(default, deserialize_with = "lci_config::de::opt_u64")]
    pub reap_interval_seconds: Option<u64>,
    /// How often the index sweeper prunes stale `(repo, commit)` snapshots (ADR-0052). The outbox
    /// sweeper (ADR-0059) shares this same GC tick.
    #[serde(default, deserialize_with = "lci_config::de::opt_u64")]
    pub prune_interval_seconds: Option<u64>,
    /// How often the durable data-purge backstop re-checks for `disabled` repos with leftover index
    /// data (Epic #75). A rare recovery net (the prompt path is the spawned purge on disable/deny), so
    /// it runs on its own slow tick rather than the ~30s reaper cadence. Default 600.
    #[serde(default, deserialize_with = "lci_config::de::opt_u64")]
    pub purge_reconcile_interval_seconds: Option<u64>,
    /// Days a delivered (`posted`) `outbox` row is kept before the outbox sweeper prunes it
    /// (ADR-0059). Default 7.
    #[serde(default, deserialize_with = "lci_config::de::opt_i64")]
    pub outbox_posted_retention_days: Option<i64>,
    /// Days a dead-lettered (`failed`) `outbox` row is kept — longer, for inspection — before
    /// pruning (ADR-0059). Default 30.
    #[serde(default, deserialize_with = "lci_config::de::opt_i64")]
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
    lci_config::load::<FileConfig>(path).map(Some)
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

    #[test]
    fn gitlab_disabled_allows_no_projects() {
        let config: FileConfig =
            serde_json::from_str(r#"{"gitlab":{"enabled":false}}"#).expect("config parses");
        config
            .gitlab
            .validate()
            .expect("disabled GitLab needs no project config");
    }

    #[test]
    fn gitlab_enabled_requires_at_least_one_project() {
        let config: FileConfig =
            serde_json::from_str(r#"{"gitlab":{"enabled":true,"projects":[]}}"#)
                .expect("config parses");
        let err = config.gitlab.validate().expect_err("empty projects fail");
        assert!(err.to_string().contains("requires at least one"));
    }

    #[test]
    fn gitlab_rejects_duplicate_project_ids() {
        let json = r#"{
          "gitlab": {
            "enabled": true,
            "projects": [
              {
                "project_id": 1,
                "access_token": "token-a",
                "webhook_secret": "secret-a"
              },
              {
                "project_id": 1,
                "access_token": "token-b",
                "webhook_secret": "secret-b"
              }
            ]
          }
        }"#;
        let config: FileConfig = serde_json::from_str(json).expect("config parses");
        let err = config
            .gitlab
            .validate()
            .expect_err("duplicate project id fails");
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn gitlab_rejects_empty_project_secret_fields() {
        let json = r#"{
          "gitlab": {
            "enabled": true,
            "projects": [
              {
                "project_id": 1,
                "access_token": "",
                "webhook_secret": "secret-a"
              }
            ]
          }
        }"#;
        let config: FileConfig = serde_json::from_str(json).expect("config parses");
        let err = config
            .gitlab
            .validate()
            .expect_err("empty access token fails");
        assert!(err.to_string().contains("empty access_token"));
    }

    #[test]
    fn gitlab_resolves_project_defaults() {
        let json = r#"{
          "gitlab": {
            "enabled": true,
            "default_api_url": "https://gitlab.example.com/api/v4",
            "default_bot_handle": "lb",
            "projects": [
              {
                "project_id": 7,
                "access_token": "token",
                "webhook_secret": "secret"
              }
            ]
          }
        }"#;
        let config: FileConfig = serde_json::from_str(json).expect("config parses");
        config.gitlab.validate().expect("valid project config");
        let project = &config.gitlab.projects[0];
        assert_eq!(
            config.gitlab.resolved_api_url(project),
            "https://gitlab.example.com/api/v4"
        );
        assert_eq!(config.gitlab.resolved_bot_handle(project), "lb");
    }
}
