-- RFC-0006 Phase 2 (ADR-0077): the append-only, per-A2A-task streaming event log.
--
-- A2A streaming (`SubscribeToTask` / the streaming leg of `SendMessage`) must deliver an ORDERED
-- sequence of events — an initial `Task` snapshot, then `TaskStatusUpdateEvent` /
-- `TaskArtifactUpdateEvent` items — where multiple concurrent subscribers on any replica see the
-- SAME events in the SAME order, the stream CLOSES at the terminal state, and a reconnect REPLAYS
-- the sequence with no lost events (spec §3.5.2 / RFC-0006 R6).
--
-- We satisfy R6 STRUCTURALLY with this table rather than with in-process fan-out (which cannot
-- survive `replicas: 2`): ordering is a property of the log, not of any subscriber or pod. The `a2a`
-- role only READS these rows; production is a projection of state transitions the pipeline already
-- performs, appended by the control plane inside the SAME transaction that flips the status
-- (`set_task_status`) or persists the review row (`upsert_review` / `insert_review_if_absent`), so an
-- event exists for every transition a poller could observe — streaming and polling can never disagree.
CREATE TABLE IF NOT EXISTS a2a_task_events (
    -- The A2A task this event belongs to. Events are keyed on the A2A task (NOT the underlying `tasks`
    -- row or the PR), so a re-review that produces a *new* run, or a dedup'd submission fronting an
    -- existing run, each get their own isolated, per-A2A-task sequence (RFC-0006 R5).
    a2a_task_id  UUID        NOT NULL REFERENCES a2a_tasks (a2a_task_id) ON DELETE CASCADE,
    -- Per-`a2a_task_id` monotonic counter, gap-free, starting at 1. Assigned INSIDE the appending
    -- transaction as `COALESCE(MAX(seq), 0) + 1 WHERE a2a_task_id = $1`. This — not `created_at` — is
    -- the SOLE ordering authority: wall-clock is unreliable across replicas and ties, whereas
    -- `MAX(seq)+1` under the row write yields a total order every reader sees identically. A duplicate
    -- `seq` is a hard PK violation (retryable), never a silent gap.
    seq          BIGINT      NOT NULL,
    -- 'status-update' | 'artifact-update' (the two spec event types; more `kind`s reserved for a later
    -- mid-run progress increment gated on incremental transcript submission).
    kind         TEXT        NOT NULL,
    -- SCREAMING_SNAKE A2A state for a status-update (e.g. `TASK_STATE_WORKING`), NULL for an artifact.
    state        TEXT,
    -- True on the single terminal status-update, so a tailing reader closes WITHOUT re-deriving
    -- terminality from `state`.
    final        BOOLEAN     NOT NULL DEFAULT false,
    -- The serialized `StreamResponse` body (a `statusUpdate` or `artifactUpdate` object) to emit.
    payload      JSONB       NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- `(a2a_task_id, seq)` is the primary key and the ordering authority. Concurrent subscribers read
    -- the same rows ordered by the same seq → identical ordered events, with no per-pod state.
    PRIMARY KEY (a2a_task_id, seq)
);

-- `ON DELETE CASCADE` (above) ties event lifetime to the mapping row, so the `a2a_tasks` TTL sweep
-- reaps events with their parent in one delete — no orphan rows. The PK already indexes the hot
-- access pattern (ordered range scans `WHERE a2a_task_id = $1 AND seq > $cursor ORDER BY seq`).
