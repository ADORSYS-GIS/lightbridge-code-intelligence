//! ADR-0087 durable-step store (the `CheckpointRuntime` journal). EXECUTION STATE only, keyed
//! `(task_id, run_epoch, step_name)`. Written only when the agent runs under `CheckpointRuntime`
//! (opt-in). Split out of the former monolithic `db.rs` (ADR-0086 follow-up) — pure move, no behavior
//! change.

use sqlx::PgPool;
use uuid::Uuid;

// ── ADR-0087 durable-step store (the `CheckpointRuntime` journal) ────────────────────────────────
// EXECUTION STATE only, keyed `(task_id, run_epoch, step_name)`. Written only when the agent runs
// under `CheckpointRuntime` (opt-in). `run_epoch` is resolved server-side from the task row so the
// agent never has to know or supply it (trust boundary — it journals `(task_id, step_name)` and the
// control plane keys it against the run's identity tuple).

/// One journaled step result: the stored JSON (as text, cast from `jsonb`) and its content hash.
/// `result` is `None` only for a future offloaded payload (`offload_ref` — scaffolding).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DurableStepRow {
    pub result: Option<String>,
    pub content_hash: String,
}

/// Resolve a task's `run_epoch` (ADR-0076 idempotency tuple), or `None` if the task is gone. The
/// durable-step key derives `run_epoch` from this rather than trusting the caller.
pub async fn durable_step_run_epoch(
    pool: &PgPool,
    task_id: Uuid,
) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar("SELECT run_epoch FROM tasks WHERE id = $1")
        .bind(task_id)
        .fetch_optional(pool)
        .await
}

/// Does this run have ANY journaled steps (ADR-0087)? A cheap `EXISTS` used at the `running`
/// transition to tell a *fresh* attempt (no rows → clear the review buffer, today's behavior) from a
/// *resumed* run (rows present → the replay rehydrates write-step results, so clearing would drop
/// findings that never get re-buffered). With `LCI_DURABLE_REPLAY` off, `durable_step` is always
/// empty, so this is always `false` and the clear always runs — byte-identical to pre-ADR-0087.
pub async fn has_durable_steps(
    pool: &PgPool,
    task_id: Uuid,
    run_epoch: i32,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM durable_step WHERE task_id = $1 AND run_epoch = $2)",
    )
    .bind(task_id)
    .bind(run_epoch)
    .fetch_one(pool)
    .await
}

/// Upsert one journaled step result (replay-idempotent on the `(task_id, run_epoch, step_name)` key:
/// a re-run of the same step overwrites its row rather than duplicating). `result_json` is the
/// serialized JSON body, cast to `jsonb` in-SQL so no extra sqlx feature is needed.
pub async fn upsert_durable_step(
    pool: &PgPool,
    task_id: Uuid,
    run_epoch: i32,
    step_name: &str,
    result_json: &str,
    content_hash: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO durable_step (task_id, run_epoch, step_name, result, content_hash) \
         VALUES ($1, $2, $3, $4::jsonb, $5) \
         ON CONFLICT (task_id, run_epoch, step_name) \
         DO UPDATE SET result = EXCLUDED.result, content_hash = EXCLUDED.content_hash",
    )
    .bind(task_id)
    .bind(run_epoch)
    .bind(step_name)
    .bind(result_json)
    .bind(content_hash)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch one journaled step result, or `None` if this step has not run yet (the replay gap). Reads
/// `result::text` so the JSON is rehydrated without the sqlx `json` feature.
pub async fn fetch_durable_step(
    pool: &PgPool,
    task_id: Uuid,
    run_epoch: i32,
    step_name: &str,
) -> Result<Option<DurableStepRow>, sqlx::Error> {
    sqlx::query_as::<_, DurableStepRow>(
        "SELECT result::text AS result, content_hash FROM durable_step \
         WHERE task_id = $1 AND run_epoch = $2 AND step_name = $3",
    )
    .bind(task_id)
    .bind(run_epoch)
    .bind(step_name)
    .fetch_optional(pool)
    .await
}

/// Purge-on-success (ADR-0087): drop a completed run's whole journal in one statement once it
/// finalizes. Idempotent — a re-finalize just deletes zero rows. Returns the rows removed.
pub async fn purge_durable_steps(
    pool: &PgPool,
    task_id: Uuid,
    run_epoch: i32,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM durable_step WHERE task_id = $1 AND run_epoch = $2")
        .bind(task_id)
        .bind(run_epoch)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// TTL sweep (ADR-0087): the `replay` role's backstop for abandoned/failed/cancelled runs — delete
/// every row older than `retention_secs`, success or failure. The caller validates `retention_secs`
/// is `> 0` BEFORE calling (a `0` cutoff would be `now()` and sweep in-flight state); this function
/// trusts that guard. Returns the rows removed.
pub async fn sweep_durable_steps(pool: &PgPool, retention_secs: f64) -> Result<u64, sqlx::Error> {
    // Belt-and-suspenders over the load-time guard: a non-positive cutoff would resolve to `now()`
    // (or the future) and delete in-flight state. Refuse to sweep rather than trust the caller.
    if retention_secs <= 0.0 {
        return Ok(0);
    }
    let result = sqlx::query(
        "DELETE FROM durable_step WHERE created_at < now() - make_interval(secs => $1)",
    )
    .bind(retention_secs)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Store one open-mode branch patch in the offload table (ADR-0088 offload rule), keyed by its
/// content hash. Idempotent (`ON CONFLICT DO NOTHING`), so a replayed `propose_pr` re-stores the same
/// bytes without duplicating — the outbox `pr_open` intent then carries only the hash, not the patch.
pub async fn put_pr_open_blob(
    pool: &PgPool,
    content_hash: &str,
    task_id: Uuid,
    run_epoch: i32,
    patch: &[u8],
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO pr_open_blob (content_hash, task_id, run_epoch, patch) \
         VALUES ($1, $2, $3, $4) ON CONFLICT (content_hash) DO NOTHING",
    )
    .bind(content_hash)
    .bind(task_id)
    .bind(run_epoch)
    .bind(patch)
    .execute(pool)
    .await?;
    Ok(())
}

/// Rehydrate an offloaded open-mode branch patch by its content hash (the egress plane reads this
/// before pushing + verifies the bytes still hash to the key). `None` if the blob was pruned.
pub async fn get_pr_open_blob(
    pool: &PgPool,
    content_hash: &str,
) -> Result<Option<Vec<u8>>, sqlx::Error> {
    sqlx::query_scalar("SELECT patch FROM pr_open_blob WHERE content_hash = $1")
        .bind(content_hash)
        .fetch_optional(pool)
        .await
}
