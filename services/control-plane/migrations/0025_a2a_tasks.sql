-- RFC-0006 Phase 1 (#299): the A2A ingress mapping table.
--
-- Maps a server-generated A2A `taskId` (plus its `contextId` and the caller's OIDC identity) onto
-- our underlying `tasks` row, and carries the optimistic-concurrency `version` column the A2A
-- `TaskStore` CASes on.
--
-- Why the version column is load-bearing (issue #299): the SDK's `TaskStore::update(task)` takes no
-- *expected* version — it is a blind upsert. Under horizontal scaling two `a2a` replicas can each
-- read a task, mutate it, and write back, silently losing one update. The Postgres store enforces
-- optimistic concurrency at THIS layer: every update is `... WHERE version = <expected>` and bumps
-- `version`, so a lost update fails loudly instead of clobbering. This is a different, lower layer
-- than `run_epoch` (which is review-run idempotency on `tasks`), and must not be conflated with it.
CREATE TABLE IF NOT EXISTS a2a_tasks (
    -- Server-generated A2A taskId (UUIDv7 string from the SDK; stored as UUID).
    a2a_task_id        UUID        PRIMARY KEY,
    -- A2A contextId — the conversation grouping (spec §3.4). Server-generated when the caller omits it.
    context_id         TEXT        NOT NULL,
    -- The caller's stable OIDC identity (subject / client_id), from the validated access token.
    -- Every read/cancel is scoped to this so one caller can never see or cancel another's task.
    caller_id          TEXT        NOT NULL,
    -- The A2A skill this task fronts (`review` in Phase 1).
    skill              TEXT        NOT NULL,
    -- The underlying review `tasks` row this A2A task fronts. NULL for a submission REJECTED at the
    -- gate (unapproved repo / missing permission / quota breach) — those never create a run.
    underlying_task_id UUID        REFERENCES tasks(id) ON DELETE SET NULL,
    -- Last-persisted A2A wire state (SCREAMING_SNAKE, e.g. `TASK_STATE_SUBMITTED`). GetTask always
    -- RE-DERIVES the live state from the underlying task; this column is a snapshot + audit trail.
    state              TEXT        NOT NULL,
    -- Optimistic-concurrency version. Bumped only via a CAS update (WHERE version = expected).
    version            BIGINT      NOT NULL DEFAULT 1,
    -- Serialized `a2a::Task` snapshot (the canonical stored form, incl. `lb.*` linkage metadata).
    task_json          JSONB       NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Caller-scoped lookups: the per-identity quota count (rolling window over a caller's submissions)
-- and skill-filtered reads.
CREATE INDEX IF NOT EXISTS a2a_tasks_caller_idx ON a2a_tasks (caller_id, skill, created_at DESC);
-- contextId grouping (conversation) for a later cursor-paginated ListTasks (Phase 4).
CREATE INDEX IF NOT EXISTS a2a_tasks_context_idx ON a2a_tasks (caller_id, context_id);
-- Reverse lookup from an underlying task (dual-trigger reconciliation, R5 in RFC-0006).
CREATE INDEX IF NOT EXISTS a2a_tasks_underlying_idx ON a2a_tasks (underlying_task_id);
