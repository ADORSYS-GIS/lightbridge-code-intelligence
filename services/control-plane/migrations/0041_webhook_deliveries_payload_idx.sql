-- no-transaction
-- Serves payload compaction: the sweep picks the oldest deliveries that still carry a payload.
-- Partial, so a row leaves the index once its payload is compacted and the index stays sized to the
-- retention window rather than the whole table. Built CONCURRENTLY (hence no transaction, and one
-- statement per file) so webhook inserts keep flowing while it builds on a large table.
CREATE INDEX CONCURRENTLY IF NOT EXISTS webhook_deliveries_payload_received_at_idx
    ON webhook_deliveries (received_at)
    WHERE payload_json <> '{}'::jsonb;
