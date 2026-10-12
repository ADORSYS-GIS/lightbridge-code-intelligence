# GitLab Integration — Detailed Implementation Plan

> **Goal:** Add GitLab support alongside the existing GitHub support with the **least necessary
> professional changes** to ensure nothing breaks. This plan follows the user's architectural
> decisions: unified webhook route, unified outbox table, manual repo approval, no agent-runner
> changes, per-project webhooks initially.

---

## Table of contents

1. [Guiding principles](#1-guiding-principles)
2. [Phase 0 — Platform-abstraction trait (no behavior change)](#2-phase-0--platform-abstraction-trait-no-behavior-change)
3. [Phase 1 — Database migration (no behavior change)](#3-phase-1--database-migration-no-behavior-change)
4. [Phase 2 — Unified webhook route (no behavior change)](#4-phase-2--unified-webhook-route-no-behavior-change)
5. [Phase 3 — Unified outbox + reconciler (no behavior change)](#5-phase-3--unified-outbox--reconciler-no-behavior-change)
6. [Phase 4 — GitLab integration module (new functionality)](#6-phase-4--gitlab-integration-module-new-functionality)
7. [Phase 5 — GitLab webhook handler (new functionality)](#7-phase-5--gitlab-webhook-handler-new-functionality)
8. [Phase 6 — Review rendering + bot detection (new functionality)](#8-phase-6--review-rendering--bot-detection-new-functionality)
9. [Phase 7 — Testing, docs, and polish](#9-phase-7--testing-docs-and-polish)
10. [File change manifest](#10-file-change-manifest)
11. [New env vars and config](#11-new-env-vars-and-config)
12. [Migration SQL (complete)](#12-migration-sql-complete)
13. [Rollout checklist](#13-rollout-checklist)

---

## 1. Guiding principles

1. **Nothing breaks.** Every phase before Phase 4 is a pure refactor — GitHub works exactly as
   before. Each phase is independently deployable and revertible.

2. **Add, don't remove.** New columns get `DEFAULT 'github'` so existing rows are valid without
   backfill. Old column names are kept as aliases during the transition (dropped in a later
   migration only after all code is updated).

3. **One trait, two implementations.** A `CodePlatform` trait abstracts the platform-specific
   operations. `GithubApp` implements it (refactored from existing code). `GitlabClient`
   implements it (new).

4. **Unified routes and tables.** A single `/webhook` route detects the platform from headers. A
   single `outbox` table carries a `platform` column. Logs include the platform for easy tracing.

5. **Agent-runner untouched.** The control plane provides the clone URL + token via the internal
   API. The runner clones a Git URL — it doesn't know or care about the platform.

---

## 2. Phase 0 — Platform-abstraction trait (no behavior change)

**Goal:** Introduce the `CodePlatform` trait and refactor `github.rs` to implement it. All
existing behavior is unchanged.

### 2.1 New file: `services/control-plane/src/integrations/platform.rs`

```rust
//! Platform abstraction: a single trait that GitHub and GitLab both implement.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Which code-hosting platform a repository lives on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
pub enum Platform {
    GitHub,
    GitLab,
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Platform::GitHub => write!(f, "github"),
            Platform::GitLab => write!(f, "gitlab"),
        }
    }
}

impl std::str::FromStr for Platform {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "github" => Ok(Platform::GitHub),
            "gitlab" => Ok(Platform::GitLab),
            other => Err(format!("unknown platform: {other}")),
        }
    }
}

/// A reference to a repository, platform-agnostic.
#[derive(Debug, Clone)]
pub struct RepoRef {
    pub platform: Platform,
    pub full_name: String,       // "owner/repo" or "namespace/path"
    pub platform_repo_id: i64,  // GitHub repo ID or GitLab project ID
    pub installation_id: i64,   // GitHub installation ID or GitLab project ID (same as platform_repo_id for GitLab)
}

/// A changed file in a PR/MR diff.
#[derive(Debug, Clone)]
pub struct ChangedFile {
    pub path: String,
    pub patch: Option<String>,   // unified diff text
    pub additions: i64,
    pub deletions: i64,
}

/// An inline review comment to post.
#[derive(Debug, Clone)]
pub struct InlineComment {
    pub path: String,
    pub line: u32,
    pub body: String,
}

/// A review to post (body + inline comments).
#[derive(Debug, Clone)]
pub struct ReviewPost {
    pub pr_number: i64,
    pub body: String,
    pub comments: Vec<InlineComment>,
    pub labels: Vec<String>,
}

/// A reaction/award emoji to post.
#[derive(Debug, Clone)]
pub enum ReactionTarget {
    Issue { number: i64 },
    Comment { comment_id: i64 },
}

/// A reaction found on a comment (for feedback polling).
#[derive(Debug, Clone)]
pub struct Reaction {
    pub content: String,   // "thumbsup", "thumbsdown", "eyes", "+1", "-1"
    pub user_login: String,
}

/// The platform trait. GitHub and GitLab both implement this.
#[async_trait]
pub trait CodePlatform: Send + Sync {
    /// Human-readable platform name for logs.
    fn name(&self) -> &'static str;

    /// Verify a webhook signature. Returns `true` if valid.
    fn verify_webhook(&self, headers: &http::HeaderMap, body: &[u8]) -> bool;

    /// Extract the delivery (dedup) ID from webhook headers.
    fn delivery_id(&self, headers: &http::HeaderMap) -> Option<String>;

    /// Extract the event type string from webhook headers.
    fn event_type(&self, headers: &http::HeaderMap) -> Option<String>;

    /// List changed files in a PR/MR.
    async fn list_changed_files(
        &self,
        repo: &RepoRef,
        pr_number: i64,
    ) -> anyhow::Result<Vec<ChangedFile>>;

    /// Get the default branch of a repository.
    async fn default_branch(&self, repo: &RepoRef) -> anyhow::Result<String>;

    /// Get the base/head SHAs of a PR/MR.
    async fn pr_shas(
        &self,
        repo: &RepoRef,
        pr_number: i64,
    ) -> anyhow::Result<(String, String)>;

    /// Post a review (inline comments + body). Returns a platform review ID.
    async fn post_review(
        &self,
        repo: &RepoRef,
        review: &ReviewPost,
    ) -> anyhow::Result<i64>;

    /// Post a comment on an issue/PR/MR. Returns the platform comment ID.
    async fn post_comment(
        &self,
        repo: &RepoRef,
        issue_number: i64,
        body: &str,
    ) -> anyhow::Result<i64>;

    /// Add a reaction (emoji) to an issue/PR/MR or a comment.
    async fn add_reaction(
        &self,
        repo: &RepoRef,
        target: ReactionTarget,
        emoji: &str,
    ) -> anyhow::Result<()>;

    /// Add labels to an issue/PR/MR.
    async fn add_labels(
        &self,
        repo: &RepoRef,
        issue_number: i64,
        labels: &[String],
    ) -> anyhow::Result<()>;

    /// List reactions on a comment (for feedback polling).
    async fn list_comment_reactions(
        &self,
        repo: &RepoRef,
        comment_id: i64,
    ) -> anyhow::Result<Vec<Reaction>>;

    /// Build the clone URL with credentials for the agent-runner.
    fn clone_url(&self, repo: &RepoRef) -> String;
}
```

### 2.2 Refactor `integrations/github.rs`

- Add `impl CodePlatform for GithubApp` that delegates to the existing methods.
- The existing methods (`list_pr_files`, `create_pr_review`, etc.) stay as-is — the trait impl
  calls them.
- `verify_webhook` implements HMAC-SHA256 (moved from `webhook.rs::verify_signature`).
- `delivery_id` reads `X-GitHub-Delivery`.
- `event_type` reads `X-GitHub-Event`.
- `clone_url` returns `https://x-access-token:{token}@github.com/{owner}/{repo}.git`.

**No existing code is removed.** The trait impl is additive.

### 2.3 Refactor `http/webhook.rs`

- `github_webhook()` now calls `github.verify_webhook()` instead of the local `verify_signature()`.
- The local `verify_signature()` function is kept (used by tests) but delegates to the trait.
- **No other changes.** Event routing, payload parsing, and task creation stay the same.

### 2.4 Verification

- `cargo test -p control-plane` — all existing tests pass.
- `cargo clippy -D warnings` — no new warnings.
- Manual: send a test webhook, verify it's processed correctly.

---

## 3. Phase 1 — Database migration (no behavior change)

**Goal:** Rename `github_*` columns/tables to platform-agnostic names and add a `platform`
column. All existing rows get `platform = 'github'`.

### 3.1 New migration: `services/control-plane/migrations/0024_platform_abstraction.sql`

```sql
-- Phase 1: Platform abstraction — rename GitHub-specific columns and tables.
-- All existing rows get platform = 'github' (the default).

-- 1. repositories: add platform, rename github_repo_id → platform_repo_id
ALTER TABLE repositories ADD COLUMN platform TEXT NOT NULL DEFAULT 'github';
ALTER TABLE repositories RENAME COLUMN github_repo_id TO platform_repo_id;
-- The old UNIQUE constraint on github_repo_id is now on platform_repo_id.
-- We need a composite unique: (platform, platform_repo_id) since a GitHub repo ID
-- and a GitLab project ID could collide numerically.
DROP INDEX IF EXISTS repositories_platform_repo_id_key;
CREATE UNIQUE INDEX repositories_platform_repo_id_unique
    ON repositories (platform, platform_repo_id);

-- 2. tasks: rename github_delivery_id → webhook_delivery_id, installation_id stays
-- (installation_id is used for both GitHub installation ID and GitLab project ID)
ALTER TABLE tasks RENAME COLUMN github_delivery_id TO webhook_delivery_id;
-- The FK on webhook_delivery_id → github_deliveries(delivery_id) still holds
-- until we rename the table below.

-- 3. github_deliveries → webhook_deliveries
ALTER TABLE github_deliveries RENAME TO webhook_deliveries;
ALTER TABLE webhook_deliveries ADD COLUMN platform TEXT NOT NULL DEFAULT 'github';
-- The FK from tasks.webhook_delivery_id → webhook_deliveries.delivery_id is
-- preserved by the rename.

-- 4. github_outbox → outbox
ALTER TABLE github_outbox RENAME TO outbox;
ALTER TABLE outbox ADD COLUMN platform TEXT NOT NULL DEFAULT 'github';
-- Rename github_id → platform_ref_id (the ID of the posted resource on the platform)
ALTER TABLE outbox RENAME COLUMN github_id TO platform_ref_id;

-- 5. reviews: rename github_review_id → platform_review_id
ALTER TABLE reviews RENAME COLUMN github_review_id TO platform_review_id;

-- 6. review_comments: rename github_comment_id → platform_comment_id
ALTER TABLE review_comments RENAME COLUMN github_comment_id TO platform_comment_id;

-- 7. review_feedback: add platform
ALTER TABLE review_feedback ADD COLUMN platform TEXT NOT NULL DEFAULT 'github';
```

### 3.2 Update `db.rs` — all SQL queries

Every query that references `github_repo_id`, `github_delivery_id`, `github_comment_id`,
`github_review_id`, `github_deliveries`, `github_outbox`, or `github_id` must be updated to the
new names. This is a mechanical find-and-replace:

| Old name | New name |
|----------|----------|
| `github_repo_id` | `platform_repo_id` |
| `github_delivery_id` | `webhook_delivery_id` |
| `github_comment_id` | `platform_comment_id` |
| `github_review_id` | `platform_review_id` |
| `github_deliveries` | `webhook_deliveries` |
| `github_outbox` | `outbox` |
| `github_id` (in outbox) | `platform_ref_id` |

**Functions to update in `db.rs`:**

- `record_delivery()` — INSERT into `webhook_deliveries`
- `upsert_repository()` — use `platform_repo_id`, add `platform` parameter
- `register_pending_repository()` — use `platform_repo_id`, add `platform` parameter
- `set_repository_status_by_github_id()` → rename to `set_repository_status_by_platform_id()`, add `platform` parameter
- `create_task()` / `create_explicit_task()` / `create_index_task()` — `NewTask` struct: rename `github_delivery_id` → `webhook_delivery_id`
- `enqueue_github_post()` → rename to `enqueue_outbox_post()`, add `platform` parameter
- `claim_outbox_batch()` — SELECT from `outbox`, include `platform` column
- `mark_outbox_posted()` — use `platform_ref_id` instead of `github_id`
- `store_review_comments()` — use `platform_comment_id`
- `reconcile_comment_feedback()` — use `platform_comment_id`
- `upsert_review()` — use `platform_review_id`
- `list_pollable_comments()` — include `platform` column
- `GITHUB_OUTBOX_CHANNEL` → `OUTBOX_CHANNEL` (the NOTIFY channel name)

### 3.3 Update `outbox.rs`

- `Target` struct: add `platform: Platform` field
- All `enqueue_*` functions: add `platform` parameter, pass to `db::enqueue_outbox_post()`
- The `ReviewPayload`, `ReviewCommentPayload` structs stay the same (they're platform-neutral)

### 3.4 Update callers of `outbox.rs`

Every call to `enqueue_review()`, `enqueue_reply()`, `enqueue_reaction()`,
`enqueue_verdict_reaction()`, `enqueue_failure_notice()` must pass `Platform::GitHub`. These
callers are in:
- `http/internal.rs` (review buffer finalize)
- `http/webhook.rs` (spawn_react_seen)
- `review.rs` (review posting)
- `queue/lifecycle.rs` (purge)

### 3.5 Verification

- `cargo test -p control-plane` — all existing tests pass (tests use the new column names).
- `#[sqlx::test]` integration tests against live pgvector — catches schema drift.
- Manual: run the migration against a dev DB, verify all existing data is intact with
  `platform = 'github'`.

---

## 4. Phase 2 — Unified webhook route (no behavior change)

**Goal:** Replace the hardcoded `/github/webhook` route with a unified `/webhook` route that
detects the platform from headers. GitHub webhooks still work exactly as before.

### 4.1 Platform detection logic

```rust
// http/webhook.rs

/// Detect the platform from webhook headers.
/// GitHub sends `X-GitHub-Event` + `X-Hub-Signature-256` + `X-GitHub-Delivery`.
/// GitLab sends `X-Gitlab-Event` + `X-Gitlab-Token` + `X-Gitlab-Event-UUID`.
fn detect_platform(headers: &HeaderMap) -> Option<Platform> {
    if headers.contains_key("x-github-event") {
        Some(Platform::GitHub)
    } else if headers.contains_key("x-gitlab-event") {
        Some(Platform::GitLab)
    } else {
        None
    }
}
```

### 4.2 Unified webhook handler

```rust
/// Unified webhook receiver. Detects the platform from headers and dispatches.
pub async fn webhook_router(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let platform = match detect_platform(&headers) {
        Some(p) => p,
        None => {
            tracing::warn!(
                headers = ?headers.keys().map(|k| k.as_str()).collect::<Vec<_>>(),
                "webhook: unknown platform (no X-GitHub-Event or X-Gitlab-Event header)"
            );
            return (StatusCode::BAD_REQUEST, "unknown platform").into_response();
        }
    };

    tracing::info!(%platform, "webhook received");

    // Get the platform client from AppState
    let client: Arc<dyn CodePlatform> = match platform {
        Platform::GitHub => state.github_client(),
        Platform::GitLab => state.gitlab_client(),
    };

    // Verify signature
    if !client.verify_webhook(&headers, &body) {
        tracing::warn!(%platform, "webhook: signature verification failed");
        return (StatusCode::UNAUTHORIZED, "invalid signature").into_response();
    }

    // Get delivery ID for dedup
    let delivery_id = match client.delivery_id(&headers) {
        Some(id) => id,
        None => return (StatusCode::BAD_REQUEST, "missing delivery id").into_response(),
    };

    // Get event type
    let event_type = match client.event_type(&headers) {
        Some(ev) => ev,
        None => return (StatusCode::BAD_REQUEST, "missing event type").into_response(),
    };

    tracing::info!(%platform, %delivery_id, %event_type, "webhook: verified, processing");

    // Parse body as JSON
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid json").into_response(),
    };

    // Dedup
    let is_new = match &state.db {
        Some(pool) => db::record_delivery(pool, &delivery_id, &event_type, &payload).await,
        None => {
            // In-memory dedup (dev mode)
            let mut seen = state.seen_deliveries.lock().unwrap();
            if !seen.insert(delivery_id.clone()) {
                tracing::info!(%delivery_id, "webhook: duplicate delivery (in-memory)");
                return (StatusCode::ACCEPTED, "duplicate delivery").into_response();
            }
            true
        }
    };
    if !is_new {
        tracing::info!(%delivery_id, "webhook: duplicate delivery (DB)");
        return (StatusCode::ACCEPTED, "duplicate delivery").into_response();
    }

    // Route by platform
    match platform {
        Platform::GitHub => route_github_event(&state, &event_type, &payload).await,
        Platform::GitLab => route_gitlab_event(&state, &event_type, &payload).await,
    }
}
```

### 4.3 Route registration in `main.rs`

```rust
// Old:
.route("/github/webhook", post(webhook::github_webhook))

// New:
.route("/webhook", post(webhook::webhook_router))
// Keep the old route as a redirect/alias during transition:
.route("/github/webhook", post(webhook::github_webhook_legacy))
```

Where `github_webhook_legacy` simply forwards to `webhook_router` (or we keep the old handler
temporarily and remove it in a later release).

### 4.4 Readiness check relaxation

```rust
// main.rs readiness()
// Old: fails if github_webhook_secret.is_empty()
// New: fails only if BOTH github and gitlab webhook secrets are empty

let github_configured = !state.github_webhook_secret.is_empty() || state.github.is_some();
let gitlab_configured = !state.gitlab_webhook_secret.is_empty() || state.gitlab.is_some();
if !github_configured && !gitlab_configured {
    return Err("no platform webhook secret configured");
}
```

### 4.5 AppState changes

```rust
pub struct AppState {
    // ... existing fields ...

    // GitHub (existing)
    github_webhook_secret: Arc<String>,
    github: Option<github::GithubApp>,

    // GitLab (new in Phase 4, but the fields are added here for forward compatibility)
    gitlab_webhook_secret: Arc<String>,
    gitlab: Option<gitlab::GitlabClient>,

    // Unified: provide platform clients as Arc<dyn CodePlatform>
    // (added in Phase 4 when GitlabClient exists; for now, github_client() returns an Arc)
}

impl AppState {
    fn github_client(&self) -> Arc<dyn CodePlatform> {
        // Wrap GithubApp in an Arc<dyn CodePlatform>
        // This is constructed once in from_env() and stored
        ...
    }

    fn gitlab_client(&self) -> Arc<dyn CodePlatform> {
        // In Phase 2, this panics if GitLab is not configured
        // In Phase 4, this returns the real GitlabClient
        ...
    }
}
```

### 4.6 Verification

- `cargo test -p control-plane` — all existing tests pass.
- Manual: POST a GitHub webhook to `/webhook` (with GitHub headers), verify it's processed.
- Manual: POST to `/github/webhook` (legacy route), verify it still works.
- Manual: POST with no platform headers, verify 400 with a clear error message.

---

## 5. Phase 3 — Unified outbox + reconciler (no behavior change)

**Goal:** Generalize the reconciler to dispatch via the `CodePlatform` trait instead of calling
`GithubApp` directly. The outbox table now has a `platform` column.

### 5.1 Generalize `queue/reconciler.rs`

```rust
// Old: run() takes app: GithubApp
// New: run() takes a HashMap<Platform, Arc<dyn CodePlatform>>

pub async fn run(
    pool: PgPool,
    platforms: HashMap<Platform, Arc<dyn CodePlatform>>,
    review: Arc<config::ReviewSection>,
    interval: Duration,
    within_days: i32,
) -> anyhow::Result<()> {
    // ... spawn feedback poll ...
    // ... run outbox drain ...
}

async fn drain_once(
    pool: &PgPool,
    platforms: &HashMap<Platform, Arc<dyn CodePlatform>>,
) -> anyhow::Result<()> {
    let batch = db::claim_outbox_batch(pool, DRAIN_BATCH).await?;

    for row in batch {
        let platform: Platform = row.platform;  // new field from the outbox table
        let client = match platforms.get(&platform) {
            Some(c) => c,
            None => {
                tracing::error!(
                    %platform,
                    row_id = %row.id,
                    "outbox: no platform client configured for this row, marking failed"
                );
                db::mark_outbox_failed(pool, row.id, "no platform client").await?;
                continue;
            }
        };

        tracing::info!(%platform, row_id = %row.id, kind = %row.kind, "outbox: delivering");

        deliver(pool, client, &row).await?;
    }
    Ok(())
}

async fn deliver(
    pool: &PgPool,
    client: &dyn CodePlatform,
    row: &OutboxRow,
) -> anyhow::Result<()> {
    let repo = RepoRef {
        platform: row.platform,
        full_name: format!("{}/{}", row.owner, row.repo),
        platform_repo_id: /* from row */,
        installation_id: row.installation_id,
    };

    match row.kind.as_str() {
        "review" => deliver_review(pool, client, &repo, row).await?,
        "reply" => {
            let body = row.payload["body"].as_str().unwrap_or("");
            let comment_id = client.post_comment(&repo, row.payload["issue"].as_i64().unwrap_or(0), body).await?;
            db::record_comment(pool, row.task_id, comment_id, &row.kind).await?;
        }
        "reaction" => {
            let target = /* build ReactionTarget from payload */;
            let emoji = row.payload["content"].as_str().unwrap_or("");
            client.add_reaction(&repo, target, emoji).await?;
        }
        "failure_notice" => {
            if !db::has_responded_or_pending_content(pool, row.task_id).await? {
                let body = row.payload["body"].as_str().unwrap_or("");
                client.post_comment(&repo, /* issue */, body).await?;
            }
        }
        other => tracing::warn!(kind = other, "outbox: unknown kind"),
    }

    db::mark_outbox_posted(pool, row.id, /* platform_ref_id */).await?;
    Ok(())
}
```

### 5.2 Generalize feedback poller

```rust
async fn poll_once(
    pool: &PgPool,
    platforms: &HashMap<Platform, Arc<dyn CodePlatform>>,
    within_days: i32,
    interval_secs: i64,
) -> anyhow::Result<()> {
    let comments = db::list_pollable_comments(pool, within_days, interval_secs).await?;

    for comment in comments {
        let platform: Platform = comment.platform;  // new field
        let client = match platforms.get(&platform) {
            Some(c) => c,
            None => continue,
        };

        let repo = /* build RepoRef from comment */;
        let reactions = client.list_comment_reactions(&repo, comment.platform_comment_id).await?;

        db::reconcile_comment_feedback(pool, comment.task_id, comment.platform_comment_id, &comment.kind, &reactions).await?;
    }
    Ok(())
}
```

### 5.3 Update `main.rs` reconciler startup

```rust
// Old:
if let Some(app) = &state.github {
    run_reconciler(state.db.clone().unwrap(), app.clone(), ...).await;
}

// New:
let mut platforms = HashMap::new();
if let Some(github) = &state.github {
    platforms.insert(Platform::GitHub, github.clone() as Arc<dyn CodePlatform>);
}
if let Some(gitlab) = &state.gitlab {
    platforms.insert(Platform::GitLab, gitlab.clone() as Arc<dyn CodePlatform>);
}
if !platforms.is_empty() {
    run_reconciler(state.db.clone().unwrap(), platforms, ...).await;
}
```

### 5.4 Verification

- `cargo test -p control-plane` — all existing tests pass.
- Manual: trigger a review, verify it's posted to GitHub via the trait dispatch.
- Verify logs include `platform=github` for tracing.

---

## 6. Phase 4 — GitLab integration module (new functionality)

**Goal:** Create `integrations/gitlab.rs` implementing `CodePlatform` for GitLab.

### 6.1 New file: `services/control-plane/src/integrations/gitlab.rs`

```rust
//! GitLab API client — implements the CodePlatform trait.
//! Auth: a single access token (project, group, or personal) sent as `PRIVATE-TOKEN` header.
//! API base: `{GITLAB_API_URL}` (e.g. `https://gitlab.com/api/v4` or `https://gitlab.example.com/api/v4`).

use async_trait::async_trait;
use crate::integrations::platform::*;

pub struct GitlabClient {
    api_url: String,       // e.g. "https://gitlab.example.com/api/v4"
    token: String,         // access token
    bot_handle: String,    // e.g. "lightbridge-bot"
    bot_user_id: Option<i64>,  // optional: for bot detection
    http: reqwest::Client,
}

impl GitlabClient {
    pub fn from_env() -> Option<Self> {
        let api_url = std::env::var("GITLAB_API_URL").ok()?;
        let token = std::env::var("GITLAB_API_TOKEN").ok()?;
        if token.is_empty() { return None; }

        let bot_handle = std::env::var("GITLAB_BOT_HANDLE")
            .unwrap_or_else(|_| "lightbridge-bot".to_string());
        let bot_user_id = std::env::var("GITLAB_BOT_USER_ID")
            .ok()
            .and_then(|s| s.parse().ok());

        Some(Self {
            api_url: api_url.trim_end_matches('/').to_string(),
            token,
            bot_handle,
            bot_user_id,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .ok()?,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.api_url, path)
    }

    /// URL-encode a project path: "group/repo" → "group%2Frepo"
    fn project_path_encoded(repo: &RepoRef) -> String {
        repo.full_name.replace('/', "%2F")
    }
}

#[async_trait]
impl CodePlatform for GitlabClient {
    fn name(&self) -> &'static str { "gitlab" }

    fn verify_webhook(&self, headers: &HeaderMap, _body: &[u8]) -> bool {
        // GitLab uses a plain token comparison (X-Gitlab-Token header), not HMAC.
        let secret = std::env::var("GITLAB_WEBHOOK_SECRET").unwrap_or_default();
        if secret.is_empty() { return false; }  // fail-closed

        match headers.get("x-gitlab-token") {
            Some(token) => {
                // Constant-time comparison
                use subtle::ConstantTimeEq;
                token.as_bytes().ct_eq(secret.as_bytes()).into()
            }
            None => false,
        }
    }

    fn delivery_id(&self, headers: &HeaderMap) -> Option<String> {
        headers.get("x-gitlab-event-uuid")?
            .to_str().ok()
            .map(|s| s.to_string())
    }

    fn event_type(&self, headers: &HeaderMap) -> Option<String> {
        headers.get("x-gitlab-event")?
            .to_str().ok()
            .map(|s| s.to_string())
    }

    async fn list_changed_files(&self, repo: &RepoRef, mr_iid: i64) -> anyhow::Result<Vec<ChangedFile>> {
        let path = format!("/projects/{}/merge_requests/{}/changes",
            Self::project_path_encoded(repo), mr_iid);
        let resp: Value = self.http.get(self.url(&path))
            .header("PRIVATE-TOKEN", &self.token)
            .send().await?
            .json().await?;

        let changes = resp["changes"].as_array().unwrap_or(&vec![]);
        Ok(changes.iter().map(|c| ChangedFile {
            path: c["new_path"].as_str().unwrap_or("").to_string(),
            patch: c["diff"].as_str().map(|s| s.to_string()),
            additions: c["additions"].as_i64().unwrap_or(0),
            deletions: c["deletions"].as_i64().unwrap_or(0),
        }).collect())
    }

    async fn default_branch(&self, repo: &RepoRef) -> anyhow::Result<String> {
        let path = format!("/projects/{}", Self::project_path_encoded(repo));
        let resp: Value = self.http.get(self.url(&path))
            .header("PRIVATE-TOKEN", &self.token)
            .send().await?
            .json().await?;
        Ok(resp["default_branch"].as_str().unwrap_or("main").to_string())
    }

    async fn pr_shas(&self, repo: &RepoRef, mr_iid: i64) -> anyhow::Result<(String, String)> {
        let path = format!("/projects/{}/merge_requests/{}",
            Self::project_path_encoded(repo), mr_iid);
        let resp: Value = self.http.get(self.url(&path))
            .header("PRIVATE-TOKEN", &self.token)
            .send().await?
            .json().await?;
        let base = resp["diff_refs"]["base_sha"].as_str().unwrap_or("").to_string();
        let head = resp["diff_refs"]["head_sha"].as_str().unwrap_or("").to_string();
        Ok((base, head))
    }

    async fn post_review(&self, repo: &RepoRef, review: &ReviewPost) -> anyhow::Result<i64> {
        // GitLab has no "review" object. Post inline comments as discussion threads,
        // and the body as a note on the MR.

        // 1. Post each inline comment as a discussion thread
        for comment in &review.comments {
            let path = format!("/projects/{}/merge_requests/{}/discussions",
                Self::project_path_encoded(repo), review.pr_number);
            let position = json!({
                "position": {
                    "base_sha": /* base_sha */,
                    "head_sha": /* head_sha */,
                    "start_sha": /* start_sha */,
                    "position_type": "text",
                    "new_path": comment.path,
                    "old_path": comment.path,
                    "new_line": comment.line,
                    "old_line": null,
                }
            });
            self.http.post(self.url(&path))
                .header("PRIVATE-TOKEN", &self.token)
                .json(&json!({
                    "body": comment.body,
                    "position": position["position"],
                }))
                .send().await?;
        }

        // 2. Post the review body as a note on the MR
        let path = format!("/projects/{}/merge_requests/{}/notes",
            Self::project_path_encoded(repo), review.pr_number);
        let resp: Value = self.http.post(self.url(&path))
            .header("PRIVATE-TOKEN", &self.token)
            .json(&json!({ "body": review.body }))
            .send().await?
            .json().await?;

        // 3. Add labels if any
        if !review.labels.is_empty() {
            let path = format!("/projects/{}/merge_requests/{}",
                Self::project_path_encoded(repo), review.pr_number);
            self.http.put(self.url(&path))
                .header("PRIVATE-TOKEN", &self.token)
                .json(&json!({ "labels": review.labels.join(",") }))
                .send().await?;
        }

        // Return the note ID as the "review ID"
        Ok(resp["id"].as_i64().unwrap_or(0))
    }

    async fn post_comment(&self, repo: &RepoRef, issue_number: i64, body: &str) -> anyhow::Result<i64> {
        // Try MR first, then issue (the caller knows which type it is, but GitLab uses
        // different endpoints. We'll use the payload's target_type to decide.)
        // For simplicity, this tries MR notes, then issue notes.
        let mr_path = format!("/projects/{}/merge_requests/{}/notes",
            Self::project_path_encoded(repo), issue_number);
        let resp = self.http.post(self.url(&mr_path))
            .header("PRIVATE-TOKEN", &self.token)
            .json(&json!({ "body": body }))
            .send().await?;

        if resp.status().is_success() {
            let json: Value = resp.json().await?;
            return Ok(json["id"].as_i64().unwrap_or(0));
        }

        // Try issue notes
        let issue_path = format!("/projects/{}/issues/{}/notes",
            Self::project_path_encoded(repo), issue_number);
        let resp: Value = self.http.post(self.url(&issue_path))
            .header("PRIVATE-TOKEN", &self.token)
            .json(&json!({ "body": body }))
            .send().await?
            .json().await?;
        Ok(resp["id"].as_i64().unwrap_or(0))
    }

    async fn add_reaction(&self, repo: &RepoRef, target: ReactionTarget, emoji: &str) -> anyhow::Result<()> {
        let (kind, id) = match target {
            ReactionTarget::Issue { number } => ("issues", number),
            ReactionTarget::Comment { comment_id } => ("notes", comment_id),
        };
        let path = match target {
            ReactionTarget::Issue { number } => format!("/projects/{}/issues/{}/award_emoji",
                Self::project_path_encoded(repo), number),
            ReactionTarget::Comment { comment_id } => format!("/projects/{}/merge_requests/{}/notes/{}/award_emoji",
                Self::project_path_encoded(repo), /* mr_iid */, comment_id),
        };
        self.http.post(self.url(&path))
            .header("PRIVATE-TOKEN", &self.token)
            .json(&json!({ "name": emoji }))
            .send().await?;
        Ok(())
    }

    async fn add_labels(&self, repo: &RepoRef, issue_number: i64, labels: &[String]) -> anyhow::Result<()> {
        let path = format!("/projects/{}/merge_requests/{}",
            Self::project_path_encoded(repo), issue_number);
        self.http.put(self.url(&path))
            .header("PRIVATE-TOKEN", &self.token)
            .json(&json!({ "labels": labels.join(",") }))
            .send().await?;
        Ok(())
    }

    async fn list_comment_reactions(&self, repo: &RepoRef, comment_id: i64) -> anyhow::Result<Vec<Reaction>> {
        // List award emoji on a note
        let path = format!("/projects/{}/merge_requests/{}/notes/{}/award_emoji",
            Self::project_path_encoded(repo), /* mr_iid */, comment_id);
        let resp: Vec<Value> = self.http.get(self.url(&path))
            .header("PRIVATE-TOKEN", &self.token)
            .send().await?
            .json().await?;
        Ok(resp.iter().map(|r| Reaction {
            content: r["name"].as_str().unwrap_or("").to_string(),
            user_login: r["user"]["username"].as_str().unwrap_or("").to_string(),
        }).collect())
    }

    fn clone_url(&self, repo: &RepoRef) -> String {
        // Use oauth2 token in URL for git clone
        // Format: https://oauth2:{token}@gitlab.example.com/{namespace}/{path}.git
        let base = self.api_url.trim_end_matches("/api/v4");
        format!("https://oauth2:{}@{}/{}.git", self.token, base, repo.full_name)
    }
}
```

### 6.2 Update `AppState::from_env()` in `main.rs`

```rust
// Add GitLab client construction
let gitlab = gitlab::GitlabClient::from_env();
let gitlab_webhook_secret = Arc::new(std::env::var("GITLAB_WEBHOOK_SECRET").unwrap_or_default());

// Add to AppState
pub struct AppState {
    // ... existing ...
    gitlab: Option<gitlab::GitlabClient>,
    gitlab_webhook_secret: Arc<String>,
}

// Platform client accessors
impl AppState {
    fn platform_client(&self, platform: Platform) -> Option<Arc<dyn CodePlatform>> {
        match platform {
            Platform::GitHub => self.github.as_ref().map(|g| Arc::new(g.clone()) as Arc<dyn CodePlatform>),
            Platform::GitLab => self.gitlab.as_ref().map(|g| Arc::new(g.clone()) as Arc<dyn CodePlatform>),
        }
    }
}
```

### 6.3 Update reconciler startup in `main.rs`

```rust
let mut platforms = HashMap::new();
if let Some(github) = &state.github {
    platforms.insert(Platform::GitHub, Arc::new(github.clone()) as Arc<dyn CodePlatform>);
}
if let Some(gitlab) = &state.gitlab {
    platforms.insert(Platform::GitLab, Arc::new(gitlab.clone()) as Arc<dyn CodePlatform>);
}
```

### 6.4 Verification

- `cargo build -p control-plane` — compiles with GitLab support.
- `cargo test -p control-plane` — all existing tests pass.
- Manual: start control plane with `GITLAB_API_URL` + `GITLAB_API_TOKEN` set, verify it starts.

---

## 7. Phase 5 — GitLab webhook handler (new functionality)

**Goal:** Parse GitLab webhook payloads and create tasks.

### 7.1 New functions in `http/webhook.rs`

```rust
/// Route a GitLab webhook event to the appropriate handler.
async fn route_gitlab_event(
    state: &Arc<AppState>,
    event_type: &str,
    payload: &Value,
) -> Response {
    let pool = match &state.db {
        Some(p) => p,
        None => return (StatusCode::ACCEPTED, "no db").into_response(),
    };

    let result = match event_type {
        "Merge Request Hook" => handle_gitlab_merge_request(state, pool, payload).await,
        "Push Hook" => handle_gitlab_push(state, pool, payload).await,
        "Note Hook" => handle_gitlab_note(state, pool, payload).await,
        other => {
            tracing::info!(event = other, platform = "gitlab", "gitlab webhook: event type not handled, persisted only");
            Ok(())
        }
    };

    match result {
        Ok(()) => (StatusCode::ACCEPTED, "accepted").into_response(),
        Err(e) => {
            tracing::error!(error = %e, platform = "gitlab", event = %event_type, "gitlab webhook handler error");
            (StatusCode::INTERNAL_SERVER_ERROR, "handler error").into_response()
        }
    }
}

/// Handle a GitLab Merge Request webhook.
async fn handle_gitlab_merge_request(
    state: &Arc<AppState>,
    pool: &PgPool,
    payload: &Value,
) -> anyhow::Result<()> {
    let project = &payload["project"];
    let mr = &payload["object_attributes"];

    let platform_repo_id = project["id"].as_i64().unwrap_or(0);
    let full_name = project["path_with_namespace"].as_str().unwrap_or("");
    let (namespace, path) = full_name.split_once('/').unwrap_or(("", full_name));
    let default_branch = project["default_branch"].as_str().unwrap_or("main");
    let mr_iid = mr["iid"].as_i64().unwrap_or(0);
    let action = mr["action"].as_str().unwrap_or("");
    let head_sha = mr["last_commit"]["id"].as_str().unwrap_or("");
    let author_login = payload["user"]["username"].as_str().unwrap_or("");

    tracing::info!(
        platform = "gitlab",
        event = "merge_request",
        action = action,
        repo = full_name,
        mr_iid,
        "handling gitlab merge request"
    );

    // Bot detection (GitLab has no user.type field; detect by configured bot username)
    if state.gitlab_bot_login_matches(author_login) {
        tracing::info!(platform = "gitlab", author = author_login, "skipping bot-authored MR");
        return Ok(());
    }

    let repo_ref = RepoRef {
        platform: Platform::GitLab,
        full_name: full_name.to_string(),
        platform_repo_id,
        installation_id: platform_repo_id,  // GitLab uses project_id as installation_id
    };

    match action {
        "open" | "reopen" => {
            // Upsert repository
            let repository_id = db::upsert_repository(
                pool, platform_repo_id, namespace, path, default_branch,
                Some(platform_repo_id),  // installation_id = project_id for GitLab
                Platform::GitLab,  // new parameter
            ).await?;

            // Approval gate
            if !db::repository_approved(pool, repository_id).await? {
                tracing::info!(platform = "gitlab", repo = full_name, "repo not approved, skipping");
                return Ok(());
            }

            // Get base SHA (need to fetch MR details)
            let gitlab = state.gitlab.as_ref().ok_or_else(|| anyhow::anyhow!("gitlab client not configured"))?;
            let (base_sha, head_sha_full) = gitlab.pr_shas(&repo_ref, mr_iid).await?;

            // Create fast review task
            let task = NewTask {
                repository_id,
                installation_id: platform_repo_id,
                webhook_delivery_id: /* delivery_id from context */,
                target_type: "pull_request".to_string(),
                target_id: mr_iid,
                command_text: "review".to_string(),
                base_sha,
                head_sha: head_sha_full,
                run_epoch: 0,
                tier: "fast".to_string(),
                trigger_comment_id: None,
            };
            db::create_task(pool, &task).await?;
            tracing::info!(platform = "gitlab", repo = full_name, mr_iid, "fast review task created");
        }
        "close" | "merge" => {
            let repository_id = db::find_repository_id(pool, platform_repo_id, Platform::GitLab).await?;
            db::cancel_active_tasks_for_pr(pool, repository_id, mr_iid).await?;
            tracing::info!(platform = "gitlab", repo = full_name, mr_iid, "cancelled active tasks");
        }
        _ => {}
    }
    Ok(())
}

/// Handle a GitLab Push webhook (re-index on default branch push).
async fn handle_gitlab_push(
    state: &Arc<AppState>,
    pool: &PgPool,
    payload: &Value,
) -> anyhow::Result<()> {
    let project = &payload["project"];
    let platform_repo_id = project["id"].as_i64().unwrap_or(0);
    let full_name = project["path_with_namespace"].as_str().unwrap_or("");
    let default_branch = project["default_branch"].as_str().unwrap_or("main");
    let ref_name = payload["ref"].as_str().unwrap_or("");
    let deleted = payload["after"].as_str() == Some("0000000000000000000000000000000000000000");

    tracing::info!(
        platform = "gitlab",
        event = "push",
        repo = full_name,
        ref_name,
        "handling gitlab push"
    );

    if deleted { return Ok(()); }
    if ref_name != format!("refs/heads/{}", default_branch) { return Ok(()); }

    let (namespace, path) = full_name.split_once('/').unwrap_or(("", full_name));
    let repository_id = db::upsert_repository(
        pool, platform_repo_id, namespace, path, default_branch,
        Some(platform_repo_id), Platform::GitLab,
    ).await?;

    if !db::repository_approved(pool, repository_id).await? { return Ok(()); }

    db::create_index_task(pool, repository_id, platform_repo_id).await?;
    tracing::info!(platform = "gitlab", repo = full_name, "index task created");
    Ok(())
}

/// Handle a GitLab Note webhook (@mention on MR or Issue).
async fn handle_gitlab_note(
    state: &Arc<AppState>,
    pool: &PgPool,
    payload: &Value,
) -> anyhow::Result<()> {
    let project = &payload["project"];
    let platform_repo_id = project["id"].as_i64().unwrap_or(0);
    let full_name = project["path_with_namespace"].as_str().unwrap_or("");
    let note = &payload["object_attributes"];
    let note_body = note["note"].as_str().unwrap_or("");
    let note_id = note["id"].as_i64().unwrap_or(0);
    let noteable_type = note["noteable_type"].as_str().unwrap_or("");

    // Check for @mention
    let bot_handle = state.gitlab_bot_handle();
    if !mentions_handle(note_body, &bot_handle) {
        return Ok(());
    }

    tracing::info!(
        platform = "gitlab",
        event = "note",
        repo = full_name,
        noteable_type,
        "handling gitlab note (@mention)"
    );

    let (namespace, path) = full_name.split_once('/').unwrap_or(("", full_name));
    let default_branch = project["default_branch"].as_str().unwrap_or("main");
    let repository_id = db::upsert_repository(
        pool, platform_repo_id, namespace, path, default_branch,
        Some(platform_repo_id), Platform::GitLab,
    ).await?;

    if !db::repository_approved(pool, repository_id).await? { return Ok(()); }

    let command = command_from_comment(note_body);

    // Determine target type
    let (target_type, target_id, base_sha, head_sha) = match noteable_type {
        "MergeRequest" => {
            let mr_iid = payload["merge_request"]["iid"].as_i64().unwrap_or(0);
            let gitlab = state.gitlab.as_ref().unwrap();
            let repo_ref = RepoRef {
                platform: Platform::GitLab,
                full_name: full_name.to_string(),
                platform_repo_id,
                installation_id: platform_repo_id,
            };
            let (base, head) = gitlab.pr_shas(&repo_ref, mr_iid).await?;
            ("pull_request", mr_iid, Some(base), Some(head))
        }
        "Issue" => {
            let issue_iid = payload["issue"]["iid"].as_i64().unwrap_or(0);
            ("issue", issue_iid, None, None)
        }
        _ => return Ok(()),
    };

    let task = NewTask {
        repository_id,
        installation_id: platform_repo_id,
        webhook_delivery_id: /* delivery_id */,
        target_type: target_type.to_string(),
        target_id: target_id,
        command_text: command,
        base_sha: base_sha.unwrap_or_default(),
        head_sha: head_sha.unwrap_or_default(),
        run_epoch: 0,
        tier: "deep".to_string(),
        trigger_comment_id: Some(note_id),
    };
    db::create_explicit_task(pool, &task).await?;
    tracing::info!(platform = "gitlab", repo = full_name, target_type, "deep review task created");
    Ok(())
}
```

### 7.2 GitLab bot detection

```rust
impl AppState {
    fn gitlab_bot_handle(&self) -> String {
        self.gitlab.as_ref()
            .map(|g| g.bot_handle.clone())
            .unwrap_or_else(|| "lightbridge-bot".to_string())
    }

    fn gitlab_bot_login_matches(&self, login: &str) -> bool {
        match &self.gitlab {
            Some(g) => {
                // Check by configured bot handle (case-insensitive)
                login.eq_ignore_ascii_case(&g.bot_handle)
                    // Or by bot user ID if configured
                    || g.bot_user_id.map(|id| {
                        // Would need to fetch the user's ID from GitLab API
                        // For now, just check the handle
                        false
                    }).unwrap_or(false)
            }
            None => false,
        }
    }
}
```

### 7.3 Verification

- `cargo test -p control-plane` — all existing tests pass.
- Manual: POST a GitLab Merge Request Hook to `/webhook`, verify a task is created.
- Manual: POST a GitLab Push Hook to `/webhook`, verify an index task is created.
- Manual: POST a GitLab Note Hook with `@lightbridge-bot` to `/webhook`, verify a deep review task is created.

---

## 8. Phase 6 — Review rendering + bot detection (new functionality)

**Goal:** Generalize review rendering to use the platform's bot handle instead of `GITHUB_APP_HANDLE`.

### 8.1 Update `review.rs`

- Replace `GITHUB_APP_HANDLE` references with a platform-aware handle.
- The `render_fast_body()` function takes a `handle: &str` parameter (it already does in the
  current code — just pass the right handle based on the platform).
- Diff scoping: GitLab's diff format (`diff` field in changes response) is similar to GitHub's
  (`patch` field). The diff scoping logic parses unified diff text, which is the same format.

### 8.2 Update `outbox.rs` `Target` struct

```rust
pub struct Target<'a> {
    pub task_id: Option<Uuid>,
    pub platform: Platform,      // new
    pub installation_id: i64,
    pub owner: &'a str,
    pub repo: &'a str,
}
```

### 8.3 Update all callers of `outbox.rs`

Every call to `enqueue_review()`, `enqueue_reply()`, etc. must pass `Platform::GitHub` or
`Platform::GitLab` based on the task's platform. The task's platform is determined by the
repository's platform column.

### 8.4 Verification

- `cargo test -p control-plane` — all existing tests pass.
- Manual: trigger a review for a GitHub repo, verify the rendered body uses the GitHub handle.
- Manual: trigger a review for a GitLab repo, verify the rendered body uses the GitLab handle.

---

## 9. Phase 7 — Testing, docs, and polish

### 9.1 Tests

- Add unit tests for GitLab webhook payload parsing.
- Add unit tests for `detect_platform()`.
- Add integration tests for GitLab webhook handling (POST to `/webhook` with GitLab headers).
- Add integration tests for the outbox with `platform = 'gitlab'`.
- Mock the GitLab API for reconciler tests (or use a local GitLab instance in CI).

### 9.2 Documentation

- Update `explain.md` — mark GitLab as implemented (remove "architectural projection" note).
- Update `local-dev-guide.md` — add GitLab setup instructions (local GitLab in Docker).
- Add an ADR for the platform-abstraction layer.
- Update `docs/gitlab-integration-report.md` — mark as implemented.

### 9.3 Polish

- Add `platform` to the web console UI (show GitHub/GitLab icon per repo).
- Add metrics: `webhook_delivery{platform,event}`, `outbox_delivery{platform,kind,status}`.
- Add a health check for GitLab API connectivity (like the GitHub App key check).

---

## 10. File change manifest

### New files

| File | Purpose |
|------|---------|
| `services/control-plane/src/integrations/platform.rs` | `CodePlatform` trait, `Platform` enum, `RepoRef`, `ReviewPost`, etc. |
| `services/control-plane/src/integrations/gitlab.rs` | `GitlabClient` implementing `CodePlatform` |
| `services/control-plane/migrations/0024_platform_abstraction.sql` | Rename `github_*` → `platform_*`, add `platform` column |
| `docs/adr/0071-platform-abstraction-layer.md` | ADR for the trait + two implementations |

### Modified files

| File | Changes |
|------|---------|
| `services/control-plane/src/main.rs` | AppState fields (add `gitlab`, `gitlab_webhook_secret`), route registration (`/webhook`), readiness check, reconciler startup |
| `services/control-plane/src/http/webhook.rs` | Unified `webhook_router()`, `detect_platform()`, `route_gitlab_event()`, `handle_gitlab_merge_request()`, `handle_gitlab_push()`, `handle_gitlab_note()` |
| `services/control-plane/src/integrations/github.rs` | Add `impl CodePlatform for GithubApp` |
| `services/control-plane/src/outbox.rs` | Add `platform` to `Target`, all `enqueue_*` functions |
| `services/control-plane/src/queue/reconciler.rs` | Take `HashMap<Platform, Arc<dyn CodePlatform>>` instead of `GithubApp`, dispatch by `platform` |
| `services/control-plane/src/db.rs` | Rename all `github_*` references to `platform_*`, add `platform` parameter to functions |
| `services/control-plane/src/review.rs` | Use platform-aware bot handle |
| `services/control-plane/src/config.rs` | Add GitLab config section (optional) |
| `services/control-plane/Cargo.toml` | Add `async-trait` dependency (if not already present) |
| `explain.md` | Update GitLab section from "architectural projection" to "implemented" |
| `local-dev-guide.md` | Update GitLab section with real setup instructions |

### Unchanged files

| File | Why |
|------|-----|
| `services/agent-runner/**` | Entirely platform-agnostic — zero changes |
| `services/config/**` | Platform-agnostic config loader |
| `xtask/**` | CI automation — no platform-specific logic |
| `.github/workflows/**` | CI/CD pipelines — no platform-specific logic |
| `apps/web/**` | May add platform indicator in UI (minor, optional) |
| `compose.yaml` | Local dev dependencies — no GitLab needed (GitLab is optional) |

---

## 11. New env vars and config

### Control plane (serve role)

| Var | Required | Default | Purpose |
|-----|----------|---------|---------|
| `GITLAB_API_URL` | GitLab: yes | — | GitLab API base URL (e.g. `https://gitlab.com/api/v4` or `https://gitlab.example.com/api/v4`) |
| `GITLAB_API_TOKEN` | GitLab: yes | — | Bot account access token (project, group, or personal) |
| `GITLAB_WEBHOOK_SECRET` | GitLab: yes | — | Shared secret for `X-Gitlab-Token` verification |
| `GITLAB_BOT_HANDLE` | no | `lightbridge-bot` | Bot username for @mention detection |
| `GITLAB_BOT_USER_ID` | no | — | Optional: numeric bot user ID for bot detection |

### Readiness check

The readiness check changes from "GitHub webhook secret must be set" to "at least one platform's
webhook secret must be set":

```rust
// At least one platform must be configured
let any_platform = !state.github_webhook_secret.is_empty()
    || !state.gitlab_webhook_secret.is_empty();
if !any_platform {
    return Err("no platform webhook secret configured");
}
```

### Config file (`control-plane.json`)

```json
{
  "platform": {
    "github": {
      "enabled": "{env:GITHUB_APP_ID:-}",
      "app_handle": "{env:GITHUB_APP_HANDLE:-lightbridge-assistant}"
    },
    "gitlab": {
      "enabled": "{env:GITLAB_API_URL:-}",
      "api_url": "{env:GITLAB_API_URL:-}",
      "bot_handle": "{env:GITLAB_BOT_HANDLE:-lightbridge-bot}"
    }
  },
  "agent": { ... },
  "dispatcher": { ... },
  "review": { ... },
  "embeddings": { ... }
}
```

---

## 12. Migration SQL (complete)

```sql
-- 0024_platform_abstraction.sql
-- Platform abstraction: rename GitHub-specific columns and tables.
-- All existing rows get platform = 'github' (the default).

BEGIN;

-- 1. repositories: add platform, rename github_repo_id → platform_repo_id
ALTER TABLE repositories ADD COLUMN IF NOT EXISTS platform TEXT NOT NULL DEFAULT 'github';
ALTER TABLE repositories RENAME COLUMN github_repo_id TO platform_repo_id;

-- Replace the single-column unique with a composite unique
DROP INDEX IF EXISTS repositories_platform_repo_id_key;
CREATE UNIQUE INDEX IF NOT EXISTS repositories_platform_repo_id_unique
    ON repositories (platform, platform_repo_id);

-- 2. tasks: rename github_delivery_id → webhook_delivery_id
ALTER TABLE tasks RENAME COLUMN github_delivery_id TO webhook_delivery_id;

-- 3. github_deliveries → webhook_deliveries
ALTER TABLE github_deliveries RENAME TO webhook_deliveries;
ALTER TABLE webhook_deliveries ADD COLUMN IF NOT EXISTS platform TEXT NOT NULL DEFAULT 'github';

-- 4. github_outbox → outbox
ALTER TABLE github_outbox RENAME TO outbox;
ALTER TABLE outbox ADD COLUMN IF NOT EXISTS platform TEXT NOT NULL DEFAULT 'github';
ALTER TABLE outbox RENAME COLUMN github_id TO platform_ref_id;

-- 5. reviews: rename github_review_id → platform_review_id
ALTER TABLE reviews RENAME COLUMN github_review_id TO platform_review_id;

-- 6. review_comments: rename github_comment_id → platform_comment_id
ALTER TABLE review_comments RENAME COLUMN github_comment_id TO platform_comment_id;

-- 7. review_feedback: add platform
ALTER TABLE review_feedback ADD COLUMN IF NOT EXISTS platform TEXT NOT NULL DEFAULT 'github';

COMMIT;
```

---

## 13. Rollout checklist

### Phase 0 — Platform trait (no behavior change)
- [ ] Create `integrations/platform.rs` with trait + types
- [ ] Add `impl CodePlatform for GithubApp` in `integrations/github.rs`
- [ ] Move `verify_signature` logic into the trait impl
- [ ] `cargo test -p control-plane` passes
- [ ] `cargo clippy -D warnings` passes
- [ ] Deploy: GitHub behavior unchanged

### Phase 1 — Database migration (no behavior change)
- [ ] Create migration `0024_platform_abstraction.sql`
- [ ] Update all SQL in `db.rs` (mechanical rename)
- [ ] Update `NewTask` struct: `github_delivery_id` → `webhook_delivery_id`
- [ ] Update `OutboxRow` struct: add `platform` field
- [ ] Update `outbox.rs`: add `platform` to `Target`, all `enqueue_*` functions
- [ ] Update all callers of `outbox.rs` to pass `Platform::GitHub`
- [ ] Update frontend types: `github_delivery_id` → `webhook_delivery_id`
- [ ] Update frontend UI: use `webhook_delivery_id` field
- [ ] `#[sqlx::test]` integration tests pass against live pgvector
- [ ] Deploy: migration runs automatically, all existing data gets `platform = 'github'`

### Phase 2 — Unified webhook route (no behavior change)
- [ ] Add `detect_platform()` function
- [ ] Add `webhook_router()` unified handler
- [ ] Register `/webhook` route in `main.rs`
- [ ] Keep `/github/webhook` as legacy alias
- [ ] Update readiness check: at least one platform configured
- [ ] `cargo test -p control-plane` passes
- [ ] Deploy: GitHub webhooks work on both `/webhook` and `/github/webhook`

### Phase 3 — Unified outbox + reconciler (no behavior change)
- [ ] Generalize `reconciler::run()` to take `HashMap<Platform, Arc<dyn CodePlatform>>`
- [ ] Generalize `drain_once()` to dispatch by `platform` column
- [ ] Generalize `poll_once()` (feedback) to dispatch by `platform`
- [ ] Update `main.rs` reconciler startup
- [ ] `cargo test -p control-plane` passes
- [ ] Deploy: GitHub outbox drains via trait dispatch, logs include `platform=github`

### Phase 4 — GitLab integration module (new functionality)
- [ ] Create `integrations/gitlab.rs` with `GitlabClient`
- [ ] Implement all `CodePlatform` trait methods
- [ ] Add `GitlabClient::from_env()`
- [ ] Update `AppState::from_env()` to construct `GitlabClient`
- [ ] Update `AppState` with `gitlab` and `gitlab_webhook_secret` fields
- [ ] `cargo build -p control-plane` compiles
- [ ] `cargo test -p control-plane` passes

### Phase 5 — GitLab webhook handler (new functionality)
- [ ] Add `route_gitlab_event()` in `webhook.rs`
- [ ] Add `handle_gitlab_merge_request()`
- [ ] Add `handle_gitlab_push()`
- [ ] Add `handle_gitlab_note()`
- [ ] Add GitLab bot detection
- [ ] Manual test: POST GitLab MR webhook → task created
- [ ] Manual test: POST GitLab Push webhook → index task created
- [ ] Manual test: POST GitLab Note webhook with @mention → deep review task created

### Phase 6 — Review rendering + bot detection (new functionality)
- [ ] Update `review.rs` to use platform-aware bot handle
- [ ] Update `outbox.rs` `Target` struct with `platform` field
- [ ] Update all callers to pass the correct platform
- [ ] Manual test: GitLab review renders with GitLab bot handle

### Phase 7 — Testing, docs, and polish
- [ ] Add unit tests for GitLab webhook parsing
- [ ] Add integration tests for GitLab webhook → task flow
- [ ] Add metrics with `platform` label
- [ ] Update `explain.md`
- [ ] Update `local-dev-guide.md`
- [ ] Add ADR for platform abstraction
- [ ] Full CI gate: `just ci` passes