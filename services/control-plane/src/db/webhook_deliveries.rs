//! Webhook delivery storage lifecycle.
//!
//! A delivery row outlives its payload's usefulness by a wide margin. The row itself is permanent:
//! its `delivery_id` PRIMARY KEY is the webhook dedup guarantee and `tasks.webhook_delivery_id`
//! references it. The payload is only read around ingest (routing, and the MCP-review quota window),
//! yet it is the bulk of the row's size. Compaction therefore replaces old payloads with `{}` and
//! keeps every row.

use sqlx::PgPool;

/// Replace the payload of up to `batch` deliveries received more than `retention_days` ago with
/// `{}`, oldest first. Returns how many rows were compacted.
///
/// A non-positive `retention_days` or `batch` compacts nothing: `make_interval(days => 0)` would
/// match every row, including ones whose payload is still being read. The batch is materialised as
/// an array so the update resolves rows through the primary key rather than scanning the table, and
/// `FOR UPDATE SKIP LOCKED` lets overlapping sweeps split the work instead of queueing.
pub async fn compact_webhook_payloads(
    pool: &PgPool,
    retention_days: i64,
    batch: i64,
) -> Result<u64, sqlx::Error> {
    if retention_days <= 0 || batch <= 0 {
        return Ok(0);
    }
    let result = sqlx::query(
        "UPDATE webhook_deliveries SET payload_json = '{}'::jsonb \
         WHERE delivery_id = ANY(ARRAY( \
             SELECT delivery_id FROM webhook_deliveries \
             WHERE payload_json <> '{}'::jsonb \
               AND received_at < now() - make_interval(days => $1::int) \
             ORDER BY received_at \
             LIMIT $2 \
             FOR UPDATE SKIP LOCKED))",
    )
    .bind(retention_days)
    .bind(batch)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}
