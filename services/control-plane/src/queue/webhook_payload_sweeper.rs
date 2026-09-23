//! Webhook payload compaction sweeper (ADR-0119).
//!
//! Every accepted webhook delivery is stored with its full JSON payload, and nothing else ever
//! shrinks that table. This sweeper, run on the dispatcher's storage-GC tick alongside the index,
//! outbox and A2A sweepers, compacts payloads older than the retention window (see
//! [`crate::db::compact_webhook_payloads`] for why the rows themselves are kept, and why the
//! `mcp.review` quota ledger is left alone). Each tick handles a bounded batch, so a large backlog
//! drains over several ticks rather than in one write burst.
//! Idempotent: a compacted row no longer matches, and a failed cycle is retried next tick.

use sqlx::PgPool;

use crate::{db, http::metrics};

/// One sweep cycle: compact up to `batch` payloads older than `retention_days`.
pub async fn sweep_once(pool: &PgPool, retention_days: i64, batch: i64) -> anyhow::Result<()> {
    let compacted = db::compact_webhook_payloads(pool, retention_days, batch).await?;
    if compacted > 0 {
        metrics::webhook_payloads_compacted(compacted);
        tracing::info!(
            compacted,
            retention_days,
            batch,
            "webhook payload sweeper: compacted aged payloads"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::platform::Platform;
    use serde_json::{Value, json};

    /// Record a delivery through the webhook path, then back-date it by `age_days`.
    async fn seed_delivery(pool: &PgPool, delivery_id: &str, event: &str, age_days: i64) {
        let payload = json!({ "action": "completed", "repository": { "full_name": "o/r" } });
        assert!(
            db::record_delivery(pool, Platform::GitHub, delivery_id, event, &payload)
                .await
                .unwrap()
        );
        sqlx::query(
            "UPDATE webhook_deliveries SET received_at = now() - make_interval(days => $2::int) \
             WHERE delivery_id = $1",
        )
        .bind(delivery_id)
        .bind(age_days)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn payload_of(pool: &PgPool, delivery_id: &str) -> Value {
        sqlx::query_scalar("SELECT payload_json FROM webhook_deliveries WHERE delivery_id = $1")
            .bind(delivery_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    fn is_compacted(payload: &Value) -> bool {
        payload == &json!({})
    }

    /// Payloads past the window are compacted, recent ones are untouched, and no row is removed.
    #[sqlx::test(migrations = "./migrations")]
    async fn compacts_only_payloads_past_retention(pool: PgPool) {
        seed_delivery(&pool, "old", "check_run", 10).await;
        seed_delivery(&pool, "recent", "check_run", 2).await;

        sweep_once(&pool, 7, 100).await.unwrap();

        assert!(is_compacted(&payload_of(&pool, "old").await));
        assert!(!is_compacted(&payload_of(&pool, "recent").await));
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM webhook_deliveries")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 2, "compaction never deletes a row");
    }

    /// The `mcp.review` quota ledger keeps its payload at any age: `reserve_mcp_run_slot` reads
    /// `payload_json->>'caller'` back over a window this sweep cannot see.
    #[sqlx::test(migrations = "./migrations")]
    async fn mcp_review_rows_are_never_compacted(pool: PgPool) {
        let provenance = json!({ "source": "mcp", "caller": "svc-a", "repo": "o/r", "pr": 1 });
        assert!(
            db::reserve_mcp_run_slot(
                &pool,
                "svc-a",
                3600,
                20,
                Platform::GitHub,
                "mcp-1",
                &provenance,
            )
            .await
            .unwrap()
        );
        seed_delivery(&pool, "gh-1", "pull_request", 999).await;
        sqlx::query(
            "UPDATE webhook_deliveries SET received_at = now() - interval '999 days' \
                     WHERE delivery_id = 'mcp-1'",
        )
        .execute(&pool)
        .await
        .unwrap();

        let compacted = db::compact_webhook_payloads(&pool, 7, 100).await.unwrap();

        assert_eq!(compacted, 1, "only the platform delivery is compacted");
        assert_eq!(payload_of(&pool, "mcp-1").await, provenance);
        assert!(is_compacted(&payload_of(&pool, "gh-1").await));
    }

    /// A redelivery of a compacted delivery is still recognised as a duplicate.
    #[sqlx::test(migrations = "./migrations")]
    async fn redelivery_after_compaction_is_still_a_duplicate(pool: PgPool) {
        seed_delivery(&pool, "d-1", "pull_request", 30).await;
        sweep_once(&pool, 7, 100).await.unwrap();
        assert!(is_compacted(&payload_of(&pool, "d-1").await));

        let redelivered = json!({ "action": "opened" });
        let is_new =
            db::record_delivery(&pool, Platform::GitHub, "d-1", "pull_request", &redelivered)
                .await
                .unwrap();
        assert!(!is_new, "the kept row still dedups the redelivery");
    }

    /// A delivery referenced by a task is compacted in place; the task's reference stays valid.
    #[sqlx::test(migrations = "./migrations")]
    async fn compaction_keeps_task_references_intact(pool: PgPool) {
        seed_delivery(&pool, "d-task", "issue_comment", 30).await;
        let repository_id =
            db::upsert_repository(&pool, Platform::GitHub, 1, "o", "r", "main", None)
                .await
                .unwrap();
        db::create_task(
            &pool,
            &db::NewTask {
                model_override: None,
                check_runs_enabled: false,
                run_after_secs: None,
                repository_id,
                installation_id: 1,
                webhook_delivery_id: "d-task".to_string(),
                target_type: "pull_request".to_string(),
                target_id: 1,
                command_text: "review".to_string(),
                base_sha: None,
                head_sha: None,
                run_epoch: 0,
                preset: "deep".to_string(),
                entry_point: "mention".to_string(),
                trigger_comment_id: None,
                trace_context: None,
            },
        )
        .await
        .unwrap();

        sweep_once(&pool, 7, 100).await.unwrap();

        assert!(is_compacted(&payload_of(&pool, "d-task").await));
        let referenced: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM tasks t JOIN webhook_deliveries d \
                 ON d.delivery_id = t.webhook_delivery_id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(referenced, 1);
    }

    /// The batch bounds each call, oldest first, and a drained table makes the sweep a no-op.
    #[sqlx::test(migrations = "./migrations")]
    async fn batch_bounds_each_call_oldest_first(pool: PgPool) {
        seed_delivery(&pool, "oldest", "workflow_job", 30).await;
        seed_delivery(&pool, "middle", "workflow_job", 20).await;
        seed_delivery(&pool, "newest", "workflow_job", 10).await;

        assert_eq!(db::compact_webhook_payloads(&pool, 7, 2).await.unwrap(), 2);
        assert!(is_compacted(&payload_of(&pool, "oldest").await));
        assert!(is_compacted(&payload_of(&pool, "middle").await));
        assert!(!is_compacted(&payload_of(&pool, "newest").await));

        assert_eq!(db::compact_webhook_payloads(&pool, 7, 2).await.unwrap(), 1);
        assert_eq!(db::compact_webhook_payloads(&pool, 7, 2).await.unwrap(), 0);
    }

    /// A non-positive retention or batch compacts nothing, never "everything".
    #[sqlx::test(migrations = "./migrations")]
    async fn non_positive_settings_skip_compaction(pool: PgPool) {
        seed_delivery(&pool, "ancient", "check_suite", 999).await;

        assert_eq!(
            db::compact_webhook_payloads(&pool, 0, 100).await.unwrap(),
            0
        );
        assert_eq!(
            db::compact_webhook_payloads(&pool, -1, 100).await.unwrap(),
            0
        );
        assert_eq!(db::compact_webhook_payloads(&pool, 7, 0).await.unwrap(), 0);
        assert!(!is_compacted(&payload_of(&pool, "ancient").await));
    }
}
