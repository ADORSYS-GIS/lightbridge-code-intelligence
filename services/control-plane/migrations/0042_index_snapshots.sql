-- The commit an index wrote, and the snapshots that finished writing.
--
-- `tasks.indexed_sha` is the commit a run submitted its chunks and graph under, recorded when the
-- first batch arrives. A row in `index_snapshots` says that run completed, and retrieval pins to
-- completed snapshots — one still being written has no row and is never read.

ALTER TABLE tasks ADD COLUMN IF NOT EXISTS indexed_sha TEXT;

CREATE TABLE IF NOT EXISTS index_snapshots (
    repository_id BIGINT      NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    commit_sha    TEXT        NOT NULL,
    completed_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (repository_id, commit_sha)
);

CREATE INDEX IF NOT EXISTS index_snapshots_latest_idx
    ON index_snapshots (repository_id, completed_at DESC);
