-- ADR-0087: durable replay via `CheckpointRuntime`. The agent loop journals each step's *result*
-- here (through the mediated internal API — the agent holds no DB credential, ADR-0002/0037), so a
-- pod death can requeue the SAME run_epoch and replay completed steps from storage instead of
-- re-paying them. This is EXECUTION STATE only (RFC-0005: we own execution state, Postgres owns
-- domain data); it is self-purging (success-delete + a TTL sweep owned by the `replay` role).
--
-- Additive and prod-neutral: a NEW table only, written to only when the agent runs under
-- `CheckpointRuntime` (opt-in, off by default — prod keeps running `Passthrough`).
--
-- Key: `(task_id, run_epoch, step_name)` — ADR-0076's run-identity tuple + the stability-tested step
-- name (`llm_turn:{n}` / `tools:{n}` / `tool:{n}:{id}`). Unique, so it doubles as the
-- replay-idempotent upsert key: re-running a step re-writes the same row rather than duplicating.
CREATE TABLE IF NOT EXISTS durable_step (
    task_id      UUID        NOT NULL,
    run_epoch    INTEGER     NOT NULL,
    step_name    TEXT        NOT NULL,
    -- The journaled result (`serde_json`), OR a content-hashed pointer when the payload is over-cap
    -- (the ADR-0082 offload rule). Exactly one is set; `offload_ref` is scaffolding for now.
    result       JSONB,
    offload_ref  TEXT,
    -- Lets replay verify a rehydrated result is the same bytes it journaled (ADR-0087 C3).
    content_hash TEXT        NOT NULL,
    -- Drives the TTL sweep (`DURABLE_STEP_RETENTION`).
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (task_id, run_epoch, step_name)
);

-- The TTL sweep deletes by age across all runs; index the age column so it stays cheap on a backlog.
CREATE INDEX IF NOT EXISTS durable_step_created_at_idx ON durable_step (created_at);
