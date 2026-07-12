-- ADR-0087 C2: make the `add_comment` write step idempotent under `CheckpointRuntime` replay.
--
-- `add_comment` APPENDs a reply row (unlike inline findings, which are last-write-wins per
-- (task, file, line), or the single-valued summary). At-least-once replay — a crash after the reply
-- buffers but before the step result persists — re-executes the step on resume and DOUBLE-APPENDS.
-- The fix is a dedup key on the run identity + the tool call id: `(task_id, run_epoch, call_id)`.
--
-- Additive and prod-neutral:
--   * both columns are NULLABLE, so pre-existing rows (call_id = NULL, run_epoch = NULL) are valid
--     and untouched;
--   * the unique index is PARTIAL (`WHERE action = 'comment' AND call_id IS NOT NULL`), so it ignores
--     inline/summary rows and every legacy comment with a NULL call_id — those still append freely.
-- The dedup only ever engages for a comment that carries a call_id, which only happens once the agent
-- threads it (behind `LCI_DURABLE_REPLAY`). With the flag off, call_id is always NULL → no behavior
-- change.
ALTER TABLE pending_review_actions ADD COLUMN IF NOT EXISTS run_epoch INTEGER;
ALTER TABLE pending_review_actions ADD COLUMN IF NOT EXISTS call_id   TEXT;

-- One comment per (task, run_epoch, call_id): a replayed reply with the same tool call id conflicts
-- and `ON CONFLICT DO NOTHING` makes the re-insert a no-op. NULL call_ids are excluded (legacy append
-- path), and NULLs are distinct in a unique index anyway, so nothing collapses that shouldn't.
CREATE UNIQUE INDEX IF NOT EXISTS pending_review_comment_dedup
    ON pending_review_actions (task_id, run_epoch, call_id)
    WHERE action = 'comment' AND call_id IS NOT NULL;
