//! Platform-egress outbox pruning sweeper (ADR-0059/0072).
//!
//! `outbox` (ADR-0059) is the single platform egress: every outbound content write becomes an
//! intent row the reconciler drains. It is append-mostly — a delivered intent settles to `posted` and
//! a dead-lettered one to `failed`, and nothing ever deletes either. In particular the per-PR 👀
//! `eyes` reaction leaves a permanent `posted` row per PR, and every enqueue (including each re-review)
//! pays an `ON CONFLICT (dedup_key) DO NOTHING` probe against the ever-growing table. Left alone it
//! grows without bound.
//!
//! This sweeper — run periodically by the dispatcher, alongside the index sweeper (ADR-0052) — deletes
//! terminal rows past their retention window: `posted` after `posted_retention_days` (their
//! feedback-join id was recorded at post time, so the row has served its purpose), `failed` after a
//! longer `failed_retention_days` (kept first for post-mortem inspection). `pending` rows are in-flight
//! and never touched, whatever their age. Best-effort + idempotent (a `DELETE` is naturally so): a
//! failed cycle is logged and retried next tick.

use sqlx::PgPool;

use crate::{db, http::metrics};

/// One sweep cycle: prune delivered/dead-lettered outbox rows older than their per-status retention.
/// Cheap when there's nothing to do (a partial `DELETE` over the small terminal-row set).
pub async fn sweep_once(
    pool: &PgPool,
    posted_retention_days: i64,
    failed_retention_days: i64,
) -> anyhow::Result<()> {
    let (posted, failed) =
        db::prune_outbox(pool, posted_retention_days, failed_retention_days).await?;
    if posted > 0 || failed > 0 {
        metrics::outbox_prune_deleted(posted, failed);
        tracing::info!(
            posted_deleted = posted,
            failed_deleted = failed,
            posted_retention_days,
            failed_retention_days,
            "outbox sweeper: pruned terminal rows"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Insert one outbox row at a controlled age. `posted` rows carry `posted_at` (what the policy
    /// keys on); `failed`/`pending` rows leave it NULL and age off `created_at`, exactly as in prod.
    async fn insert_row(pool: &PgPool, dedup_key: &str, status: &str, age_days: i64) {
        sqlx::query(
            "INSERT INTO outbox \
                 (installation_id, owner, repo, kind, payload, dedup_key, status, created_at, posted_at) \
             VALUES (1, 'o', 'r', 'reaction', '{}'::jsonb, $1, $2, \
                     now() - make_interval(days => $3::int), \
                     CASE WHEN $2 = 'posted' THEN now() - make_interval(days => $3::int) END)",
        )
        .bind(dedup_key)
        .bind(status)
        .bind(age_days)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Surviving rows' dedup keys, sorted, so assertions are order-stable.
    async fn surviving_keys(pool: &PgPool) -> Vec<String> {
        sqlx::query_scalar("SELECT dedup_key FROM outbox ORDER BY dedup_key")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    /// Old `posted` rows are pruned; recent `posted`, in-window `failed`, and any `pending` row are
    /// retained — and a second sweep is a no-op.
    #[sqlx::test]
    async fn prunes_terminal_rows_past_retention(pool: PgPool) {
        insert_row(&pool, "posted-old", "posted", 10).await; // > 7d → pruned
        insert_row(&pool, "posted-recent", "posted", 2).await; // < 7d → kept
        insert_row(&pool, "failed-mid", "failed", 10).await; // < 30d → kept (longer window)
        insert_row(&pool, "failed-ancient", "failed", 40).await; // > 30d → pruned
        insert_row(&pool, "pending-forever", "pending", 100).await; // in-flight → never pruned

        sweep_once(&pool, 7, 30).await.unwrap();

        assert_eq!(
            surviving_keys(&pool).await,
            vec!["failed-mid", "pending-forever", "posted-recent"],
            "only over-retention posted/failed rows are pruned; recent + pending survive"
        );

        // The DELETEs are idempotent — a re-run with nothing newly eligible removes nothing more.
        sweep_once(&pool, 7, 30).await.unwrap();
        assert_eq!(
            surviving_keys(&pool).await.len(),
            3,
            "second sweep is a no-op"
        );
    }

    /// The returned per-status counts reflect exactly what each policy deleted.
    #[sqlx::test]
    async fn prune_outbox_reports_per_status_counts(pool: PgPool) {
        insert_row(&pool, "p1", "posted", 9).await;
        insert_row(&pool, "p2", "posted", 8).await;
        insert_row(&pool, "f1", "failed", 31).await;

        let (posted, failed) = db::prune_outbox(&pool, 7, 30).await.unwrap();

        assert_eq!((posted, failed), (2, 1));
    }

    /// A non-positive retention window is a **skip**, never a "delete everything": `now() -
    /// make_interval(days => 0)` is `now()`, so an unguarded `0` would match every terminal row.
    #[sqlx::test]
    async fn non_positive_retention_skips_deletion(pool: PgPool) {
        insert_row(&pool, "posted-ancient", "posted", 999).await;
        insert_row(&pool, "failed-ancient", "failed", 999).await;

        let (posted, failed) = db::prune_outbox(&pool, 0, -5).await.unwrap();

        assert_eq!(
            (posted, failed),
            (0, 0),
            "no deletes for a 0/negative window"
        );
        assert_eq!(surviving_keys(&pool).await.len(), 2, "both rows survive");
    }
}
