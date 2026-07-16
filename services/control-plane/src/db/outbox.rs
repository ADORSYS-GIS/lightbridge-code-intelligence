//! GitHub egress outbox (ADR-0059): every outbound GitHub *content* write is an intent row here; the
//! reconciler is the sole consumer that posts it. Producers only enqueue. Split out of the former
//! monolithic `db.rs` (ADR-0086 follow-up) — pure move, no behavior change.

use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::integrations::platform::Platform;

// ── GitHub egress outbox (ADR-0059) ────────────────────────────────────────────────────────────────
// Every outbound GitHub *content* write is an intent row here; the reconciler is the sole consumer that
// posts it. Producers only enqueue.

/// `LISTEN`/`NOTIFY` channel the reconciler's outbox drain waits on; producers notify it on enqueue
/// (the timer fallback in the reconciler covers a missed notify, exactly like the dispatcher on
/// [`TASK_QUEUED_CHANNEL`]).
pub const OUTBOX_CHANNEL: &str = "outbox";

/// Max delivery attempts before an outbox row is parked `failed` (dead-letter). A courtesy post isn't
/// worth retrying forever; the row stays for inspection.
pub const OUTBOX_MAX_ATTEMPTS: i32 = 6;

/// A claimed outbox row, with the coordinates the reconciler needs to post it (no join required).
#[derive(Debug, sqlx::FromRow)]
pub struct OutboxRow {
    pub id: i64,
    pub task_id: Option<Uuid>,
    pub installation_id: i64,
    pub owner: String,
    pub repo: String,
    pub kind: String,
    pub payload: Value,
    pub attempts: i32,
    pub platform: Platform,
    /// The originating task's W3C `traceparent` (ticket #246), copied from `tasks.trace_context` at
    /// enqueue time (see [`enqueue_outbox_post`]) so the reconciler can re-parent its egress span even
    /// long after the task row's ambient span context is gone. `None` for rows not tied to a task
    /// (`task_id IS NULL`) or created before this column existed — the egress span then starts its own
    /// independently-sampled root, which is a graceful degradation, not a bug.
    pub trace_context: Option<String>,
}

/// Enqueue one GitHub-egress intent. Idempotent on `dedup_key` (`ON CONFLICT DO NOTHING`), so a
/// re-finalize or a retry never double-enqueues, and wakes the reconciler via `NOTIFY`. Returns whether
/// a new row was inserted (`false` = a row with this `dedup_key` already existed).
#[allow(clippy::too_many_arguments)]
pub async fn enqueue_outbox_post(
    pool: &PgPool,
    platform: Platform,
    task_id: Option<Uuid>,
    installation_id: i64,
    owner: &str,
    repo: &str,
    kind: &str,
    payload: &Value,
    dedup_key: &str,
) -> Result<bool, sqlx::Error> {
    let inserted = sqlx::query(
        "INSERT INTO outbox (platform, task_id, installation_id, owner, repo, kind, payload, dedup_key, \
         trace_context) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, (SELECT trace_context FROM tasks WHERE id = $2)) \
         ON CONFLICT (dedup_key) DO NOTHING",
    )
    .bind(platform)
    .bind(task_id)
    .bind(installation_id)
    .bind(owner)
    .bind(repo)
    .bind(kind)
    .bind(payload)
    .bind(dedup_key)
    .execute(pool)
    .await?
    .rows_affected()
        > 0;
    if inserted {
        let _ = sqlx::query("SELECT pg_notify($1, $2)")
            .bind(OUTBOX_CHANNEL)
            .bind(dedup_key)
            .execute(pool)
            .await;
    }
    Ok(inserted)
}

/// Claim up to `limit` due outbox rows in `(created_at, id)` order — `id` breaks the `created_at` ties
/// that one-transaction enqueues share (`now()` is transaction-stable). The caller posts each, then
/// [`mark_outbox_posted`] / [`mark_outbox_failed`].
///
/// `FOR UPDATE SKIP LOCKED` only holds the row lock for the duration of *this* statement (no enclosing
/// transaction), so it does **not** protect the claim→post→mark gap — two replicas could each claim a
/// distinct disjoint set, then both deliver. Correctness here rests on the **single-replica** invariant
/// (ADR-0058/0059), not the lock; the `SKIP LOCKED` just avoids a self-collision if a claim ever overlaps
/// an in-flight one.
pub async fn claim_outbox_batch(pool: &PgPool, limit: i64) -> Result<Vec<OutboxRow>, sqlx::Error> {
    sqlx::query_as::<_, OutboxRow>(
        "SELECT id, task_id, installation_id, owner, repo, kind, payload, attempts, platform, \
                trace_context \
         FROM outbox \
         WHERE status = 'pending' AND next_attempt_at <= now() \
         ORDER BY created_at, id \
         LIMIT $1 \
         FOR UPDATE SKIP LOCKED",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Mark an outbox row delivered, recording the posted GitHub id (review/comment) for correlation.
pub async fn mark_outbox_posted(
    pool: &PgPool,
    id: i64,
    platform_ref_id: Option<i64>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE outbox SET status = 'posted', posted_at = now(), platform_ref_id = $2 WHERE id = $1",
    )
    .bind(id)
    .bind(platform_ref_id)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Record a failed delivery: bump `attempts`, stash the error, and either back off (`pending`, retried
/// after `attempts²` minutes) or park as `failed` once `OUTBOX_MAX_ATTEMPTS` is reached.
pub async fn mark_outbox_failed(pool: &PgPool, id: i64, error: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE outbox SET \
             attempts = attempts + 1, \
             last_error = $2, \
             status = CASE WHEN attempts + 1 >= $3 THEN 'failed' ELSE 'pending' END, \
             next_attempt_at = now() + make_interval(mins => (attempts + 1) * (attempts + 1)) \
         WHERE id = $1",
    )
    .bind(id)
    .bind(error)
    .bind(OUTBOX_MAX_ATTEMPTS)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Prune terminal outbox rows past their retention window (ADR-0059 GC). `outbox` is
/// append-mostly — every delivered intent settles to `posted` (a 👀 reaction alone leaves a permanent
/// row per PR) and every dead-lettered one to `failed`, and nothing ever deletes them — so the table,
/// and the `ON CONFLICT (dedup_key)` probe every enqueue pays against it, grow without bound.
///
/// `posted` rows are deleted `posted_retention_days` after delivery (the feedback-join id was recorded
/// at post time, so the row has served its purpose); `failed` rows are kept `failed_retention_days` —
/// longer, for post-mortem inspection — then deleted, keyed off `created_at` (a dead-lettered row has
/// no `posted_at`; the few hours of retries between enqueue and dead-letter are negligible against the
/// multi-week window). `pending` rows are in-flight and never touched, whatever their age. Returns
/// `(posted_deleted, failed_deleted)`.
///
/// A non-positive window is treated as **skip that prune**, never "delete everything": `now() -
/// make_interval(days => 0)` is `now()`, so a `0` would make `posted_at < now()` match every delivered
/// row. The dispatcher already floors its config to a positive default; this second guard keeps the
/// public helper safe for any direct/test caller. The day count binds as `int8` and is narrowed in SQL
/// via `$1::int` (what `make_interval(days …)` — an `int` arg — wants), so an out-of-range value errors
/// loudly (`integer out of range`) instead of silently wrapping like a Rust `as i32` would.
pub async fn prune_outbox(
    pool: &PgPool,
    posted_retention_days: i64,
    failed_retention_days: i64,
) -> Result<(u64, u64), sqlx::Error> {
    let posted = if posted_retention_days > 0 {
        sqlx::query(
            "DELETE FROM outbox \
             WHERE status = 'posted' AND posted_at < now() - make_interval(days => $1::int)",
        )
        .bind(posted_retention_days)
        .execute(pool)
        .await?
        .rows_affected()
    } else {
        0
    };
    let failed = if failed_retention_days > 0 {
        sqlx::query(
            "DELETE FROM outbox \
             WHERE status = 'failed' AND created_at < now() - make_interval(days => $1::int)",
        )
        .bind(failed_retention_days)
        .execute(pool)
        .await?
        .rows_affected()
    } else {
        0
    };
    Ok((posted, failed))
}
