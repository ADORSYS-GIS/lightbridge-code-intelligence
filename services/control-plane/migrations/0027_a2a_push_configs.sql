-- RFC-0006 Phase 3 (ADR-0079): per-task push-notification webhook configs.
--
-- A caller registers a webhook (`TaskPushNotificationConfig`) and the server POSTs task updates to
-- it as they happen, instead of the caller holding an SSE stream or polling. This table is the
-- config store AND the per-config delivery cursor/outbox: the append-only `a2a_task_events` log
-- (ADR-0077, migration 0026) IS the durable queue; each config carries its own `delivered_seq`
-- cursor into that log, so push is a *consumer* of the event log — it adds nothing to the
-- `set_task_status` hot path. Ordering, at-least-once, and no-double-send are properties of the
-- per-config cursor + lease (§4), not of hand-rolled fan-out.
--
-- SECURITY (ADR-0079 §2): `url` holds a caller-controlled webhook URL — the control plane's first
-- *outbound* egress to an arbitrary-internet destination, i.e. a server-side request forgery (SSRF)
-- primitive. It is validated (HTTPS-only, port 443, every resolved IP public — no loopback / RFC1918
-- / link-local+metadata / ULA / cluster CIDRs) by the shared SSRF validator (`src/a2a/ssrf.rs`) at
-- BOTH registration and every delivery attempt; a private/invalid URL never reaches this table.
--
-- SLICE NOTE: this migration lands the table only. The four handler methods still return
-- `push_notification_not_supported` and the card still advertises `push_notifications: false` until
-- the delivery slice (slice 2) wires the notifier role. The table is created-but-unused here.
CREATE TABLE IF NOT EXISTS a2a_push_configs (
    -- Server-generated push-config id (`TaskPushNotificationConfig.id`). Multiple configs per task
    -- are allowed (spec), so this — not `a2a_task_id` — is the primary key.
    config_id       UUID        PRIMARY KEY,
    -- The A2A task this webhook is registered on. Keyed on the A2A task (matching `a2a_task_events`),
    -- and `ON DELETE CASCADE` so the `a2a_tasks` TTL sweep (#321) reaps configs with their parent.
    a2a_task_id     UUID        NOT NULL REFERENCES a2a_tasks (a2a_task_id) ON DELETE CASCADE,
    -- Validated HTTPS webhook URL (see the SECURITY note above and `src/a2a/ssrf.rs`).
    url             TEXT        NOT NULL,
    -- Caller-supplied auth token the delivery client echoes so the receiver can verify the call is
    -- from us (§3). Stored ENCRYPTED at rest, never logged, and sent only over policy-guaranteed
    -- HTTPS. NULL when the caller registers no token.
    token_enc       BYTEA,
    -- Delivery cursor: the highest `a2a_task_events.seq` already delivered to this url. A config
    -- starts at 0 and either replays from the head or from the start (a `create`-time choice; default
    -- from-head, so a late subscriber gets *future* updates). Advances strictly monotonically per
    -- config after each successful POST, so a receiver sees the task's events in `seq` order.
    delivered_seq   BIGINT      NOT NULL DEFAULT 0,
    -- Consecutive failed attempts on the next event. Reset to 0 on success; drives the backoff and
    -- the dead-letter cutoff (§4).
    attempts        INT         NOT NULL DEFAULT 0,
    -- Earliest time the notifier may (re)attempt delivery. `now()` on create (deliver ASAP); pushed
    -- forward by `now() + backoff(attempts)` after a failure.
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- 'active' | 'disabled'. A persistently-failing webhook is dead-lettered (`disabled`) after
    -- MAX_ATTEMPTS and stops being claimed; the caller can re-create/re-enable it.
    state           TEXT        NOT NULL DEFAULT 'active',
    -- Single in-flight delivery per config: the notifier claims a due config via
    -- `SELECT … FOR UPDATE SKIP LOCKED` + a lease, so exactly one worker delivers a given config at a
    -- time (no double-send across `replicas: 2`). NULL when unclaimed.
    lease_owner     TEXT,
    lease_expires_at TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- The caller identity (OIDC `sub`) that registered this config. All CRUD is caller-scoped (§1) so
    -- a caller can only register/read/delete webhooks on its own tasks.
    created_by      TEXT        NOT NULL
);

-- List/delete a task's configs, and cascade-reap them with the parent `a2a_tasks` row.
CREATE INDEX IF NOT EXISTS a2a_push_configs_task_idx ON a2a_push_configs (a2a_task_id);

-- The notifier claim scan (slice 2): pick the next `active` config due for delivery, oldest first.
-- Partial on `state = 'active'` so dead-lettered (`disabled`) configs never sit in the hot index,
-- ordered by `next_attempt_at` to serve the earliest-due config and honour backoff.
CREATE INDEX IF NOT EXISTS a2a_push_configs_due_idx
    ON a2a_push_configs (next_attempt_at)
    WHERE state = 'active';
