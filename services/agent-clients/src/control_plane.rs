//! Client for the control-plane internal runner API (ADR-0017). The runner authenticates with the
//! shared bearer it was given and (a) fetches its task context + a short-lived installation token,
//! (b) reports status transitions back. This is the runner's only channel to the control plane —
//! it holds no GitHub App key and writes nothing to GitHub itself (the control plane owns that).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The context the control plane hands the runner: repo coordinates, an installation token, and the
/// task parameters. Mirrors `control-plane/src/internal.rs::TaskContextResponse`.
#[derive(Debug, Clone, Deserialize)]
pub struct TaskContext {
    pub task_id: Uuid,
    pub repository_id: i64,
    pub owner: String,
    pub name: String,
    pub default_branch: String,
    pub clone_url: String,
    pub token: String,
    pub target_type: String,
    pub target_id: i64,
    pub command: String,
    /// Run kind (ADR-0033): `review` (diff-scoped findings) or `ask` (a conversational answer). The
    /// runner branches on this. Defaults to `review` if an older control plane omits the field.
    #[serde(default = "default_run_kind")]
    pub kind: String,
    /// Review tier (ADR-0062): `fast` (automatic `pull_request opened` — SAST + one diff-only LLM turn,
    /// no retrieval) or `deep` (`@mention` — full retrieval, multi-turn). Defaults to `deep` (the full,
    /// safe behavior) if an older control plane omits the field.
    #[serde(default = "default_tier")]
    pub tier: String,
    pub base_sha: Option<String>,
    pub head_sha: Option<String>,
    /// Whether the repo already has a semantic index — review reuses it instead of re-indexing
    /// (ADR-0025). Defaults to `false` (index) if an older control plane omits the field.
    #[serde(default)]
    pub repo_indexed: bool,
    /// The agent's own prior reviews of this target, pre-formatted by the control plane (ADR-0040 +
    /// ADR-0065). Present only on a re-review where an earlier review exists; injected as explicitly
    /// UNTRUSTED context so the run re-derives from the diff, then reconciles — retracting prior findings
    /// it cannot reproduce rather than restating them. Defaults to `None` (blind re-review, the old
    /// behavior) if an older control plane omits the field.
    #[serde(default)]
    pub prior_reviews: Option<String>,
    /// Per-repo feedback memory (M1, ADR-0044): findings a human rejected (👎) here, pre-formatted by
    /// the control plane, injected so the agent doesn't re-raise known false positives. `None` when the
    /// repo has no rejected findings, on non-review runs, or from an older control plane.
    #[serde(default)]
    pub repo_memory: Option<String>,
}

/// Default run kind when the control plane omits it (back-compat): a diff-scoped review.
fn default_run_kind() -> String {
    "review".to_string()
}

/// Default review tier when the control plane omits it (back-compat): the full `deep` review, so an
/// older control plane never silently downgrades a run to the fast/shallow path.
fn default_tier() -> String {
    "deep".to_string()
}

impl TaskContext {
    /// Attribution headers (epic #89) for the OpenAI-compatible gateway: they let the Envoy AI Gateway
    /// map this call's token spend to the customer's project (budgeting). Sent on the embeddings + the
    /// review LLM calls. Header names are lowercase per HTTP/2.
    pub fn attribution_headers(&self) -> Vec<(String, String)> {
        // Header values must be visible ASCII; a control char / non-ASCII byte makes Rust's
        // HeaderValue (embeddings) and OpenCode's Node HTTP client (review) reject it — the latter
        // would crash the review. Sanitize + bound the length defensively (the values are mostly
        // controlled, but repo names + command are not fully ours).
        let clean = |val: &str, max: usize| -> String {
            val.chars()
                .map(|c| {
                    if c.is_ascii() && !c.is_ascii_control() {
                        c
                    } else {
                        ' '
                    }
                })
                .take(max)
                .collect()
        };
        vec![
            (
                "x-code-intelligence-repo".to_string(),
                clean(&format!("{}/{}", self.owner, self.name), 200),
            ),
            // Repo OWNER (org/user login) on its own, so the gateway can bucket per-org
            // budget (x-org-id) without splitting "owner/name" in CEL.
            (
                "x-code-intelligence-owner".to_string(),
                clean(&self.owner, 100),
            ),
            (
                "x-code-intelligence-repo-id".to_string(),
                self.repository_id.to_string(),
            ),
            (
                "x-code-intelligence-task-id".to_string(),
                self.task_id.to_string(),
            ),
            (
                "x-code-intelligence-target".to_string(),
                clean(&format!("{}#{}", self.target_type, self.target_id), 100),
            ),
            (
                "x-code-intelligence-command".to_string(),
                clean(&self.command, 200),
            ),
        ]
    }

    /// The HTTPS remote with credentials embedded — what `git` is invoked against.
    ///
    /// Two paths:
    /// - **GitHub**: the control plane sends a plain `clone_url` + a short-lived installation
    ///   `token`; we splice `x-access-token:<token>@` in here (GitHub's basic-auth convention).
    /// - **GitLab** (and any platform that pre-authenticates): the control plane sends a
    ///   `clone_url` that already has credentials embedded (e.g. `https://oauth2:<token>@...`);
    ///   we detect the `@` and pass it through unchanged.
    ///
    ///   Edge case: URLs with `@` in the path (e.g. `https://github.com/owner@team/repo.git`)
    ///   will be incorrectly passed through. This is extremely rare for GitHub (subgroups with `@`
    ///   in the name are not a standard pattern) and doesn't affect GitLab (where `@` is the
    ///   credential separator).
    pub fn authenticated_clone_url(&self) -> String {
        match self.clone_url.strip_prefix("https://") {
            // Already authenticated by the control plane — use as-is.
            Some(rest) if rest.contains('@') => self.clone_url.clone(),
            // Splice the token in (GitHub's x-access-token:<token>@ form).
            Some(rest) => format!("https://x-access-token:{}@{rest}", self.token),
            None => self.clone_url.clone(),
        }
    }
}

/// One code chunk to submit to the control plane (mirrors `internal.rs::ChunkInput`).
#[derive(Debug, Serialize)]
pub struct ChunkPayload {
    pub file_path: String,
    pub language: String,
    pub chunk_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_name: Option<String>,
    pub start_line: i32,
    pub end_line: i32,
    pub content: String,
    pub embedding: Vec<f32>,
}

/// Body for `POST /internal/tasks/{id}/chunks`.
#[derive(Debug, Serialize)]
pub struct ChunkBatch {
    pub commit_sha: String,
    pub chunks: Vec<ChunkPayload>,
}

/// One structural-graph node (mirrors `internal.rs::GraphNodeInput`).
#[derive(Debug, Serialize)]
pub struct GraphNodePayload {
    pub node_id: String,
    pub label: String,
    pub source_file: String,
    pub start_line: i64,
}

/// One directed edge (`contains` / `method` / `calls` / …).
#[derive(Debug, Serialize)]
pub struct GraphEdgePayload {
    pub source: String,
    pub target: String,
    pub relation: String,
}

/// Body for `POST /internal/tasks/{id}/graph`.
#[derive(Debug, Serialize)]
pub struct GraphBatch {
    pub commit_sha: String,
    pub nodes: Vec<GraphNodePayload>,
    pub edges: Vec<GraphEdgePayload>,
}

/// A semantic-search hit (mirrors `db::CodeChunkHit`). Returned by `search`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChunkHit {
    pub file_path: String,
    pub language: String,
    pub chunk_type: String,
    pub symbol_name: Option<String>,
    pub start_line: i32,
    pub end_line: i32,
    pub content: String,
    pub score: f64,
}

/// A structural-graph symbol (mirrors `neo4j::SymbolHit`). Returned by the graph queries.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SymbolHit {
    pub node_id: String,
    pub label: String,
    pub source_file: String,
    pub start_line: i64,
}

/// One tool a discovered MCP server exposes (ADR-0066), as the control plane reports it — already
/// prefixed `mcp__<server>__<tool>` and ready to fold into the live tool schema.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DiscoveredTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Result of a knowledge-tool call (ADR-0066). Plain text, already size-capped control-plane-side —
/// untrusted content, framed as such before it reaches the model (see `review::native::tools`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KnowledgeToolResult {
    pub text: String,
}

/// One entry in the agent run transcript (ADR-0034): an assistant turn (its reasoning text +
/// `tool_calls`, with the turn's token usage) or a tool result. Submitted in order; the control plane
/// assigns the sequence. Tool-result content is truncated by the runner to keep the row bounded.
#[derive(Debug, Clone, Serialize)]
pub struct TranscriptEntry {
    /// `assistant` or `tool`.
    pub role: String,
    /// Assistant reasoning text or the tool result; `None` for an assistant turn that only called tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// The assistant turn's `tool_calls` array (raw JSON), when it called tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<serde_json::Value>,
    /// For a tool-result entry, which tool produced it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<i64>,
    /// Reasoning slice of `completion_tokens` (subset, not additive) when the model reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<i64>,
    /// The model that produced this turn (recorded in the transcript, ADR-0034).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Serialize)]
struct StatusUpdate<'a> {
    status: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'a str>,
}

/// Talks to one control plane with one task's bearer.
#[derive(Clone)]
pub struct ControlPlaneClient {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl ControlPlaneClient {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            token: token.into(),
            http: reqwest::Client::new(),
        }
    }

    /// `GET /internal/tasks/{id}` — load this task's context (with a freshly-minted token).
    pub async fn get_context(&self, task_id: Uuid) -> anyhow::Result<TaskContext> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}", self.base_url);
        let context = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .context("requesting task context")?
            .error_for_status()
            .context("control plane rejected the task-context request")?
            .json::<TaskContext>()
            .await
            .context("parsing task context")?;
        Ok(context)
    }

    /// `GET /internal/tasks/{id}/status` — the task's current status, for the self-cancel poll.
    pub async fn task_status(&self, task_id: Uuid) -> anyhow::Result<String> {
        use anyhow::Context;
        #[derive(serde::Deserialize)]
        struct StatusResponse {
            status: String,
        }
        let url = format!("{}/internal/tasks/{task_id}/status", self.base_url);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .context("requesting task status")?
            .error_for_status()
            .context("control plane rejected the task-status request")?
            .json::<StatusResponse>()
            .await
            .context("parsing task status")?;
        Ok(resp.status)
    }

    /// `POST /internal/tasks/{id}/chunks` — submit a batch of indexed code chunks.
    pub async fn submit_chunks(&self, task_id: Uuid, batch: ChunkBatch) -> anyhow::Result<()> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/chunks", self.base_url);
        self.http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&batch)
            .send()
            .await
            .context("submitting chunks")?
            .error_for_status()
            .context("control plane rejected chunk batch")?;
        Ok(())
    }

    /// `POST /internal/tasks/{id}/review/inline` — buffer one inline finding (ADR-0037 mediated write
    /// action). The control plane accumulates it and flushes on [`finalize_review`].
    ///
    /// `start_line` (ADR-0071) is the optional first line of a multi-line range — `line` is always the
    /// range's last line when both are given. Forwarded to the control plane unchanged: the runner does
    /// no range validation (ADR-0022's trust boundary puts that control-plane side), and an
    /// unrecognized/invalid range there simply falls back to a single-line comment, never a hard
    /// failure. `None` serializes as JSON `null` (an added key on the wire, but semantically identical
    /// to today's single-line behavior — same as every other `Option` field on this payload).
    #[allow(clippy::too_many_arguments)]
    pub async fn add_review_comment(
        &self,
        task_id: Uuid,
        file: &str,
        line: i32,
        start_line: Option<i32>,
        title: Option<&str>,
        priority: Option<&str>,
        category: Option<&str>,
        suggestion: Option<&str>,
        body: &str,
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/review/inline", self.base_url);
        self.http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "file": file, "line": line, "start_line": start_line, "title": title,
                "priority": priority, "category": category, "suggestion": suggestion, "body": body,
            }))
            .send()
            .await
            .context("buffering inline finding")?
            .error_for_status()
            .context("control plane rejected the inline finding")?;
        Ok(())
    }

    /// `POST /internal/tasks/{id}/review/inline/retract` — drop a buffered inline finding by
    /// `(file, line)` (Phase 2, ADR-0043): the refute pass removes a P0/P1 that didn't survive
    /// verification before it is ever posted.
    pub async fn retract_finding(
        &self,
        task_id: Uuid,
        file: &str,
        line: i32,
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        let url = format!(
            "{}/internal/tasks/{task_id}/review/inline/retract",
            self.base_url
        );
        self.http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "file": file, "line": line }))
            .send()
            .await
            .context("retracting inline finding")?
            .error_for_status()
            .context("control plane rejected the retract")?;
        Ok(())
    }

    /// `POST /internal/tasks/{id}/review/inline/clear` — drop ALL buffered inline findings. Used on an
    /// `abort` so an incomplete/untrusted run posts only its note, not its half-baked findings (a
    /// `placeholder` finding reached a PR this way — run 7c15f9bb).
    pub async fn clear_findings(&self, task_id: Uuid) -> anyhow::Result<()> {
        use anyhow::Context;
        let url = format!(
            "{}/internal/tasks/{task_id}/review/inline/clear",
            self.base_url
        );
        self.http
            .post(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .context("clearing inline findings")?
            .error_for_status()
            .context("control plane rejected the clear")?;
        Ok(())
    }

    /// `POST /internal/tasks/{id}/review/comment` — buffer one plain reply (ADR-0037).
    pub async fn add_review_reply(&self, task_id: Uuid, body: &str) -> anyhow::Result<()> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/review/comment", self.base_url);
        self.http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "body": body }))
            .send()
            .await
            .context("buffering comment")?
            .error_for_status()
            .context("control plane rejected the comment")?;
        Ok(())
    }

    /// `POST /internal/tasks/{id}/review/summary` — set the run's summary/verdict (ADR-0037).
    pub async fn set_review_summary(&self, task_id: Uuid, body: &str) -> anyhow::Result<()> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/review/summary", self.base_url);
        self.http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "body": body }))
            .send()
            .await
            .context("setting summary")?
            .error_for_status()
            .context("control plane rejected the summary")?;
        Ok(())
    }

    /// `POST /internal/tasks/{id}/review/finalize` — flush the accumulated buffer as one grouped
    /// review (ADR-0037). `outcome` is how the run ended (`finished` / `exhausted` / `aborted`,
    /// ADR-0068): the control plane suppresses the post and reacts 👍 ONLY on an explicitly clean
    /// `finished` with zero findings — an aborted/exhausted run's honest note must still post, never
    /// masquerade as a clean pass.
    pub async fn finalize_review(&self, task_id: Uuid, outcome: &str) -> anyhow::Result<()> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/review/finalize", self.base_url);
        self.http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "outcome": outcome }))
            .send()
            .await
            .context("finalizing review")?
            .error_for_status()
            .context("control plane rejected the finalize")?;
        Ok(())
    }

    /// `POST /internal/tasks/{id}/transcript` — submit the agent run transcript (ADR-0034) for
    /// observability. Best-effort: a failure here must not fail the task.
    pub async fn submit_transcript(
        &self,
        task_id: Uuid,
        entries: &[TranscriptEntry],
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/transcript", self.base_url);
        self.http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "entries": entries }))
            .send()
            .await
            .context("submitting transcript")?
            .error_for_status()
            .context("control plane rejected the transcript")?;
        Ok(())
    }

    /// `POST /internal/tasks/{id}/review/telemetry` — record run-level review telemetry at run START
    /// (ADR-0034/0062/0066): the tool set OFFERED to the model this run (`tools`, each `{name, source}`)
    /// and the resolved config, **already redacted + base64-encoded by the caller** (the api_key never
    /// leaves this process in the clear). Best-effort: a failure here must not fail the review.
    pub async fn submit_review_telemetry(
        &self,
        task_id: Uuid,
        tools: &serde_json::Value,
        config_b64: &str,
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        let url = format!(
            "{}/internal/tasks/{task_id}/review/telemetry",
            self.base_url
        );
        self.http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "tools": tools, "config_b64": config_b64 }))
            .send()
            .await
            .context("submitting review telemetry")?
            .error_for_status()
            .context("control plane rejected the review telemetry")?;
        Ok(())
    }

    /// `POST /internal/tasks/{id}/graph` — submit the structural code graph (Graphify → Neo4j).
    pub async fn submit_graph(&self, task_id: Uuid, batch: GraphBatch) -> anyhow::Result<()> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/graph", self.base_url);
        self.http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&batch)
            .send()
            .await
            .context("submitting graph")?
            .error_for_status()
            .context("control plane rejected graph batch")?;
        Ok(())
    }

    /// `POST /internal/tasks/{id}/search` — semantic search over the task's pgvector index. The
    /// caller passes the already-embedded query (the vector MCP embeds the text with the runner's
    /// embeddings key); the control plane scopes the search to the task's repo.
    pub async fn search(
        &self,
        task_id: Uuid,
        embedding: &[f32],
        limit: i64,
    ) -> anyhow::Result<Vec<ChunkHit>> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/search", self.base_url);
        let hits = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "embedding": embedding, "limit": limit }))
            .send()
            .await
            .context("semantic search request")?
            .error_for_status()
            .context("control plane rejected the search")?
            .json::<Vec<ChunkHit>>()
            .await
            .context("parsing search hits")?;
        Ok(hits)
    }

    /// `POST /internal/tasks/{id}/graph/query` with `op=find_symbol`.
    pub async fn graph_find_symbol(
        &self,
        task_id: Uuid,
        term: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<SymbolHit>> {
        self.graph_query(
            task_id,
            serde_json::json!({ "op": "find_symbol", "term": term, "limit": limit }),
        )
        .await
    }

    /// `POST /internal/tasks/{id}/graph/query` with `op=get_callers`.
    pub async fn graph_get_callers(
        &self,
        task_id: Uuid,
        node_id: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<SymbolHit>> {
        self.graph_query(
            task_id,
            serde_json::json!({ "op": "get_callers", "node_id": node_id, "limit": limit }),
        )
        .await
    }

    async fn graph_query(
        &self,
        task_id: Uuid,
        body: serde_json::Value,
    ) -> anyhow::Result<Vec<SymbolHit>> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/graph/query", self.base_url);
        let hits = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .context("graph query request")?
            .error_for_status()
            .context("control plane rejected the graph query")?
            .json::<Vec<SymbolHit>>()
            .await
            .context("parsing graph hits")?;
        Ok(hits)
    }

    /// `GET /internal/tasks/{id}/knowledge/tools` — discover every tool the currently-configured MCP
    /// servers expose (ADR-0066). Called once at run start (not compiled in — any server the control
    /// plane is configured with shows up with zero runner code changes). Empty when no servers are
    /// configured, never an error, so a review still runs normally with no external-knowledge tools.
    pub async fn list_knowledge_tools(&self, task_id: Uuid) -> anyhow::Result<Vec<DiscoveredTool>> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/knowledge/tools", self.base_url);
        let tools = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .context("knowledge-tool discovery request")?
            .error_for_status()
            .context("control plane rejected knowledge-tool discovery")?
            .json()
            .await
            .context("parsing discovered knowledge tools")?;
        Ok(tools)
    }

    /// `POST /internal/tasks/{id}/knowledge/call` — dispatch a call to a previously-discovered
    /// knowledge tool (ADR-0066). `tool` is the prefixed name from `list_knowledge_tools`
    /// (`mcp__<server>__<tool>`); `arguments` is forwarded verbatim.
    pub async fn call_knowledge_tool(
        &self,
        task_id: Uuid,
        tool: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<String> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/knowledge/call", self.base_url);
        let body: KnowledgeToolResult = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "tool": tool, "arguments": arguments }))
            .send()
            .await
            .context("knowledge-tool call request")?
            .error_for_status()
            .context("control plane rejected the knowledge-tool call")?
            .json()
            .await
            .context("parsing knowledge-tool result")?;
        Ok(body.text)
    }

    /// `POST /internal/tasks/{id}/status` — report a status transition (best-effort `detail`).
    pub async fn report_status(
        &self,
        task_id: Uuid,
        status: &str,
        detail: Option<&str>,
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/status", self.base_url);
        self.http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&StatusUpdate { status, detail })
            .send()
            .await
            .context("reporting status")?
            .error_for_status()
            .context("control plane rejected the status report")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(clone_url: &str, token: &str) -> TaskContext {
        TaskContext {
            task_id: Uuid::nil(),
            repository_id: 1,
            owner: "octo".into(),
            name: "repo".into(),
            default_branch: "main".into(),
            clone_url: clone_url.into(),
            token: token.into(),
            target_type: "pull_request".into(),
            target_id: 7,
            command: "review".into(),
            kind: "review".into(),
            tier: "deep".into(),
            base_sha: None,
            head_sha: Some("deadbeef".into()),
            repo_indexed: false,
            prior_reviews: None,
            repo_memory: None,
        }
    }

    #[test]
    fn authenticated_url_embeds_the_token_after_the_scheme() {
        let ctx = context("https://github.com/octo/repo.git", "test-tok");
        assert_eq!(
            ctx.authenticated_clone_url(),
            "https://x-access-token:test-tok@github.com/octo/repo.git"
        );
    }

    #[test]
    fn authenticated_url_passes_through_non_https_unchanged() {
        // Defensive: we only know how to splice credentials into an https remote.
        let ctx = context("git@github.com:octo/repo.git", "test-tok");
        assert_eq!(
            ctx.authenticated_clone_url(),
            "git@github.com:octo/repo.git"
        );
    }

    #[test]
    fn authenticated_url_passes_through_pre_authenticated_https_unchanged() {
        // GitLab: the control plane embeds oauth2:<token>@ in the clone_url itself.
        // The runner must NOT re-splice x-access-token: into an already-authenticated URL.
        let ctx = context(
            "https://oauth2:glpat-deadbeef@gitlab.com/group/repo.git",
            "",
        );
        assert_eq!(
            ctx.authenticated_clone_url(),
            "https://oauth2:glpat-deadbeef@gitlab.com/group/repo.git"
        );
    }

    #[test]
    fn attribution_headers_are_complete_sanitized_and_bounded() {
        let mut ctx = context("https://github.com/octo/repo.git", "test-tok");
        ctx.owner = "octo\norg".into();
        ctx.name = format!("répo{}", "x".repeat(250));
        ctx.command = "review\u{7}now".into();

        let headers = ctx.attribution_headers();
        let find = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
                .expect("attribution header")
        };

        assert_eq!(headers.len(), 6);
        assert_eq!(find("x-code-intelligence-owner"), "octo org");
        assert_eq!(find("x-code-intelligence-repo-id"), "1");
        assert_eq!(find("x-code-intelligence-task-id"), Uuid::nil().to_string());
        assert_eq!(find("x-code-intelligence-target"), "pull_request#7");
        assert_eq!(find("x-code-intelligence-command"), "review now");
        let repo = find("x-code-intelligence-repo");
        assert_eq!(repo.chars().count(), 200);
        assert!(repo.is_ascii());
    }
}
