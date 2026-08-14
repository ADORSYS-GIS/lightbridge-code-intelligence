//! Repository CRUD + the approval-gate lifecycle (Epic #75): upsert on webhook sight, pending
//! registration, approve/disable transitions, and the A2A slug lookup (RFC-0006). Split out of the
//! former monolithic `db.rs` (ADR-0086 follow-up) — pure move, no behavior change.

use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::integrations::platform::Platform;

use super::TASK_QUEUED_CHANNEL;
use super::tasks::NewTask;

/// Insert or update a repository by its GitHub id; returns the local `repositories.id`.
/// `installation_id` is recorded when known (`Some`) and preserved otherwise (`COALESCE`), so the
/// index-on-approve path can mint a token for it. Status is never touched here (the approval gate
/// owns it).
pub async fn upsert_repository(
    pool: &PgPool,
    platform: Platform,
    platform_repo_id: i64,
    owner: &str,
    name: &str,
    default_branch: &str,
    installation_id: Option<i64>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO repositories (platform, platform_repo_id, owner, name, default_branch, installation_id) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (platform, platform_repo_id) DO UPDATE \
           SET owner = EXCLUDED.owner, name = EXCLUDED.name, \
               default_branch = EXCLUDED.default_branch, \
               installation_id = COALESCE(EXCLUDED.installation_id, repositories.installation_id) \
         RETURNING id",
    )
    .bind(platform)
    .bind(platform_repo_id)
    .bind(owner)
    .bind(name)
    .bind(default_branch)
    .bind(installation_id)
    .fetch_one(pool)
    .await
}

/// A connected repository for the dashboard's Repositories view (ADR-0016), with a small activity
/// summary (run count + most-recent run) derived from `tasks`. RepoIndex health is not joined yet —
/// `repo_index` has no writer today (snapshot readiness is tracked via `code_chunks` /
/// `latest_indexed_commit`, ADR-0050); it is intentionally reserved for the `ready`-row-per-commit
/// gate described in [ADR-0055](../../../../docs/adr/0055-review-waits-for-index-readiness.md)'s
/// "Failed/partial index" follow-up (RFC-0002), not dead schema (#245).
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct RepositoryRow {
    pub id: i64,
    pub platform_repo_id: i64,
    pub platform: Platform,
    pub owner: String,
    pub name: String,
    pub default_branch: String,
    /// Approval gate (Epic #75): `pending` | `approved` | `disabled`. `active` mirrors
    /// `status = 'approved'` for the existing dashboard; `status` is the source of truth.
    pub status: String,
    pub active: bool,
    #[serde(with = "time::serde::rfc3339::option")]
    pub approved_at: Option<OffsetDateTime>,
    pub approved_by: Option<String>,
    pub task_count: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_task_at: Option<OffsetDateTime>,
}

/// Connected repositories, most-recently-active first, optionally filtered by approval `status`
/// (e.g. `Some("pending")` for the admin approval queue). Aggregates each repo's task activity in one
/// query so the list needs no per-row round-trip.
pub async fn list_repositories(
    pool: &PgPool,
    status: Option<&str>,
) -> Result<Vec<RepositoryRow>, sqlx::Error> {
    sqlx::query_as::<_, RepositoryRow>(
        "SELECT r.id, r.platform_repo_id, r.platform, r.owner, r.name, r.default_branch, r.status, \
           (r.status = 'approved') AS active, r.approved_at, r.approved_by, \
           COUNT(t.id) AS task_count, MAX(t.created_at) AS last_task_at \
         FROM repositories r LEFT JOIN tasks t ON t.repository_id = r.id \
         WHERE ($1::text IS NULL OR r.status = $1) \
         GROUP BY r.id \
         ORDER BY last_task_at DESC NULLS LAST, r.owner, r.name",
    )
    .bind(status)
    .fetch_all(pool)
    .await
}

/// A page boundary for [`list_repositories_page`]: the `(last_task_at, id)` of the row it points
/// at, and which direction to continue from there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryCursor {
    /// Continue forward from the bottom of a previous page.
    After(OffsetDateTime, i64),
    /// Continue backward from the top of a previous page.
    Before(OffsetDateTime, i64),
}

/// One page of [`list_repositories_page`]: the rows in display order (most-recently-active first),
/// the count of every repository matching `q` regardless of page (for a "1–12 of 357" label), and
/// the boundary to continue in each direction — `None` at either edge of the list.
pub struct RepositoryPage {
    pub rows: Vec<RepositoryRow>,
    pub total: i64,
    pub next: Option<(OffsetDateTime, i64)>,
    pub prev: Option<(OffsetDateTime, i64)>,
}

/// One page of connected repositories, most-recently-active first. `cursor` continues from a
/// previous page in either direction; `None` starts at the first page. `q` matches `owner/name`,
/// case-insensitively.
///
/// The ordering is `repositories.last_task_at` (migration 0039), a stored column the keyset
/// predicate can seek on. Never-run repositories carry the `'epoch'` sentinel so the comparison is
/// total, and the projection maps it back to NULL — the wire keeps reporting "no runs yet" as an
/// absent timestamp.
///
/// The final tie-break is `id`, not [`list_repositories`]'s `owner, name`: a page boundary needs a
/// unique key, and every never-run repository shares the same `last_task_at`.
pub async fn list_repositories_page(
    pool: &PgPool,
    q: Option<&str>,
    cursor: Option<RepositoryCursor>,
    page_size: i64,
) -> Result<RepositoryPage, sqlx::Error> {
    // Independent of the cursor — it is the "of N" in a range label, not a page-relative count, so
    // it does not shrink as the cursor advances.
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM repositories r \
         WHERE ($1::text IS NULL OR r.owner || '/' || r.name ILIKE '%' || $1 || '%')",
    )
    .bind(q)
    .fetch_one(pool)
    .await?;

    // One row past the page, in whichever direction this query scans, so "does another page exist
    // that way" is a fact rather than a guess from a full page.
    let (mut rows, has_more) = if let Some(RepositoryCursor::Before(activity_at, id)) = cursor {
        let rows = sqlx::query_as::<_, RepositoryRow>(
            "SELECT r.id, r.platform_repo_id, r.platform, r.owner, r.name, r.default_branch, r.status, \
               (r.status = 'approved') AS active, r.approved_at, r.approved_by, \
               c.task_count, NULLIF(r.last_task_at, 'epoch') AS last_task_at \
             FROM repositories r \
             LEFT JOIN LATERAL ( \
               SELECT COUNT(*) AS task_count FROM tasks t WHERE t.repository_id = r.id \
             ) c ON TRUE \
             WHERE ($1::text IS NULL OR r.owner || '/' || r.name ILIKE '%' || $1 || '%') \
               AND (r.last_task_at, r.id) > ($2, $3) \
             ORDER BY r.last_task_at ASC, r.id ASC \
             LIMIT $4",
        )
        .bind(q)
        .bind(activity_at)
        .bind(id)
        .bind(page_size + 1)
        .fetch_all(pool)
        .await?;
        let has_more = rows.len() as i64 > page_size;
        (rows, has_more)
    } else {
        let after = match cursor {
            Some(RepositoryCursor::After(activity_at, id)) => Some((activity_at, id)),
            _ => None,
        };
        let rows = sqlx::query_as::<_, RepositoryRow>(
            "SELECT r.id, r.platform_repo_id, r.platform, r.owner, r.name, r.default_branch, r.status, \
               (r.status = 'approved') AS active, r.approved_at, r.approved_by, \
               c.task_count, NULLIF(r.last_task_at, 'epoch') AS last_task_at \
             FROM repositories r \
             LEFT JOIN LATERAL ( \
               SELECT COUNT(*) AS task_count FROM tasks t WHERE t.repository_id = r.id \
             ) c ON TRUE \
             WHERE ($1::text IS NULL OR r.owner || '/' || r.name ILIKE '%' || $1 || '%') \
               AND ($2::timestamptz IS NULL OR (r.last_task_at, r.id) < ($2, $3)) \
             ORDER BY r.last_task_at DESC, r.id DESC \
             LIMIT $4",
        )
        .bind(q)
        .bind(after.map(|(activity_at, _)| activity_at))
        .bind(after.map(|(_, id)| id))
        .bind(page_size + 1)
        .fetch_all(pool)
        .await?;
        let has_more = rows.len() as i64 > page_size;
        (rows, has_more)
    };

    rows.truncate(page_size as usize);
    if matches!(cursor, Some(RepositoryCursor::Before(..))) {
        rows.reverse(); // ascending scan order back to newest-first display order
    }

    // Paging in a direction always implies a page exists on the *other* side too — the one just
    // navigated from — so only the side actually scanned needs the extra-row probe above.
    let (has_next, has_prev) = match cursor {
        None => (has_more, false),
        Some(RepositoryCursor::After(..)) => (has_more, true),
        Some(RepositoryCursor::Before(..)) => (true, has_more),
    };
    // Back to the sentinel the ordering uses, so paging across the never-run group resumes inside
    // it rather than restarting at its head.
    let next = has_next.then(|| rows.last()).flatten().map(|row| {
        (
            row.last_task_at.unwrap_or(OffsetDateTime::UNIX_EPOCH),
            row.id,
        )
    });
    let prev = has_prev.then(|| rows.first()).flatten().map(|row| {
        (
            row.last_task_at.unwrap_or(OffsetDateTime::UNIX_EPOCH),
            row.id,
        )
    });

    Ok(RepositoryPage {
        rows,
        total,
        next,
        prev,
    })
}

/// Register a repository seen via an `installation` / `installation_repositories` webhook as
/// **pending** approval. New repo → inserted pending. A previously **disabled** repo (uninstalled
/// then re-added) is re-opened to pending so the admin sees it in the queue again; an already
/// `approved`/`pending` repo is left untouched (the `WHERE` guard), preserving its status and real
/// `default_branch`. The installation payload carries no `default_branch`, so a placeholder is fine —
/// the first PR webhook fills it in. Returns `true` when a row was inserted or re-pended.
pub async fn register_pending_repository(
    pool: &PgPool,
    platform: Platform,
    platform_repo_id: i64,
    owner: &str,
    name: &str,
    default_branch: &str,
    installation_id: Option<i64>,
) -> Result<bool, sqlx::Error> {
    let affected = sqlx::query(
        "INSERT INTO repositories (platform, platform_repo_id, owner, name, default_branch, installation_id, status) \
         VALUES ($1, $2, $3, $4, $5, $6, 'pending') \
         ON CONFLICT (platform, platform_repo_id) DO UPDATE \
           SET status = 'pending', owner = EXCLUDED.owner, name = EXCLUDED.name, \
               installation_id = COALESCE(EXCLUDED.installation_id, repositories.installation_id) \
           WHERE repositories.status = 'disabled'",
    )
    .bind(platform)
    .bind(platform_repo_id)
    .bind(owner)
    .bind(name)
    .bind(default_branch)
    .bind(installation_id)
    .execute(pool)
    .await?;
    Ok(affected.rows_affected() > 0)
}

/// A single repository by its control-plane id, or `None` if no such repo. Read-only — unlike
/// [`set_repository_status_by_id`], which is approve/deny's own mutate-then-reselect helper. Used by
/// story #500's preset-write endpoint to resolve the repo's platform/owner/name/default_branch before
/// picking a `CodePlatform` client.
pub async fn get_repository_by_id(
    pool: &PgPool,
    id: i64,
) -> Result<Option<RepositoryRow>, sqlx::Error> {
    sqlx::query_as::<_, RepositoryRow>(
        "SELECT r.id, r.platform_repo_id, r.platform, r.owner, r.name, r.default_branch, r.status, \
           (r.status = 'approved') AS active, r.approved_at, r.approved_by, \
           COUNT(t.id) AS task_count, MAX(t.created_at) AS last_task_at \
         FROM repositories r LEFT JOIN tasks t ON t.repository_id = r.id \
         WHERE r.id = $1 GROUP BY r.id",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// A repository's approval status (`pending`/`approved`/`disabled`), or `None` if no such repo.
pub async fn repository_status(
    pool: &PgPool,
    repository_id: i64,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT status FROM repositories WHERE id = $1")
        .bind(repository_id)
        .fetch_optional(pool)
        .await
}

/// Is this repository approved for work? The gate the webhook handlers check before creating any
/// review/index task. A missing repo (shouldn't happen — callers upsert first) reads as not approved.
pub async fn repository_approved(pool: &PgPool, repository_id: i64) -> Result<bool, sqlx::Error> {
    Ok(repository_status(pool, repository_id).await?.as_deref() == Some("approved"))
}

/// Persist a repository's resolved default branch (e.g. fetched at approval time for a repo first
/// seen via an installation webhook, which doesn't carry it).
pub async fn update_repository_default_branch(
    pool: &PgPool,
    repository_id: i64,
    default_branch: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE repositories SET default_branch = $2 WHERE id = $1")
        .bind(repository_id)
        .bind(default_branch)
        .execute(pool)
        .await
        .map(|_| ())
}

/// The repository's GitHub `installation_id` (for minting a clone token), or `None` if not recorded.
pub async fn repository_installation_id(
    pool: &PgPool,
    repository_id: i64,
) -> Result<Option<i64>, sqlx::Error> {
    let id: Option<Option<i64>> =
        sqlx::query_scalar("SELECT installation_id FROM repositories WHERE id = $1")
            .bind(repository_id)
            .fetch_optional(pool)
            .await?;
    Ok(id.flatten())
}

/// A repository resolved by its platform + `owner/name` slug for an A2A review submission
/// (RFC-0006). Carries just what the submission path needs to enforce the approval gate and build
/// the review task **without a forge round-trip** (the `a2a` role holds no forge credentials).
#[derive(Debug, sqlx::FromRow)]
pub struct RepoForReview {
    pub id: i64,
    /// GitHub App installation id (or GitLab project id). `None` for a repo seen but never carried
    /// through a PR/installation webhook that recorded it — the A2A path treats that as not runnable.
    pub installation_id: Option<i64>,
    /// Approval status: `pending` | `approved` | `disabled`. The A2A path rejects anything but
    /// `approved` (the same gate as the webhook path, ADR-0063).
    pub status: String,
}

/// Resolve an approved-or-not repository by platform + `owner`/`name`. `None` when no such repo has
/// ever been connected (an A2A review of a never-seen repo → `TASK_STATE_REJECTED`, never a side
/// door around the approval gate). Case-insensitive on owner/name to match forge slug behaviour.
pub async fn find_repository(
    pool: &PgPool,
    platform: Platform,
    owner: &str,
    name: &str,
) -> Result<Option<RepoForReview>, sqlx::Error> {
    sqlx::query_as::<_, RepoForReview>(
        "SELECT id, installation_id, status FROM repositories \
         WHERE platform = $1 AND lower(owner) = lower($2) AND lower(name) = lower($3)",
    )
    .bind(platform)
    .bind(owner)
    .bind(name)
    .fetch_optional(pool)
    .await
}

/// Find the id of an already-existing task matching a [`NewTask`]'s idempotency tuple
/// (`repository_id, target_type, target_id, command_text, head_sha, run_epoch`) — the same columns as
/// `tasks_idempotency_idx`, `NULLS NOT DISTINCT` on `head_sha`. Used by the A2A review path so that
/// when [`create_task`] dedups (returns `None`), the A2A task can still be mapped onto the existing
/// underlying run instead of forking the idempotency logic.
pub async fn find_task_id_by_idempotency(
    pool: &PgPool,
    task: &NewTask,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT id FROM tasks \
         WHERE repository_id = $1 AND target_type = $2 AND target_id = $3 \
           AND command_text = $4 AND head_sha IS NOT DISTINCT FROM $5 AND run_epoch = $6 \
         ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(task.repository_id)
    .bind(&task.target_type)
    .bind(task.target_id)
    .bind(&task.command_text)
    .bind(&task.head_sha)
    .bind(task.run_epoch)
    .fetch_optional(pool)
    .await
}

/// Enqueue a standalone **index** task for a repository's default branch (Epic #75, Milestone B —
/// runs on admin approval, and on every default-branch push via `handle_push`). Skips if an index task
/// is already active for the repo (so a burst of pushes / a re-approve doesn't pile up duplicates).
/// Returns the new task id, or `None` if one was already pending/running. Unlike review tasks it has no
/// originating delivery (`webhook_delivery_id` NULL) and no SHA (the runner indexes the default-branch
/// HEAD).
///
/// `run_epoch` is computed as `MAX+1` over the same columns as `tasks_idempotency_idx` (minus
/// `run_epoch`), exactly like [`create_explicit_task`]. This is **load-bearing**: an index task carries
/// a NULL `head_sha`, so every re-index of a repo shares the idempotency tuple
/// `(repo, 'repository', repo, 'index', NULL, run_epoch)` — and `tasks_idempotency_idx` is
/// `NULLS NOT DISTINCT`. Hardcoding `run_epoch = 0` (the original bug) meant the *second* index for a
/// repo passed the `NOT EXISTS` active-guard (the first index was terminal, not active) but then
/// **collided on the unique index** → `duplicate key` error → a repo was only ever indexed once.
pub async fn create_index_task(
    pool: &PgPool,
    repository_id: i64,
    installation_id: i64,
) -> Result<Option<Uuid>, sqlx::Error> {
    let id = Uuid::new_v4();
    let inserted = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO tasks (id, repository_id, installation_id, target_type, target_id, \
         command_text, run_epoch, status) \
         SELECT $1, $2, $3, 'repository', $2, 'index', \
           (SELECT COALESCE(MAX(run_epoch), -1) + 1 FROM tasks \
              WHERE repository_id = $2 AND target_type = 'repository' AND target_id = $2 \
                AND command_text = 'index' AND head_sha IS NULL), \
           'queued' \
         WHERE NOT EXISTS ( \
           SELECT 1 FROM tasks WHERE repository_id = $2 AND command_text = 'index' \
             AND status IN ('queued', 'running', 'posting_result') \
         ) \
         RETURNING id",
    )
    .bind(id)
    .bind(repository_id)
    .bind(installation_id)
    .fetch_optional(pool)
    .await;

    // A concurrent push that cleared the `NOT EXISTS` guard at the same instant can race us to the same
    // computed epoch and trip `tasks_idempotency_idx` — that's a benign dedup (the other push queued the
    // index), not an error to surface.
    let inserted = match inserted {
        Ok(v) => v,
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => None,
        Err(e) => return Err(e),
    };

    if let Some(new_id) = inserted {
        let _ = sqlx::query("SELECT pg_notify($1, $2)")
            .bind(TASK_QUEUED_CHANNEL)
            .bind(new_id.to_string())
            .execute(pool)
            .await;
    }
    Ok(inserted)
}

/// Set a repository's approval status by its **GitHub** id (webhook path — e.g. mark `disabled` when
/// removed from the installation). Returns the repo's **local** id (so the caller can purge its index
/// data), or `None` if the repo isn't known locally.
pub async fn set_repository_status_by_platform_id(
    pool: &PgPool,
    platform: Platform,
    platform_repo_id: i64,
    status: &str,
) -> Result<Option<i64>, sqlx::Error> {
    // Clear the approval audit on any non-approved transition (e.g. disable) so stale approver/time
    // don't linger — mirrors `set_repository_status_by_id`.
    sqlx::query_scalar(
        "UPDATE repositories SET status = $3, \
           approved_at = CASE WHEN $3 = 'approved' THEN approved_at ELSE NULL END, \
           approved_by = CASE WHEN $3 = 'approved' THEN approved_by ELSE NULL END \
         WHERE platform_repo_id = $2 AND platform = $1 \
         RETURNING id",
    )
    .bind(platform)
    .bind(platform_repo_id)
    .bind(status)
    .fetch_optional(pool)
    .await
}

/// Admin action: set a repository's approval status by its **local** id, recording who/when on
/// approval. Returns the updated row, or `None` if no such repo. `approved_by` is the admin's
/// identity (OIDC subject/username); cleared for non-approved transitions.
pub async fn set_repository_status_by_id(
    pool: &PgPool,
    id: i64,
    status: &str,
    approved_by: Option<&str>,
) -> Result<Option<RepositoryRow>, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE repositories SET status = $2, \
           approved_at = CASE WHEN $2 = 'approved' THEN now() ELSE NULL END, \
           approved_by = CASE WHEN $2 = 'approved' THEN $3 ELSE NULL END \
         WHERE id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(approved_by)
    .execute(pool)
    .await?;
    if updated.rows_affected() == 0 {
        return Ok(None);
    }
    sqlx::query_as::<_, RepositoryRow>(
        "SELECT r.id, r.platform_repo_id, r.platform, r.owner, r.name, r.default_branch, r.status, \
           (r.status = 'approved') AS active, r.approved_at, r.approved_by, \
           COUNT(t.id) AS task_count, MAX(t.created_at) AS last_task_at \
         FROM repositories r LEFT JOIN tasks t ON t.repository_id = r.id \
         WHERE r.id = $1 GROUP BY r.id",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}
