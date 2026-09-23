//! Webhook delivery storage lifecycle.
//!
//! A delivery row outlives its payload's usefulness by a wide margin. The row itself is permanent:
//! its `delivery_id` PRIMARY KEY is the webhook dedup guarantee and `tasks.webhook_delivery_id`
//! references it. A platform delivery's payload, by contrast, is read only while it is being routed,
//! yet it is the bulk of the row's size. Compaction therefore replaces old payloads with `{}` and
//! keeps every row.
//!
//! `{}` means "no payload retained" and nothing more. It does not record *why*: a delivery stored
//! without one (an event no router acts on) and one compacted after its retention window look
//! identical in the table. No platform sends an empty webhook body, so an empty payload is never
//! evidence of an ingest bug.

use sqlx::PgPool;

/// Replace the payload of up to `batch` platform deliveries received more than `retention_days` ago
/// with `{}`, oldest first. Returns how many rows were compacted.
///
/// `mcp.review` rows are never compacted. They are not platform deliveries but this service's own
/// quota ledger, and `reserve_mcp_run_slot` reads their `payload_json->>'caller'` back over a window
/// this sweep cannot see: `MCP_QUOTA_WINDOW_SECS` has no upper bound and belongs to another role's
/// environment. Compacting one inside that window would silently stop it counting toward its
/// caller's quota — a quota that loosens as rows age, with nothing to show for it. Excluding the
/// event name keeps that impossible whatever either knob is set to, and costs nothing: the ledger
/// payload is a few provenance fields, not a webhook body.
///
/// A non-positive `retention_days` or `batch` compacts nothing: `make_interval(days => 0)` would
/// match every row. The batch is materialised as an array so the update resolves rows through the
/// primary key rather than scanning the table, and `FOR UPDATE SKIP LOCKED` lets overlapping sweeps
/// split the work instead of queueing.
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
               AND event_name <> 'mcp.review' \
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
