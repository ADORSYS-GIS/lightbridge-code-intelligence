//! Client for the control-plane internal runner API (ADR-0017). The runner authenticates with the
//! shared bearer it was given and (a) fetches its task context + a short-lived installation token,
//! (b) reports status transitions back. This is the runner's only channel to the control plane —
//! it holds no GitHub App key and writes nothing to GitHub itself (the control plane owns that).
//!
//! The endpoint wrappers are grouped by concern into sibling modules (all still `impl
//! ControlPlaneClient`, so callers see one client with one flat method set):
//! [`tasks`] (context/status lifecycle), [`review`] (inline findings, comments, transcript,
//! telemetry), [`indexing`] (chunk/graph submission), [`search`] (semantic + structural-graph
//! queries), [`knowledge`] (ADR-0066 MCP tool discovery/dispatch), [`durable_step`] (ADR-0087
//! journal), and [`pr`] (ADR-0088 open-mode PR proposal).

use serde::Deserialize;
use uuid::Uuid;

mod durable_step;
mod indexing;
mod knowledge;
mod pr;
mod review;
mod search;
mod tasks;

pub use durable_step::StoredStep;
pub use indexing::{ChunkBatch, ChunkPayload, GraphBatch, GraphEdgePayload, GraphNodePayload};
pub use knowledge::{DiscoveredTool, KnowledgeToolResult};
pub use review::TranscriptEntry;
pub use search::{ChunkHit, SymbolHit};

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
