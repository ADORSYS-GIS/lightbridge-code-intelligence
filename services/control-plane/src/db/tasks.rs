//! Task lifecycle: rows, creation (webhook + explicit `@mention`), cancellation, dispatch claiming,
//! leasing, status transitions (including the ADR-0055 index-readiness gate), and the runner's
//! execution-context read. Split out of the former monolithic `db.rs` (ADR-0086 follow-up) — pure
//! move, no behavior change.

use std::time::Duration;

use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::integrations::platform::Platform;

use super::TASK_QUEUED_CHANNEL;

/// A task row as stored — one task run for the dashboard (ADR-0016). Serialized directly to the
/// `/tasks` API (timestamps as RFC 3339). The `repo_*` fields are joined from `repositories` so the
/// dashboard can show a human repo name + branch without a second round-trip (LEFT JOIN, so they're
/// `None` for the rare orphaned row).
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct TaskRow {
    pub id: Uuid,
    pub repository_id: i64,
    pub installation_id: i64,
    /// `None` for admin-initiated tasks (e.g. index-on-approve) that have no originating webhook
    /// delivery; `Some` for webhook-created tasks. (Column is nullable since migration 0008.)
    pub webhook_delivery_id: Option<String>,
    pub target_type: String,
    pub target_id: i64,
    pub command_text: String,
    /// Run kind (ADR-0033): `review` (diff-scoped findings) or `ask` (a conversational answer).
    /// Defaults to `review` for rows created before migration 0011.
    pub kind: String,
    pub base_sha: Option<String>,
    pub head_sha: Option<String>,
    pub status: String,
    pub priority: i32,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub started_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub completed_at: Option<OffsetDateTime>,
    pub repo_owner: Option<String>,
    pub repo_name: Option<String>,
    pub repo_default_branch: Option<String>,
    /// The platform the task's repository lives on (`github`/`gitlab`), surfaced from the joined
    /// `repositories.platform` so the dashboard can build platform-correct deep links (MR vs PR,
    /// GitLab project URL vs GitHub). Added by branch `frontend/gitlab-repo-url`; ported here from
    /// the retired `db.rs` during the main→branch merge (main split `db.rs` into `db/`).
    pub repo_platform: Option<Platform>,
    /// The Kubernetes Job name (set once dispatched), so the console can stream the run's logs. `None`
    /// before dispatch or after the Job is reaped/TTL'd.
    pub job_name: Option<String>,
    /// The runner's free-text status `detail` (#137), persisted on the last status report that carried
    /// one. `None` for a genuine clean success / runs that predate migration 0016; `Some` records why
    /// a run did not post a review (e.g. a failure reason, or a "posted nothing" no-op), so the console
    /// can tell a silent no-op apart from a real clean review.
    pub error_detail: Option<String>,
}

/// Fields needed to create a task from a webhook event.
pub struct NewTask {
    pub repository_id: i64,
    pub installation_id: i64,
    pub webhook_delivery_id: String,
    pub target_type: String,
    pub target_id: i64,
    pub command_text: String,
    pub base_sha: Option<String>,
    pub head_sha: Option<String>,
    /// Re-run discriminator (RFC-0001), used by the content-idempotent [`create_task`] path (the
    /// automatic first review uses `0`). The explicit `@mention` path goes through
    /// [`create_explicit_task`], which computes the epoch inside the INSERT and ignores this field.
    pub run_epoch: i32,
    /// Resolved review preset (ADR-0103), e.g. `"fast"`/`"deep"`/`"ultra"` or an operator-defined
    /// name — resolved from repo config per entry point before the task is created
    /// (`review::preset::resolve_preset`). The runner reads it from the task context. Index tasks
    /// don't set it (the column defaults to `deep`, ignored).
    pub preset: String,
    /// Which entry point created this task (`"pr_open"`/`"mention"`/`"a2a"`) — kept separate from
    /// `preset` because preset names are operator-defined and can't be relied on to signal intent
    /// (e.g. "was this an automatic open-PR pass"). Index tasks don't set it meaningfully (defaults to
    /// `mention`, ignored).
    pub entry_point: String,
    /// GitHub id of the `@mention` comment that triggered this task (ADR-0068), so the lifecycle
    /// reactions target the triggering comment. `None` for the automatic `pull_request opened` review
    /// (no trigger comment → the reactions land on the PR body) and for index tasks.
    pub trigger_comment_id: Option<i64>,
    /// W3C `traceparent` of the webhook-receipt span that created this task (ticket #246), captured via
    /// `lci_observability::current_traceparent()`. Persisted (not held in memory) so it survives the
    /// dispatcher's queueing delay; read back at dispatch time to parent the Job's own trace.
    pub trace_context: Option<String>,
    /// Resolved repo/org model override (ADR-0110, story #501) — `crate::model::resolve_model_override`,
    /// called at the same task-creation sites as preset resolution. `None` when neither a repo nor an
    /// org override is set; the runner then applies the preset's own configured model unchanged.
    pub model_override: Option<String>,
    /// Resolved `check_run_reporting` setting (epic #566), SNAPSHOTTED here rather than re-read at each
    /// use: the check's start and resolve happen minutes apart and must agree, so an operator flipping
    /// the toggle mid-run must not strand an in-progress check on the PR.
    pub check_runs_enabled: bool,
    /// Delay before the task becomes claimable (epic #566's `debounce` push-storm strategy) — bound to
    /// `run_after = now() + this many seconds`. `None`/`Some(0)` means claimable immediately (today's
    /// behavior for every existing caller). Only [`create_task`] honors this; [`create_explicit_task`]
    /// (the `@mention` path) always runs immediately — a human asking for a review should never wait
    /// out someone else's debounce window.
    pub run_after_secs: Option<u64>,
}

/// A task claimed by the dispatcher for execution (the subset needed to launch its Job).
#[derive(Debug, sqlx::FromRow)]
pub struct ClaimedTask {
    pub id: Uuid,
    pub repository_id: i64,
    pub installation_id: i64,
    pub target_type: String,
    pub target_id: i64,
    pub command_text: String,
    pub base_sha: Option<String>,
    pub head_sha: Option<String>,
    pub attempts: i32,
    /// See [`NewTask::trace_context`] — carried through so [`crate::integrations::k8s::KubeLauncher`]
    /// can re-parent the Job's trace under it.
    pub trace_context: Option<String>,
}
/// Initial-status SQL for a newly inserted task (ADR-0055 index-readiness gate). A **non-`index`**
/// task (review / ask — anything that reads the index) starts `waiting_for_index` when an `index` task
/// is **in flight** for the repo, so it isn't claimed against a half-written snapshot; otherwise it
/// starts `queued`. Index tasks themselves never wait. Spliced where the bound params are
/// `$2 = repository_id` and `$7 = command_text` (true for both [`create_task`] and
/// [`create_explicit_task`]); a `set`-time `EXISTS` is fine — the dispatcher and the
/// release in [`set_task_status`] handle the (repo, index) lifecycle, not this insert.
const INITIAL_TASK_STATUS_SQL: &str = "CASE WHEN $7 <> 'index' AND EXISTS ( \
        SELECT 1 FROM tasks ix WHERE ix.repository_id = $2 AND ix.command_text = 'index' \
          AND ix.status IN ('queued', 'running', 'posting_result', 'waiting_for_index')) \
     THEN 'waiting_for_index' ELSE 'queued' END";

/// After inserting a task: wake a listening dispatcher when it's claimable (`queued`), or log that it
/// was parked behind an in-flight index (`waiting_for_index`, ADR-0055). A waiting task is woken later
/// by [`set_task_status`] when the repo's index task completes — notifying now would be a no-op (the
/// claim query only selects `queued`).
async fn notify_or_log_initial_status(pool: &PgPool, id: Uuid, repository_id: i64, status: &str) {
    if status == "waiting_for_index" {
        tracing::info!(
            task_id = %id, repository_id,
            "review gated: WaitingForIndex — an index task is in flight; will run once it completes (ADR-0055)"
        );
        return;
    }
    // Wake a listening dispatcher; harmless if none is connected (the dispatcher also polls).
    let _ = sqlx::query("SELECT pg_notify($1, $2)")
        .bind(TASK_QUEUED_CHANNEL)
        .bind(id.to_string())
        .execute(pool)
        .await;
}

/// Enqueue a task idempotently. Returns the new task id, or `None` when an equivalent task already
/// exists — GitHub can deliver several events for one PR head (e.g. `opened` then `synchronize`),
/// and the `tasks_idempotency_idx` unique index collapses those to a single task. On a real insert,
/// the task starts `queued` (→ notifies [`TASK_QUEUED_CHANNEL`]) or `waiting_for_index` when the repo
/// is mid-index (ADR-0055).
pub async fn create_task(pool: &PgPool, task: &NewTask) -> Result<Option<Uuid>, sqlx::Error> {
    let id = Uuid::new_v4();
    let inserted: Option<(Uuid, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO tasks (id, repository_id, installation_id, webhook_delivery_id, target_type, \
         target_id, command_text, base_sha, head_sha, run_epoch, preset, entry_point, \
         trigger_comment_id, trace_context, model_override, check_runs_enabled, run_after, status) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, \
                 now() + ($17 * interval '1 second'), {INITIAL_TASK_STATUS_SQL}) \
         ON CONFLICT (repository_id, target_type, target_id, command_text, head_sha, run_epoch) \
         DO NOTHING \
         RETURNING id, status"
    )))
    .bind(id)
    .bind(task.repository_id)
    .bind(task.installation_id)
    .bind(&task.webhook_delivery_id)
    .bind(&task.target_type)
    .bind(task.target_id)
    .bind(&task.command_text)
    .bind(&task.base_sha)
    .bind(&task.head_sha)
    .bind(task.run_epoch)
    .bind(&task.preset)
    .bind(&task.entry_point)
    .bind(task.trigger_comment_id)
    .bind(&task.trace_context)
    .bind(&task.model_override)
    .bind(task.check_runs_enabled)
    .bind(task.run_after_secs.unwrap_or(0) as i64)
    .fetch_optional(pool)
    .await?;

    if let Some((new_id, status)) = &inserted {
        // A debounced task is not yet claimable — waking the dispatcher now would be a no-op (the
        // claim query filters on `run_after <= now()`) and just adds needless churn; the poll
        // fallback picks it up once its delay elapses.
        let delayed = task.run_after_secs.is_some_and(|s| s > 0);
        if delayed && status == "queued" {
            tracing::debug!(
                task_id = %new_id, delay_secs = task.run_after_secs,
                "task created with a debounce delay; skipping the immediate wake"
            );
        } else {
            notify_or_log_initial_status(pool, *new_id, task.repository_id, status).await;
        }
    }
    Ok(inserted.map(|(id, _)| id))
}

/// Enqueue an **explicit human command** (an `@mention`), which must ALWAYS land a task — never
/// content-deduped. True webhook redeliveries are already collapsed upstream by the
/// `webhook_deliveries` PRIMARY KEY, so content-idempotency on this path adds nothing and only drops
/// legitimate re-requests.
///
/// The `run_epoch` is folded into the INSERT — `COALESCE(MAX(run_epoch), -1) + 1` over the SAME
/// columns as the idempotency index (minus `run_epoch`), using the REAL `command_text` — so it is
/// computed and consumed in a single statement.
///
/// That subquery+insert is NOT atomic under READ COMMITTED: two near-simultaneous deliveries for the
/// same natural key can each read the same `MAX` and collide on `tasks_idempotency_idx` (`23505`).
/// We **retry** on that unique violation (a fresh `MAX` is computed each attempt, so the loser just
/// lands the next epoch) — bounded, so a genuinely persistent conflict can't spin forever. This keeps
/// the "an explicit mention always lands a task" guarantee even under concurrency. On insert, notifies
/// [`TASK_QUEUED_CHANNEL`] like [`create_task`].
pub async fn create_explicit_task(pool: &PgPool, task: &NewTask) -> Result<Uuid, sqlx::Error> {
    const MAX_ATTEMPTS: u32 = 5;
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let id = Uuid::new_v4();
        let result = sqlx::query_as::<_, (Uuid, String)>(sqlx::AssertSqlSafe(format!(
            "INSERT INTO tasks (id, repository_id, installation_id, webhook_delivery_id, target_type, \
             target_id, command_text, base_sha, head_sha, preset, entry_point, trigger_comment_id, \
             trace_context, model_override, check_runs_enabled, run_epoch, status) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, \
               (SELECT COALESCE(MAX(run_epoch), -1) + 1 FROM tasks \
                WHERE repository_id = $2 AND target_type = $5 AND target_id = $6 \
                  AND command_text = $7 AND head_sha IS NOT DISTINCT FROM $9), \
               {INITIAL_TASK_STATUS_SQL}) \
             RETURNING id, status"
        )))
        .bind(id)
        .bind(task.repository_id)
        .bind(task.installation_id)
        .bind(&task.webhook_delivery_id)
        .bind(&task.target_type)
        .bind(task.target_id)
        .bind(&task.command_text)
        .bind(&task.base_sha)
        .bind(&task.head_sha)
        .bind(&task.preset)
        .bind(&task.entry_point)
        .bind(task.trigger_comment_id)
        .bind(&task.trace_context)
        .bind(&task.model_override)
        .bind(task.check_runs_enabled)
        .fetch_one(pool)
        .await;
        match result {
            Ok((new_id, status)) => {
                notify_or_log_initial_status(pool, new_id, task.repository_id, &status).await;
                return Ok(new_id);
            }
            // Lost the epoch race — recompute MAX and try the next epoch.
            Err(sqlx::Error::Database(db))
                if db.code().as_deref() == Some("23505") && attempt < MAX_ATTEMPTS =>
            {
                tracing::debug!(attempt, "explicit-task epoch race (23505); retrying");
            }
            Err(e) => return Err(e),
        }
    }
}

/// Cancel a PR's active tasks (queued/running/posting_result) — used when the PR is closed so its
/// work stops. Returns the cancelled task ids. The agent Jobs of cancelled tasks are deleted by the
/// reaper (the control plane that serves webhooks has no Kubernetes client — trust boundary).
pub async fn cancel_active_tasks_for_pr(
    pool: &PgPool,
    repository_id: i64,
    pr: i64,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "UPDATE tasks SET status = 'cancelled', completed_at = now(), \
             lease_owner = NULL, lease_expires_at = NULL \
         WHERE repository_id = $1 AND target_type = 'pull_request' AND target_id = $2 \
           AND status IN ('queued', 'running', 'posting_result') \
         RETURNING id",
    )
    .bind(repository_id)
    .bind(pr)
    .fetch_all(pool)
    .await
}

/// Cancel a PR's earlier **automatic** review runs when a new push supersedes them (epic #566's
/// `supersede` push-storm strategy). Deliberately narrower than [`cancel_active_tasks_for_pr`] in two
/// ways:
///
/// - Scoped to `entry_point IN ('pr_open', 'pr_sync')` — an explicit human `@mention`
///   (`entry_point = 'mention'`) or an A2A-triggered run is never cancelled by someone else's push;
///   only the bot's own automatic review passes are superseded.
/// - Includes `waiting_for_index` (ADR-0055) alongside the live dispatch statuses, which
///   `cancel_active_tasks_for_pr` leaves out — a review parked behind an in-flight index (or, after
///   this epic, sitting out a `debounce` delay) is exactly the kind of stale not-yet-started run
///   supersede exists to kill, not just ones already running.
///
/// `keep_head` (the new push's head) is excluded so a fresh push never cancels itself. Returns the
/// cancelled task ids — the caller resolves each one's check run to `Cancelled` so a killed run's
/// "in progress" check doesn't hang on the PR forever.
pub async fn cancel_superseded_pr_reviews(
    pool: &PgPool,
    repository_id: i64,
    pr: i64,
    keep_head: &str,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "UPDATE tasks SET status = 'cancelled', completed_at = now(), \
             lease_owner = NULL, lease_expires_at = NULL \
         WHERE repository_id = $1 AND target_type = 'pull_request' AND target_id = $2 \
           AND entry_point IN ('pr_open', 'pr_sync') \
           AND status IN ('queued', 'waiting_for_index', 'running', 'posting_result') \
           AND head_sha IS DISTINCT FROM $3 \
         RETURNING id",
    )
    .bind(repository_id)
    .bind(pr)
    .bind(keep_head)
    .fetch_all(pool)
    .await
}

/// Cancel every active (queued/running/posting) task for a repository — used when the repo is removed
/// from the installation or denied, so in-flight Jobs are stopped (the reaper deletes them) and
/// nothing new dispatches. Returns the cancelled task ids.
pub async fn cancel_active_tasks_for_repo(
    pool: &PgPool,
    repository_id: i64,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "UPDATE tasks SET status = 'cancelled', completed_at = now(), \
             lease_owner = NULL, lease_expires_at = NULL \
         WHERE repository_id = $1 AND status IN ('queued', 'running', 'posting_result') \
         RETURNING id",
    )
    .bind(repository_id)
    .fetch_all(pool)
    .await
}

/// Cancel a single task by id, if it's still active. Returns `true` when a row moved to `cancelled`
/// (false if the id is unknown or already terminal). Backs the manual "Cancel run" action; the
/// runner's self-cancel poll / the reaper then stop the Job + pod.
pub async fn cancel_task_by_id(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE tasks SET status = 'cancelled', completed_at = now(), \
             lease_owner = NULL, lease_expires_at = NULL \
         WHERE id = $1 \
           AND status IN ('received', 'waiting_for_index', 'queued', 'running', 'posting_result')",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}
/// Cancelled tasks that still have a Kubernetes Job to clean up (the reaper deletes the Job, then
/// clears `job_name` so the row isn't returned again).
pub async fn list_cancelled_with_job(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<ReapableTask>, sqlx::Error> {
    sqlx::query_as::<_, ReapableTask>(
        "SELECT id, job_name, attempts FROM tasks \
         WHERE status = 'cancelled' AND job_name IS NOT NULL \
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Clear a task's `job_name` once its Job has been deleted (so the cleanup is idempotent).
pub async fn clear_job_name(pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE tasks SET job_name = NULL WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .map(|_| ())
}

/// Atomically claim the next due `queued` task and take a short dispatch lease. `FOR UPDATE SKIP
/// LOCKED` guarantees that concurrent dispatcher replicas never claim the same row. Returns `None`
/// when nothing is due. (Lease expiry is reaped by the scheduler in RFC-0001 Phase 2.)
pub async fn claim_next_task(
    pool: &PgPool,
    owner: &str,
    lease: Duration,
) -> Result<Option<ClaimedTask>, sqlx::Error> {
    sqlx::query_as::<_, ClaimedTask>(
        "UPDATE tasks \
         SET status = 'running', attempts = attempts + 1, started_at = now(), \
             lease_owner = $1, lease_expires_at = now() + ($2 * interval '1 second') \
         WHERE id = ( \
           SELECT id FROM tasks \
           WHERE status = 'queued' AND run_after <= now() \
           ORDER BY priority DESC, created_at, id \
           FOR UPDATE SKIP LOCKED \
           LIMIT 1 \
         ) \
         RETURNING id, repository_id, installation_id, target_type, target_id, command_text, \
                   base_sha, head_sha, attempts, trace_context",
    )
    .bind(owner)
    .bind(lease.as_secs_f64())
    .fetch_optional(pool)
    .await
}

/// Record the Kubernetes Job created for a dispatched task.
pub async fn set_task_job(pool: &PgPool, id: Uuid, job_name: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE tasks SET job_name = $2 WHERE id = $1")
        .bind(id)
        .bind(job_name)
        .execute(pool)
        .await
        .map(|_| ())
}

/// Return a `running` task to the queue with a backoff delay — used both when Job creation fails and
/// when the reaper requeues a stuck task. Clears the lease, `started_at`, and `job_name` so the next
/// claim is clean and the next dispatch creates a fresh Job (the Job name is derived from the task
/// id, so a stale name would otherwise collide). Guarded on the active statuses so two reapers can't
/// both requeue the same task. Returns `true` if a row was actually requeued.
pub async fn release_task(pool: &PgPool, id: Uuid, backoff: Duration) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE tasks \
         SET status = 'queued', lease_owner = NULL, lease_expires_at = NULL, started_at = NULL, \
             job_name = NULL, run_after = now() + ($2 * interval '1 second') \
         WHERE id = $1 AND status IN ('running', 'posting_result')",
    )
    .bind(id)
    .bind(backoff.as_secs_f64())
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// A `running` task whose claim lease has expired — a candidate the reaper reconciles against its
/// Job's real liveness (RFC-0001 Phase 2).
#[derive(Debug, sqlx::FromRow)]
pub struct ReapableTask {
    pub id: Uuid,
    pub job_name: Option<String>,
    pub attempts: i32,
}

/// Tasks stuck in an active status (`running`/`posting_result`) past their lease — the lease is set
/// short at claim and renewed by the reaper only while the Job is live, so an expired lease just
/// means "needs reconciling", not "dead" — the caller decides by checking each Job's liveness.
/// Bounded so one cycle is cheap (backed by the `tasks_reapable_idx` partial index).
pub async fn list_reapable_tasks(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<ReapableTask>, sqlx::Error> {
    sqlx::query_as::<_, ReapableTask>(
        "SELECT id, job_name, attempts FROM tasks \
         WHERE status IN ('running', 'posting_result') AND lease_expires_at < now() \
         ORDER BY started_at NULLS FIRST, id \
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Extend an active task's lease — the reaper's heartbeat for a Job it confirmed is still live, so a
/// long-running task isn't reclaimed. No-op (returns `false`) if the task is no longer active.
pub async fn renew_lease(pool: &PgPool, id: Uuid, lease: Duration) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE tasks SET lease_expires_at = now() + ($2 * interval '1 second') \
         WHERE id = $1 AND status IN ('running', 'posting_result')",
    )
    .bind(id)
    .bind(lease.as_secs_f64())
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Optional filters for [`list_tasks_page`]. `status` is already-expanded raw DB status values (the
/// UI's `StatusVariant` → raw-status mapping lives in the HTTP handler, so this module stays free of
/// API-shape concerns) — `None` means "no status filter", matching `repository_id`/`query`.
#[derive(Debug, Default)]
pub struct TasksPageFilter {
    pub status: Option<Vec<String>>,
    pub repository_id: Option<i64>,
    pub query: Option<String>,
}

/// A page of tasks (most-recent-first) plus the total matching row count — the real pagination behind
/// `GET /tasks`'s `page`/`page_size`/`status`/`repository_id`/`q` query params. `query` matches
/// (case-insensitively) against `command_text`, `head_sha`, `target_id`, and the joined repo's
/// `owner`/`name` — the practical equivalent of the dashboard's old client-side search, not the
/// rendered trigger label verbatim.
pub async fn list_tasks_page(
    pool: &PgPool,
    filter: TasksPageFilter,
    limit: i64,
    offset: i64,
) -> Result<(Vec<TaskRow>, i64), sqlx::Error> {
    // sqlx 0.9's `SqlSafeStr` lint requires literal `&'static str` queries (no `format!`/runtime
    // string building, even from a compile-time-constant fragment) — the WHERE clause is duplicated
    // below rather than spliced in, to satisfy that without an `AssertSqlSafe` escape hatch.
    let tasks = sqlx::query_as::<_, TaskRow>(
        "SELECT t.*, r.owner AS repo_owner, r.name AS repo_name, \
         r.default_branch AS repo_default_branch, r.platform AS repo_platform \
         FROM tasks t LEFT JOIN repositories r ON r.id = t.repository_id \
         WHERE ($1::text[] IS NULL OR t.status = ANY($1)) \
           AND ($2::bigint IS NULL OR t.repository_id = $2) \
           AND ($3::text IS NULL OR t.command_text ILIKE '%' || $3 || '%' \
                OR t.head_sha ILIKE '%' || $3 || '%' \
                OR t.target_id::text ILIKE '%' || $3 || '%' \
                OR r.owner ILIKE '%' || $3 || '%' OR r.name ILIKE '%' || $3 || '%') \
         ORDER BY t.created_at DESC, t.id DESC \
         LIMIT $4 OFFSET $5",
    )
    .bind(&filter.status)
    .bind(filter.repository_id)
    .bind(&filter.query)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) \
         FROM tasks t LEFT JOIN repositories r ON r.id = t.repository_id \
         WHERE ($1::text[] IS NULL OR t.status = ANY($1)) \
           AND ($2::bigint IS NULL OR t.repository_id = $2) \
           AND ($3::text IS NULL OR t.command_text ILIKE '%' || $3 || '%' \
                OR t.head_sha ILIKE '%' || $3 || '%' \
                OR t.target_id::text ILIKE '%' || $3 || '%' \
                OR r.owner ILIKE '%' || $3 || '%' OR r.name ILIKE '%' || $3 || '%')",
    )
    .bind(&filter.status)
    .bind(filter.repository_id)
    .bind(&filter.query)
    .fetch_one(pool)
    .await?;

    Ok((tasks, total))
}

/// A single task by id.
pub async fn get_task(pool: &PgPool, id: Uuid) -> Result<Option<TaskRow>, sqlx::Error> {
    sqlx::query_as::<_, TaskRow>(
        "SELECT t.*, r.owner AS repo_owner, r.name AS repo_name, \
         r.default_branch AS repo_default_branch, r.platform AS repo_platform \
         FROM tasks t LEFT JOIN repositories r ON r.id = t.repository_id \
         WHERE t.id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}
/// Everything the agent runner needs to act on a task, joined with its repository identity. Served
/// by the internal runner API (`GET /internal/tasks/{id}`) so the runner never holds the GitHub App
/// key — it receives repo coordinates here and a freshly-minted installation token alongside (the
/// control plane mints it; see `internal.rs`). `installation_id` is kept server-side for that.
#[derive(Debug, sqlx::FromRow)]
pub struct TaskContextRow {
    pub id: Uuid,
    pub repository_id: i64,
    pub installation_id: i64,
    pub owner: String,
    pub name: String,
    pub default_branch: String,
    pub platform: Platform,
    pub target_type: String,
    pub target_id: i64,
    pub command_text: String,
    /// Run kind (ADR-0033): `review` or `ask`. The runner branches on this — a `review` produces
    /// diff-scoped findings, an `ask` produces a conversational answer.
    pub kind: String,
    /// Resolved review preset (ADR-0103) — the runner resolves its `ReviewConfig` by this name.
    pub preset: String,
    /// Which entry point created this task (`"pr_open"`/`"mention"`/`"a2a"`, ADR-0103) — used for
    /// framing decisions that must not key off the (operator-defined) preset name.
    pub entry_point: String,
    pub base_sha: Option<String>,
    pub head_sha: Option<String>,
    /// The `@mention` comment that triggered this task (ADR-0068), or `None` for the automatic
    /// `pull_request opened` review. When `Some`, the lifecycle reactions target this comment.
    pub trigger_comment_id: Option<i64>,
    /// Resolved repo/org model override (ADR-0110, story #501), or `None` for no override — the
    /// runner then applies the preset's own configured model unchanged.
    pub model_override: Option<String>,
    /// Whether this task posts a check run / commit status (epic #566), snapshotted at creation. Read
    /// by all four check-run sites so start and resolve can never disagree.
    pub check_runs_enabled: bool,
}

/// Load a task's execution context, or `None` if no such task exists. INNER JOIN on `repositories`:
/// a task always references a repository (FK), so a missing row means a bad/expired id.
pub async fn get_task_context(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<TaskContextRow>, sqlx::Error> {
    sqlx::query_as::<_, TaskContextRow>(
        "SELECT t.id, t.repository_id, t.installation_id, r.owner, r.name, r.default_branch, r.platform, \
                t.target_type, t.target_id, t.command_text, t.kind, t.preset, t.entry_point, t.base_sha, \
                t.head_sha, t.trigger_comment_id, t.model_override, t.check_runs_enabled \
         FROM tasks t JOIN repositories r ON r.id = t.repository_id \
         WHERE t.id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}
/// Statuses the runner is allowed to report. Transitioning into a terminal one stamps
/// `completed_at` and releases the lease; `running` (re)stamps `started_at`. Anything else is
/// rejected by the handler before reaching here.
pub fn is_runner_reportable_status(status: &str) -> bool {
    matches!(
        status,
        "running" | "posting_result" | "succeeded" | "failed" | "timed_out" | "cancelled"
    )
}

/// The task's current status string, or `None` if the row is gone. Lightweight (no token mint) —
/// the runner polls this to self-cancel promptly when its task is cancelled, independent of the
/// reaper (which may be down mid-deploy).
pub async fn get_task_status(pool: &PgPool, id: Uuid) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT status FROM tasks WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Persist the GitHub check-run id opened for a task, so a later resolve can address the SAME check
/// run (new feature — see `outbox::enqueue_check_run_start`). GitLab/Bitbucket never call this (their
/// status APIs upsert by sha, no id to remember).
pub async fn set_check_run_external_id(
    pool: &PgPool,
    id: Uuid,
    external_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE tasks SET check_run_external_id = $2 WHERE id = $1")
        .bind(id)
        .bind(external_id)
        .execute(pool)
        .await
        .map(|_| ())
}

/// Read back a task's check-run id, or `None` when no start ever recorded one (not yet delivered,
/// dead-lettered, or a platform that doesn't use one).
pub async fn get_check_run_external_id(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar("SELECT check_run_external_id FROM tasks WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map(Option::flatten)
}

/// Whether `task_id` is still the most-recently-created, non-cancelled task for its `(repository_id,
/// target_type, target_id, head_sha)` group — i.e. no NEWER run has been created against the SAME PR/MR
/// head SHA. Multiple review runs can land on the identical SHA (repeated `@mention` re-reviews, or the
/// automatic open-review followed by a mention-triggered one); each one enqueues its own
/// `check_run_resolve`/`check_run_start` outbox row, but every platform's check/status is addressed by
/// `(head_sha, name)` (GitHub also self-heals via its `filter=latest` `created_at` ordering when an
/// `external_id` is known — but not when it's missing, e.g. a dead-lettered start; GitLab/Bitbucket have
/// no self-healing at all, upserting unconditionally by sha+name/key). Without this guard, whichever
/// task's resolve is *delivered* last (which can lag hours behind a retry backoff) wins regardless of
/// which run is actually newest or authoritative (#571, a design gap left by #558/#559).
///
/// `newer.status <> 'cancelled'` excludes a superseded task that was itself cancelled before it ever
/// reviewed anything (e.g. the PR closed) — that never produces a competing verdict, so it must not
/// block an older task's own resolve. Ordering is `(created_at, id)`, not `run_epoch`: `run_epoch` is
/// scoped per `(repo, target, command_text, head_sha)` (see [`create_explicit_task`]), so two tasks on
/// the same SHA with different `command_text` (e.g. differently-worded mentions) can both start at
/// epoch 0 — it is not a global ordering across the SHA the way `created_at` is.
///
/// Returns `true` (proceed) when `task_id` is unknown — there is nothing to compare against, and the
/// call sites already treat this delivery as best-effort/non-fatal.
pub async fn should_report_check_run(pool: &PgPool, task_id: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT NOT EXISTS ( \
             SELECT 1 FROM tasks newer \
             JOIN tasks self ON self.id = $1 \
             WHERE newer.repository_id = self.repository_id \
               AND newer.target_type = self.target_type \
               AND newer.target_id = self.target_id \
               AND newer.head_sha IS NOT DISTINCT FROM self.head_sha \
               AND newer.status <> 'cancelled' \
               AND (newer.created_at, newer.id) > (self.created_at, self.id) \
         )",
    )
    .bind(task_id)
    .fetch_one(pool)
    .await
}

/// Apply a runner-reported status transition. Terminal states (`succeeded`/`failed`/`timed_out`/
/// `cancelled`) stamp `completed_at` and clear the dispatcher lease so the reaper (Phase 2) won't
/// reclaim a finished task; `running` stamps `started_at` if unset. Returns `false` if no task
/// matched the id. The caller validates `status` with [`is_runner_reportable_status`] first.
///
/// `detail` is the runner's free-text status reason (#137): when `Some` it is persisted to
/// `error_detail` so the console can surface why a run did not post a review (and tell a silent no-op
/// apart from a real clean success). `None` leaves any existing `error_detail` untouched — a later
/// report without a detail must not erase a reason an earlier one recorded.
/// When an `index` task reaches a terminal status, flip the repo's `waiting_for_index` tasks — parked
/// by the ADR-0055 enqueue gate — to `queued` and wake the dispatcher. A no-op when the completed task
/// is **not** an index task (the `done` CTE is empty), so [`set_task_status`] can call it on every
/// terminal transition. Fires on ANY terminal status (including `failed`/`cancelled`) so a failed index
/// never strands reviews forever — the common `succeeded` case is the one that grounds them.
async fn release_reviews_waiting_on_index(pool: &PgPool, index_task_id: Uuid) {
    let released: Vec<Uuid> = match sqlx::query_scalar(
        // GREATEST(run_after, now()), NOT a flat `now()` (epic #566): a `debounce`-strategy sync task
        // can land in `waiting_for_index` with a run_after still in the future (created while an index
        // for the same repo happened to be in flight). A flat `now()` would fast-forward it claimable
        // the instant the UNRELATED index finishes, defeating the whole point of the debounce window.
        // For every other release (run_after already <= now()) this is unchanged: GREATEST picks now().
        "WITH done AS (SELECT repository_id FROM tasks WHERE id = $1 AND command_text = 'index') \
         UPDATE tasks SET status = 'queued', run_after = GREATEST(run_after, now()) \
         WHERE repository_id IN (SELECT repository_id FROM done) \
           AND status = 'waiting_for_index' \
         RETURNING id",
    )
    .bind(index_task_id)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        // Do NOT swallow this: a failed release leaves the repo's reviews parked in
        // `waiting_for_index`, which the claim query never selects — they only recover when a *later*
        // index task completes. Log loudly so the stall is visible and an operator can requeue (#214
        // review). The status stamp already succeeded, so we don't fail the caller.
        Err(error) => {
            tracing::error!(
                %error, index_task_id = %index_task_id,
                "ADR-0055: releasing reviews waiting on the index FAILED — they stay parked until the \
                 next index completes; a manual requeue may be needed"
            );
            return;
        }
    };
    if released.is_empty() {
        return;
    }
    tracing::info!(
        index_task_id = %index_task_id,
        released = released.len(),
        "index complete: released tasks that were waiting for the index (ADR-0055)"
    );
    // One NOTIFY is enough — the dispatcher drains every `queued` task on any wake.
    let _ = sqlx::query("SELECT pg_notify($1, $2)")
        .bind(TASK_QUEUED_CHANNEL)
        .bind(index_task_id.to_string())
        .execute(pool)
        .await;
}

pub async fn set_task_status(
    pool: &PgPool,
    id: Uuid,
    status: &str,
    detail: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let terminal = matches!(status, "succeeded" | "failed" | "timed_out" | "cancelled");
    // ADR-0077: the status flip and the A2A stream-event append share ONE transaction, so an event
    // exists for every transition a poller could observe (streaming and polling can never disagree).
    // The `UPDATE tasks … WHERE id = $1` takes the row write-lock, serializing concurrent producers on
    // this task; the event append then runs under that lock (see `a2a::events`).
    let mut tx = pool.begin().await?;
    let result = sqlx::query(
        "UPDATE tasks SET \
             status = $2, \
             started_at = CASE WHEN $2 = 'running' THEN COALESCE(started_at, now()) ELSE started_at END, \
             completed_at = CASE WHEN $3 THEN now() ELSE completed_at END, \
             lease_owner = CASE WHEN $3 THEN NULL ELSE lease_owner END, \
             lease_expires_at = CASE WHEN $3 THEN NULL ELSE lease_expires_at END, \
             error_detail = CASE WHEN $2 = 'running' THEN NULL ELSE COALESCE($4, error_detail) END \
         WHERE id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(terminal)
    .bind(detail)
    .execute(&mut *tx)
    .await?;
    // Project the transition onto the A2A event log for any A2A task fronting this run (no-op for a
    // non-A2A task). Atomic with the status flip above; a failure rolls back both, so the runner's
    // retry re-applies the transition and its event together.
    crate::a2a::events::append_transition_events(&mut tx, id, status).await?;
    tx.commit().await?;
    // ADR-0055: a completed index task releases the repo's reviews that were parked behind it. Kept
    // outside the transaction (unchanged behaviour) — it is an independent queue nudge, not part of the
    // status/event atomicity.
    if terminal {
        release_reviews_waiting_on_index(pool, id).await;
    }
    Ok(result.rows_affected() > 0)
}

pub async fn count_recent_mcp_runs(
    pool: &PgPool,
    caller_id: &str,
    window_secs: i64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM webhook_deliveries \
         WHERE event_name = 'mcp.review' \
           AND payload_json->>'caller' = $1 \
           AND received_at > now() - make_interval(secs => $2::double precision)",
    )
    .bind(caller_id)
    .bind(window_secs as f64)
    .fetch_one(pool)
    .await
}
