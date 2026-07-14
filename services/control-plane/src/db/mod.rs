//! Postgres persistence (hand-written SQLx; cratestack codegen deferred — ADR-0005).
//!
//! Runtime queries only (no compile-time `query!`), so the crate builds without a database. The
//! pool is optional: absent `DATABASE_URL` the control plane runs in a degraded, in-memory mode
//! (dev) and readiness reports it.
//!
//! Split by domain (ADR-0086 follow-up code-quality pass) — this file keeps connection setup, the
//! embedding-dimension reconciler, pool liveness, and webhook-delivery dedup; everything else lives in
//! a sibling module below and is re-exported here so call sites keep using `crate::db::foo(...)`
//! unchanged.

use serde_json::Value;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
#[cfg(test)]
use std::time::Duration;
#[cfg(test)]
use time::OffsetDateTime;
#[cfg(test)]
use uuid::Uuid;

use crate::integrations::platform::Platform;

mod code_chunks;
mod durable_step;
mod feedback;
mod outbox;
mod pending_actions;
mod push_notifications;
mod repositories;
mod reviews;
mod tasks;

pub use code_chunks::*;
pub use durable_step::*;
pub use feedback::*;
pub use outbox::*;
pub use pending_actions::*;
pub use push_notifications::*;
pub use repositories::*;
pub use reviews::*;
pub use tasks::*;

#[cfg(test)]
mod tests;

/// Postgres `LISTEN`/`NOTIFY` channel the dispatcher waits on; `create_task` notifies it on enqueue
/// so a dispatcher reacts immediately instead of waiting for its poll fallback.
pub const TASK_QUEUED_CHANNEL: &str = "task_queued";

/// Connect to `DATABASE_URL` and run migrations. Returns `Ok(None)` when the URL is unset (dev).
/// **Fails fast** (`Err`) when the URL is set but the database is unreachable or migrations fail —
/// the process should exit so the orchestrator restarts it and retries, rather than running
/// permanently unready with no recovery path.
pub async fn connect_from_env() -> anyhow::Result<Option<PgPool>> {
    use anyhow::Context;
    let url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(error) => {
            return Err(anyhow::Error::from(error).context("failed to read DATABASE_URL"));
        }
    };
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .context("failed to connect to DATABASE_URL")?;
    sqlx::migrate!()
        .run(&pool)
        .await
        .context("database migrations failed")?;
    tracing::info!("database connected and migrations applied");
    Ok(Some(pool))
}

/// Reconcile the `code_chunks.embedding` column width to the configured `dimension` (ADR-0018). The
/// pgvector column is a fixed-width `vector(N)`, so changing the embedding model's dimension is
/// **destructive** — every stored vector is the wrong width. No-op when the column already matches
/// (or isn't present / has no fixed dim). On a mismatch: if `allow_clear`, **TRUNCATE `code_chunks`
/// and ALTER the column** to the new width; else return `Err` (fail loud) so a config typo can't
/// silently wipe the semantic index. Idempotent + safe to run from each role at startup.
pub async fn reconcile_embedding_dimension(
    pool: &PgPool,
    dimension: i64,
    allow_clear: bool,
) -> anyhow::Result<()> {
    use anyhow::bail;
    // pgvector stores the dimension in the column's `atttypmod` (== N for `vector(N)`, -1 if none).
    // `to_regclass` resolves the table via the active search_path and yields NULL (→ no row) when it
    // doesn't exist, so this is schema-safe and a no-op before the table is created.
    let current: Option<i32> = sqlx::query_scalar(
        "SELECT atttypmod FROM pg_attribute \
         WHERE attrelid = to_regclass('code_chunks') AND attname = 'embedding' AND NOT attisdropped",
    )
    .fetch_optional(pool)
    .await?;
    let Some(current) = current.filter(|&m| m > 0).map(i64::from) else {
        return Ok(()); // no code_chunks/embedding column or no fixed dimension yet — nothing to do
    };
    if current == dimension {
        return Ok(());
    }
    if !allow_clear {
        bail!(
            "embedding dimension changed ({current} → {dimension}) but \
             embeddings.allow_reindex_on_dim_change is false; refusing to wipe code_chunks. \
             Set the flag to reindex from scratch, or revert the dimension."
        );
    }
    tracing::warn!(
        from = current,
        to = dimension,
        "embedding dimension changed; TRUNCATE code_chunks + ALTER column (reindex from scratch)"
    );
    // Atomic: TRUNCATE + ALTER in one transaction so a failed ALTER can't leave the table truncated
    // but still at the old width (an inconsistent state the next startup wouldn't detect).
    let mut tx = pool.begin().await?;
    sqlx::query("TRUNCATE TABLE code_chunks")
        .execute(&mut *tx)
        .await?;
    // `dimension` is an i64 from typed config (not user free-text), so formatting it into the DDL is
    // safe; the vector type width can't be a bind parameter.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "ALTER TABLE code_chunks ALTER COLUMN embedding TYPE vector({dimension})"
    )))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Persist a GitHub delivery, using its `delivery_id` PRIMARY KEY for exactly-once handling.
/// Returns `true` if the delivery is new (inserted), `false` if it was already seen (duplicate).
pub async fn record_delivery(
    pool: &PgPool,
    platform: Platform,
    delivery_id: &str,
    event_name: &str,
    payload: &Value,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO webhook_deliveries (platform, delivery_id, event_name, payload_json) \
         VALUES ($1, $2, $3, $4) ON CONFLICT (delivery_id) DO NOTHING",
    )
    .bind(platform)
    .bind(delivery_id)
    .bind(event_name)
    .bind(payload)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Liveness of the connection pool (used by readiness).
pub async fn ping(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT 1").execute(pool).await.map(|_| ())
}
