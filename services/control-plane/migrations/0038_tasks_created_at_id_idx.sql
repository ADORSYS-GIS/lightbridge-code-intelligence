-- Supports GET /tasks' real server-side pagination (most-recent-first, keyset-shaped ORDER BY),
-- replacing the old fixed-LIMIT-100/no-offset query that had no supporting index because it never
-- needed one at that scale. (created_at, id) matches the query's exact ORDER BY tie-break.
CREATE INDEX IF NOT EXISTS tasks_created_at_id_idx ON tasks (created_at DESC, id DESC);
