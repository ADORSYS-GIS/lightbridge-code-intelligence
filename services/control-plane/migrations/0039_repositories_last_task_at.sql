-- Most-recent run per repository, stored on the row instead of aggregated per query.
--
-- `GET /repositories` orders by run recency and pages with a keyset cursor over that ordering.
-- Derived as `MAX(tasks.created_at)`, the sort key only exists after the join and the GROUP BY, so
-- the cursor predicate cannot use an index and every page re-scans the whole repositories × tasks
-- join. It is also not stable: a repository receiving a run mid-walk moves in the ordering and is
-- skipped or repeated. A stored column makes the same ordering an index range scan over a value
-- that only moves forward.
--
-- 'epoch' rather than NULL for "no runs yet": `NULL < $ts` is UNKNOWN in SQL, so a NULL sort key
-- drops never-run repositories from every page after the first. A total ordering keeps the keyset
-- comparison honest; `task_count = 0` still identifies them, and the read query maps the sentinel
-- back to NULL for the wire.
ALTER TABLE repositories
    ADD COLUMN IF NOT EXISTS last_task_at TIMESTAMPTZ NOT NULL DEFAULT 'epoch';

UPDATE repositories r
   SET last_task_at = COALESCE(
       (SELECT MAX(t.created_at) FROM tasks t WHERE t.repository_id = r.id),
       'epoch'
   );

CREATE INDEX IF NOT EXISTS repositories_last_task_at_id_idx
    ON repositories (last_task_at DESC, id DESC);

-- A trigger rather than an UPDATE beside each INSERT: tasks are created from several call sites
-- (webhook ingest, manual re-run), and a column the list ordering depends on must not depend on
-- each of them remembering. GREATEST keeps a backdated insert from moving a repository backwards.
CREATE OR REPLACE FUNCTION repositories_track_last_task_at() RETURNS TRIGGER AS $$
BEGIN
    UPDATE repositories
       SET last_task_at = GREATEST(last_task_at, NEW.created_at)
     WHERE id = NEW.repository_id;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS tasks_track_repository_activity ON tasks;
CREATE TRIGGER tasks_track_repository_activity
    AFTER INSERT ON tasks
    FOR EACH ROW EXECUTE FUNCTION repositories_track_last_task_at();
