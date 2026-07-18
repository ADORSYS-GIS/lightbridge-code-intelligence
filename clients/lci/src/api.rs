//! Typed client for the control-plane admin API. Mirrors the server's row structs (see
//! `services/control-plane/src/db.rs`); every call carries the bearer token. We add `#[serde(default)]`
//! / `Option` liberally and do NOT use `deny_unknown_fields`, so extra server fields never break us.
//!
//! Style follows `services/agent-runner/src/bootstrap/client.rs`: a thin reqwest wrapper with
//! `anyhow::Context` on the transport and friendly, status-mapped errors on the response.
//!
//! The row structs deliberately keep some fields the current TUI doesn't render (e.g. `default_branch`,
//! `command_text`, `completed_at`) so they stay faithful, greppable mirrors of the server rows and are
//! ready when a view surfaces them — hence the module-level `dead_code` allow.
#![allow(dead_code)]

use anyhow::{Context, Result, anyhow};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

/// The caller's verified identity + effective permissions (`GET /me`).
#[derive(Debug, Clone, Deserialize)]
pub struct Me {
    pub claims: Claims,
    #[serde(default)]
    pub permissions: Vec<String>,
}

impl Me {
    /// True if the token carries the given capability (ADR-0023).
    pub fn can(&self, permission: &str) -> bool {
        self.permissions.iter().any(|p| p == permission)
    }

    /// A short human identity for the status bar: username, else email, else subject.
    pub fn identity(&self) -> &str {
        self.claims
            .preferred_username
            .as_deref()
            .or(self.claims.email.as_deref())
            .unwrap_or(&self.claims.sub)
    }
}

/// The token claims we display. Extra claims are ignored (no `deny_unknown_fields`).
#[derive(Debug, Clone, Deserialize)]
pub struct Claims {
    pub sub: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub preferred_username: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// Token expiry (unix seconds).
    #[serde(default)]
    pub exp: Option<i64>,
}

/// A repository row from `GET /admin/repositories`. Mirrors `db::RepositoryRow`.
#[derive(Debug, Clone, Deserialize)]
pub struct RepositoryRow {
    pub id: i64,
    #[serde(default)]
    pub github_repo_id: i64,
    pub owner: String,
    pub name: String,
    #[serde(default)]
    pub default_branch: String,
    /// `pending` | `approved` | `disabled`.
    pub status: String,
    #[serde(default)]
    pub active: bool,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub approved_at: Option<OffsetDateTime>,
    #[serde(default)]
    pub approved_by: Option<String>,
    #[serde(default)]
    pub task_count: i64,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_task_at: Option<OffsetDateTime>,
}

/// A task row from `GET /tasks`. Mirrors `db::TaskRow` (only the fields the TUI shows).
#[derive(Debug, Clone, Deserialize)]
pub struct TaskRow {
    pub id: Uuid,
    #[serde(default)]
    pub repository_id: i64,
    #[serde(default)]
    pub target_type: String,
    #[serde(default)]
    pub target_id: i64,
    #[serde(default)]
    pub command_text: String,
    #[serde(default)]
    pub kind: String,
    /// `received` | `waiting_for_index` | `queued` | `running` | `posting_result` | `succeeded` |
    /// `failed` | `timed_out` | `cancelled`.
    pub status: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub started_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub completed_at: Option<OffsetDateTime>,
    #[serde(default)]
    pub repo_owner: Option<String>,
    #[serde(default)]
    pub repo_name: Option<String>,
    #[serde(default)]
    pub job_name: Option<String>,
    #[serde(default)]
    pub error_detail: Option<String>,
    /// The diff range the run reviewed (mirrors `db::TaskRow`). `None` on rows the server wrote before
    /// the columns existed, and on non-PR runs; the detail view renders them as `base→head`.
    #[serde(default)]
    pub base_sha: Option<String>,
    #[serde(default)]
    pub head_sha: Option<String>,
}

impl TaskRow {
    /// Whether this task is still in flight (a candidate for cancel + the default Runs filter).
    pub fn is_active(&self) -> bool {
        matches!(
            self.status.as_str(),
            "received" | "waiting_for_index" | "queued" | "running" | "posting_result"
        )
    }
}

/// A persisted review for a run (`GET /tasks/{id}/review`). Mirrors `db::ReviewRow`. The endpoint
/// 404s when no review was recorded (older run, index task, or a review that never posted) — the
/// client maps that to `None` rather than an error.
#[derive(Debug, Clone, Deserialize)]
pub struct ReviewRow {
    pub task_id: Uuid,
    pub summary: String,
    pub body: String,
    pub inline_count: i32,
    pub deferred_count: i32,
    pub out_of_scope_count: i32,
    /// The structured findings blob (shape varies by run); kept as raw JSON — the detail view only
    /// shows the tally, not the raw findings.
    #[serde(default)]
    pub findings: Value,
    #[serde(default)]
    pub review_url: Option<String>,
    #[serde(default)]
    pub github_review_id: Option<i64>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Thin authenticated client over the control-plane base URL.
///
/// The bearer is a **shared, swappable** cell (`Arc<RwLock<String>>`) read fresh per request, so a
/// background token refresh can rotate it live without rebuilding the client (P1: a fixed bearer kept
/// sending the expired access token after ~5 min). Cloning an `ApiClient` shares the same cell.
#[derive(Clone)]
pub struct ApiClient {
    http: reqwest::Client,
    base: String,
    token: std::sync::Arc<tokio::sync::RwLock<String>>,
}

impl ApiClient {
    pub fn new(http: reqwest::Client, base: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            http,
            base: base.into(),
            token: std::sync::Arc::new(tokio::sync::RwLock::new(token.into())),
        }
    }

    /// Replace the bearer used by every subsequent request (called after a token refresh).
    pub async fn set_bearer(&self, token: impl Into<String>) {
        *self.token.write().await = token.into();
    }

    /// Read the current bearer for a single request.
    async fn bearer(&self) -> String {
        self.token.read().await.clone()
    }

    /// The host portion of the base URL, for the status bar.
    pub fn host(&self) -> String {
        url::Url::parse(&self.base)
            .ok()
            .and_then(|u| u.host_str().map(String::from))
            .unwrap_or_else(|| self.base.clone())
    }

    /// `GET /me` — identity + capabilities.
    pub async fn me(&self) -> Result<Me> {
        self.get_json("/me").await
    }

    /// `GET /admin/repositories[?status=…]`. `status` filters the approval queue; `None` returns all.
    pub async fn list_repositories(&self, status: Option<&str>) -> Result<Vec<RepositoryRow>> {
        let path = match status {
            Some(s) => format!("/admin/repositories?status={s}"),
            None => "/admin/repositories".to_string(),
        };
        self.get_json(&path).await
    }

    /// `POST /admin/repositories/{id}/approve` (needs `repo:approve`).
    pub async fn approve(&self, id: i64) -> Result<RepositoryRow> {
        self.post_json(&format!("/admin/repositories/{id}/approve"))
            .await
    }

    /// `POST /admin/repositories/{id}/deny` (needs `repo:deny`; triggers a server-side purge).
    pub async fn deny(&self, id: i64) -> Result<RepositoryRow> {
        self.post_json(&format!("/admin/repositories/{id}/deny"))
            .await
    }

    /// `GET /tasks` — most recent first, capped at 100 by the server (needs `task:read`).
    pub async fn list_tasks(&self) -> Result<Vec<TaskRow>> {
        self.get_json("/tasks").await
    }

    /// `POST /tasks/{id}/cancel` — 204 on success, 409 if already terminal (needs `task:cancel`).
    pub async fn cancel_task(&self, id: Uuid) -> Result<()> {
        let url = format!("{}/tasks/{id}/cancel", self.base);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(self.bearer().await)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        match resp.status() {
            s if s.is_success() => Ok(()),
            StatusCode::CONFLICT => Err(anyhow!("task already finished — nothing to cancel")),
            other => Err(map_status(other, "cancel task")),
        }
    }

    /// `GET /tasks/{id}` — the full metadata for one task (needs `task:read`).
    pub async fn get_task(&self, id: Uuid) -> Result<TaskRow> {
        self.get_json(&format!("/tasks/{id}")).await
    }

    /// `GET /tasks/{id}/review` — the persisted review, or `None` when the server 404s (no review
    /// recorded yet). Gated server-side on `review:read`. Other failures propagate as errors.
    pub async fn get_review(&self, id: Uuid) -> Result<Option<ReviewRow>> {
        let path = format!("/tasks/{id}/review");
        let url = format!("{}{}", self.base, path);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(self.bearer().await)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        self.decode::<ReviewRow>(resp, &path).await.map(Some)
    }

    /// Shared GET → JSON with status-mapped errors.
    async fn get_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let url = format!("{}{}", self.base, path);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(self.bearer().await)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        self.decode(resp, path).await
    }

    /// Shared POST → JSON with status-mapped errors.
    async fn post_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let url = format!("{}{}", self.base, path);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(self.bearer().await)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        self.decode(resp, path).await
    }

    /// Map the response status, then parse the body as `T`.
    async fn decode<T: for<'de> Deserialize<'de>>(
        &self,
        resp: reqwest::Response,
        path: &str,
    ) -> Result<T> {
        let status = resp.status();
        if !status.is_success() {
            return Err(map_status(status, path));
        }
        let body = resp
            .text()
            .await
            .with_context(|| format!("reading {path} body"))?;
        serde_json::from_str(&body).with_context(|| format!("parsing {path} response"))
    }
}

/// Turn an HTTP status into a friendly operator-facing error.
pub fn map_status(status: StatusCode, context: &str) -> anyhow::Error {
    let msg = match status {
        StatusCode::UNAUTHORIZED => "auth expired or invalid — re-login (lci login)".to_string(),
        StatusCode::FORBIDDEN => {
            format!("you lack the permission required for this action ({context})")
        }
        StatusCode::NOT_FOUND => format!("not found ({context})"),
        StatusCode::SERVICE_UNAVAILABLE => {
            "control plane or IdP unavailable — try again shortly".to_string()
        }
        other => format!("request failed ({other}) — {context}"),
    };
    anyhow!(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn set_bearer_swaps_the_emitted_token_and_is_shared_across_clones() {
        let client = ApiClient::new(reqwest::Client::new(), "https://api.test", "old-token");
        // A clone shares the same bearer cell (as the TUI's spawned request tasks do).
        let clone = client.clone();
        assert_eq!(client.bearer().await, "old-token");
        assert_eq!(clone.bearer().await, "old-token");

        // Simulate a refresh rotating the access token.
        client.set_bearer("new-token").await;
        assert_eq!(
            client.bearer().await,
            "new-token",
            "original sees the new bearer"
        );
        assert_eq!(
            clone.bearer().await,
            "new-token",
            "clones share the swap — spawned tasks pick up the rotated token"
        );
    }

    #[test]
    fn parses_me_fixture() {
        let json = r#"{
            "claims": {
                "sub": "abc-123",
                "email": "op@example.test",
                "preferred_username": "operator",
                "name": "The Operator",
                "exp": 1893456000,
                "azp": "lightbridge-cli"
            },
            "permissions": ["repo:read", "repo:approve", "task:read"]
        }"#;
        let me: Me = serde_json::from_str(json).unwrap();
        assert_eq!(me.identity(), "operator");
        assert!(me.can("repo:approve"));
        assert!(!me.can("repo:deny"));
        assert_eq!(me.claims.exp, Some(1893456000));
    }

    #[test]
    fn parses_repository_row_fixture() {
        let json = r#"[{
            "id": 7,
            "github_repo_id": 999001,
            "owner": "vymalo",
            "name": "lightbridge-code-intelligence",
            "default_branch": "main",
            "status": "pending",
            "active": false,
            "approved_at": null,
            "approved_by": null,
            "task_count": 12,
            "last_task_at": "2026-07-02T10:15:30Z",
            "installation_id": 42
        }]"#;
        let rows: Vec<RepositoryRow> = serde_json::from_str(json).unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.owner, "vymalo");
        assert_eq!(r.status, "pending");
        assert_eq!(r.task_count, 12);
        assert!(r.approved_at.is_none());
        assert!(r.last_task_at.is_some());
    }

    #[test]
    fn parses_task_row_fixture_and_active_flag() {
        let json = r#"[{
            "id": "3f2504e0-4f89-41d3-9a0c-0305e82c3301",
            "repository_id": 7,
            "installation_id": 42,
            "target_type": "pull_request",
            "target_id": 128,
            "command_text": "review",
            "kind": "review",
            "status": "running",
            "priority": 0,
            "created_at": "2026-07-02T09:00:00Z",
            "started_at": "2026-07-02T09:00:05Z",
            "completed_at": null,
            "repo_owner": "vymalo",
            "repo_name": "lightbridge-code-intelligence",
            "job_name": "review-abc",
            "error_detail": null,
            "base_sha": "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4",
            "head_sha": "e4f5a6b7c8d90e1f2a3b4c5d6e7f8091a2b3c4d5"
        }]"#;
        let rows: Vec<TaskRow> = serde_json::from_str(json).unwrap();
        let t = &rows[0];
        assert_eq!(t.status, "running");
        assert!(t.is_active());
        assert_eq!(t.target_id, 128);
        assert_eq!(t.repo_owner.as_deref(), Some("vymalo"));
        assert_eq!(
            t.base_sha.as_deref(),
            Some("a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4")
        );
        assert_eq!(
            t.head_sha.as_deref(),
            Some("e4f5a6b7c8d90e1f2a3b4c5d6e7f8091a2b3c4d5")
        );

        let done = TaskRow {
            status: "succeeded".into(),
            ..t.clone()
        };
        assert!(!done.is_active());
    }

    #[test]
    fn parses_review_row_fixture() {
        let json = r#"{
            "task_id": "3f2504e0-4f89-41d3-9a0c-0305e82c3301",
            "summary": "LGTM overall; two nits and one deferred concern.",
            "body": "Review body text (markdown).",
            "inline_count": 2,
            "deferred_count": 1,
            "out_of_scope_count": 0,
            "findings": {"inline": [{"path": "a.rs"}]},
            "review_url": "https://github.com/vymalo/lci/pull/128#pullrequestreview-1",
            "github_review_id": 987654,
            "created_at": "2026-07-02T09:12:00Z"
        }"#;
        let r: ReviewRow = serde_json::from_str(json).unwrap();
        assert_eq!(r.inline_count, 2);
        assert_eq!(r.deferred_count, 1);
        assert_eq!(r.out_of_scope_count, 0);
        assert!(r.review_url.is_some());
        assert_eq!(r.github_review_id, Some(987654));
        assert!(r.summary.contains("LGTM"));
    }

    #[test]
    fn status_mapping_is_friendly() {
        assert!(
            map_status(StatusCode::UNAUTHORIZED, "x")
                .to_string()
                .contains("re-login")
        );
        assert!(
            map_status(StatusCode::FORBIDDEN, "approve")
                .to_string()
                .contains("permission")
        );
        assert!(
            map_status(StatusCode::SERVICE_UNAVAILABLE, "x")
                .to_string()
                .contains("unavailable")
        );
    }
}
