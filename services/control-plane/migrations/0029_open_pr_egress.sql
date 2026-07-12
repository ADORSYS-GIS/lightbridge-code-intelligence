-- ADR-0088: the `open` mode's mediated PR-open egress. The open agent (a credential-light, sandboxed
-- run-once pod) commits to a local branch and calls the internal API; the control plane offloads the
-- branch patch here and enqueues a `pr_open` intent onto the SAME outbox the reconciler drains. This
-- extends the ADR-0037 mediated-action boundary from comments to code — the agent never holds a forge
-- token and never pushes.
--
-- DORMANT + additive: no trigger creates an `open` task, and the reconciler's `pr_open` delivery arm
-- is gated on a security sign-off — so nothing writes these rows in prod yet. The schema lands now so
-- the producer path (endpoint → offload → dedup'd intent) is real and testable.

-- 1. Allow the new `pr_open` outbox kind. The inline CHECK from migration 0020 kept its original
--    auto-generated name (`github_outbox_kind_check`) across the 0024 table rename; drop it and re-add
--    the widened set under a stable name.
ALTER TABLE outbox DROP CONSTRAINT IF EXISTS github_outbox_kind_check;
ALTER TABLE outbox DROP CONSTRAINT IF EXISTS outbox_kind_check;
ALTER TABLE outbox
    ADD CONSTRAINT outbox_kind_check
    CHECK (kind IN ('review', 'reply', 'reaction', 'label', 'failure_notice', 'pr_open'));

-- 2. The offload store (ADR-0082's offload rule, reused by ADR-0088). A `pr_open` intent can be large
--    (a multi-file diff, far bigger than a comment body). The branch patch is content-hashed and stored
--    here; the outbox `pr_open` payload carries the KEY + hash, not the bytes, so the egress plane
--    rehydrates exactly the branch the sandbox produced and can verify it before pushing.
--
--    Keyed by `content_hash` (idempotent put, so a replayed proposal re-stores the same bytes without
--    duplicating). `(task_id, run_epoch)` are carried for traceability + the self-purge sweep. This is
--    EXECUTION STATE (RFC-0005: we own execution state; Postgres owns domain data) — self-purging, torn
--    down with the run.
CREATE TABLE IF NOT EXISTS pr_open_blob (
    content_hash TEXT        PRIMARY KEY,
    task_id      UUID        NOT NULL,
    run_epoch    INTEGER     NOT NULL,
    patch        BYTEA       NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Drives an age-based cleanup sweep (a large diff must not linger after the run settles).
CREATE INDEX IF NOT EXISTS pr_open_blob_created_at_idx ON pr_open_blob (created_at);
