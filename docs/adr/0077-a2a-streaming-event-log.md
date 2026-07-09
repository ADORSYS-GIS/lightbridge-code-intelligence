# ADR-0077: A2A streaming via an append-only per-task event log (RFC-0006 Phase 2)

- **Status:** Proposed
- **Date:** 2026-07-09
- **Deciders:** @stephane-segning

## Context and Problem Statement

[RFC-0006](../rfc/0006-a2a-agent-surface.md) exposes Lightbridge's `review` agent over the A2A
protocol. Phase 1 (card + `SendMessage` + `GetTask` + `CancelTask`, polling only) is merged and
live (#308): the [`a2a` role](../../services/control-plane/src/a2a/mod.rs) runs as a fourth
ingress face on the control plane, backed by the existing Postgres task queue, and its handler
returns `unsupported_operation` for `SendStreamingMessage` / `SubscribeToTask`
([`handler.rs:456-474`](../../services/control-plane/src/a2a/handler.rs)); the card advertises
`capabilities.streaming: false` ([`card.rs:54-59`](../../services/control-plane/src/a2a/card.rs)).

This ADR is the design pass for **Phase 2 — streaming**. The forces:

- Our deep reviews run up to 2 h ([ADR-0062](0062-two-tier-review-fast-auto-deep-on-demand.md)).
  "Fire, hold a connection, watch it progress" is exactly the delivery model A2A streaming builds
  in, and polling `GetTask` in a tight loop is the wasteful alternative peers reach for without it.
- The A2A spec (v1.0.1) imposes hard semantics on streaming (spec §3.5.2 / RFC-0006 R6):
  `SubscribeToTask` and the streaming leg of `SendStreamingMessage` deliver an **ordered** sequence
  — an initial `Task` snapshot, then `TaskStatusUpdateEvent` / `TaskArtifactUpdateEvent` items —
  **multiple concurrent subscribers must see identical, identically-ordered events**, the stream
  **closes at the terminal state**, and a **reconnect replays** the sequence (no lost events).
- The `a2a` role already runs **replicas: 2** and must stay horizontally scalable. A naive
  in-process fan-out (subscribers registered against an in-memory broadcast channel fed by "this
  pod's" run) breaks the moment the run and the subscriber land on different pods, and cannot
  satisfy "identical ordered events across subscribers" or "reconnect replays".

The question this ADR answers: **how does a stateless, replicated `a2a` role serve A2A streaming
that is ordered, replayable, fan-out-consistent, and terminal-closing — without re-running or
reaching into the review pipeline?**

## Decision Drivers

- **Spec conformance (R6):** strict per-task ordering; identical events across concurrent
  subscribers; terminal-close; reconnect-replays-from-a-durable-point.
- **Horizontal scalability:** no per-run fan-out state pinned to a pod; any replica can serve a
  subscription for any task.
- **Trust boundary unchanged ([ADR-0029](0029-focused-review-not-generic-runner.md)):** streaming
  is a *read* projection of state that already exists; the `a2a` role never launches a Job, never
  touches a forge, holds no forge credentials — the same posture as Phase 1.
- **Reuse the Phase-1 substrate:** Postgres task queue + the `a2a_tasks` mapping table
  ([0025_a2a_tasks.sql](../../services/control-plane/migrations/0025_a2a_tasks.sql)); no RFC-0005
  (Restate) dependency — Phase 2 is Postgres-only by design (RFC-0006 R7).
- **Polling must keep working unchanged:** `GetTask` remains authoritative; streaming is additive.

## Considered Options

- **A. Append-only, sequence-numbered per-task event log (a new `a2a_task_events` table); SSE
  handlers replay-then-tail it.** No fan-out state in the pod.
- **B. In-process broadcast fan-out** — subscribers attach to a Tokio broadcast channel fed by the
  run executing in the same process.
- **C. External broker** (Redis Streams / NATS JetStream) as the ordered log + pub-sub.

## Decision Outcome

Chosen option: **A — an append-only, sequence-numbered per-task event log that SSE handlers
replay-then-tail.** It is the only option that satisfies R6 *and* keeps the role stateless across
replicas *and* reuses the substrate we already run. B cannot survive horizontal scaling; C buys
low-latency wake at the cost of a new stateful dependency we do not otherwise need, and Postgres
`LISTEN`/`NOTIFY` (already the queue's wake mechanism, [`db.rs`](../../services/control-plane/src/db.rs))
gives us the same wake for free (see *Pros and Cons*).

### The event log

A new table, **`a2a_task_events`**, appended to per A2A task. It is a *sibling* of `a2a_tasks`, not
a column on it: events are 1-to-many per task, immutable once written, and queried by a very
different access pattern (ordered range scans / tail polls), so a dedicated append-only table is the
right shape rather than a growing JSONB array on the mapping row.

Schema (illustrative; final DDL lands with the implementing PR):

```sql
CREATE TABLE IF NOT EXISTS a2a_task_events (
    a2a_task_id  UUID        NOT NULL REFERENCES a2a_tasks (a2a_task_id) ON DELETE CASCADE,
    seq          BIGINT      NOT NULL,          -- monotonic per a2a_task_id, gap-free, starts at 1
    kind         TEXT        NOT NULL,          -- 'status-update' | 'artifact-update'
    state        TEXT,                          -- SCREAMING_SNAKE A2A state, for a status-update
    final        BOOLEAN     NOT NULL DEFAULT false,  -- true on the terminal status-update
    payload      JSONB       NOT NULL,          -- the serialized TaskStatusUpdateEvent / TaskArtifactUpdateEvent body
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (a2a_task_id, seq)
);
```

- **`(a2a_task_id, seq)` is the primary key** and the sole ordering authority. `seq` is a per-task
  monotonic counter assigned **inside the appending transaction** as
  `COALESCE(MAX(seq), 0) + 1 WHERE a2a_task_id = $1` (or an equivalent guarded upsert). It is **not**
  a global sequence and **not** `created_at`: wall-clock ordering is unreliable across replicas and
  ties, whereas the per-task `MAX(seq)+1` under the row's write lock yields a total order that every
  reader sees identically. This is the core of the R6 guarantee — ordering is a property of the log,
  not of any subscriber or pod.
- **`ON DELETE CASCADE`** ties event lifetime to the mapping row, so the TTL/retention sweep of
  `a2a_tasks` (the #308 follow-up) reaps events with their parent in one delete — no orphan rows.
- **`kind`** distinguishes the two spec event types; **`final`** marks the single terminal
  `status-update` so a tailing reader knows to close without re-deriving terminality from `state`.

### How events are produced

The `a2a` role **reads** these events; it does not generate the underlying activity. Production is a
projection of state transitions the pipeline already performs, appended by the control plane — the
review pipeline and the Job are untouched (ADR-0029 holds):

1. **Status-update events** are appended when the underlying `tasks.status` transitions. The single
   chokepoint is [`set_task_status`](../../services/control-plane/src/db.rs) (already the one place
   status changes and already fires `pg_notify`). For any `tasks` row that an `a2a_tasks` row fronts,
   the same transaction that flips the status appends an `a2a_task_events` row carrying the
   A2A-mapped state (via [`task_state_from_status`](../../services/control-plane/src/a2a/mapping.rs):
   `queued`→`SUBMITTED`, `running`→`WORKING`, `succeeded`→`COMPLETED`, …) and `NOTIFY`s an A2A
   stream channel. Because the append is in the status-change transaction, an event exists for every
   transition a poller could have observed — streaming and polling can never disagree.
2. **Artifact-update events** are appended at completion: when the review row is finalized, the
   terminal `status-update` (`COMPLETED`, `final = true`) is accompanied by an `artifact-update`
   carrying the same summary + findings artifacts `GetTask` already returns
   ([`review_artifacts`](../../services/control-plane/src/a2a/mapping.rs)). A `REJECTED` submission
   (Phase-1 gate: unapproved repo / missing permission / quota / missing head SHA) appends a single
   terminal `status-update` at rejection time — a stream opened on it replays one event and closes.
3. **Coarse progress events (optional, additive).** The transcript ([ADR-0034](0034-agent-run-transcript-and-observability.md))
   is the natural source of finer-grained "the agent is now retrieving / running SAST / drafting"
   progress. **Honest constraint:** today the runner submits the transcript as a **single end-of-run
   batch** ([0014_agent_transcript.sql](../../services/control-plane/migrations/0014_agent_transcript.sql)),
   so it cannot feed *mid-run* progress without a runner change to stream transcript rows live. Phase
   2 therefore ships with the **status/artifact events above as the guaranteed spine**; progress
   events between `WORKING` and the terminal state are a **later increment** gated on incremental
   transcript submission, and their absence is spec-compliant (the spec mandates the status/artifact
   sequence, not a minimum progress granularity). This ADR does not commit the runner change; it
   reserves `kind` space for it.

Production is **best-effort-but-transactional for the spine**: because the status-update append
shares the `set_task_status` transaction, it is as durable as the status change itself, not a
fire-and-forget side effect.

### The SSE handler contract

On `SubscribeToTask` (and the streaming leg of `SendStreamingMessage`, which submits via the
Phase-1 `submit_review` path and then streams), the handler — after the **same caller-scoped
ownership check** as `GetTask` (`load_owned`, so a caller can only subscribe to its own task; an
unknown-or-foreign id is `TaskNotFound`, no existence leak) — does:

1. **Emit the initial `Task` snapshot** (the current `GetTask` view), then
2. **Replay** `a2a_task_events` for the task from the requested point (`seq > cursor`, default 0 =
   from the beginning) **ordered by `seq`**, then
3. **Tail**: wait on the A2A `NOTIFY` channel (with a bounded poll-interval fallback, mirroring the
   dispatcher's `LISTEN`-with-timeout loop), and on each wake select rows `seq > last_emitted` in
   `seq` order, emitting each as an SSE `data:` frame.
4. **Close** the stream when it emits the event with `final = true` (terminal state). A task already
   terminal at subscribe time replays its full sequence, ends on the terminal event, and closes —
   no tailing.

**Why concurrent subscribers see identical ordered events (R6):** every subscriber, on any replica,
reads the *same* rows from the *same* table ordered by the *same* per-task `seq`. There is no
per-subscriber or per-pod ordering state to diverge — the log *is* the order. Two subscribers that
attach at different times converge: the later one replays history then joins the tail at the same
`seq` frontier. `NOTIFY` is only a **wake hint** (it may coalesce or be missed under load); the
**`seq`-cursor SELECT is the source of truth**, so a missed notification costs latency (the fallback
poll catches it), never a lost or misordered event.

**Reconnect = a fresh `SubscribeToTask`.** The spec's reconnect story is simply re-subscribing; with
a durable log this is automatic — the new subscription replays from `seq 0` (or a caller-supplied
cursor) and no events are lost, because the log outlives the connection. There is no server-side
per-connection resume state to reconstruct.

### Card and wiring changes

- **`card.rs`:** flip `capabilities.streaming` to `Some(true)`. `push_notifications` **stays
  `false`** (Phase 3). The skill, transports, and OIDC security scheme are unchanged.
- **`handler.rs`:** `send_streaming_message` and `subscribe_to_task` replace their Phase-1
  `unsupported_operation` stubs with the replay-then-tail implementation above. `list_tasks` and the
  push-config methods **stay unsupported** (Phase 4 / Phase 3 respectively).
- **Polling is untouched:** `GetTask` / `CancelTask` behave exactly as in Phase 1; `GetTask` remains
  the authoritative point read. A caller may freely mix streaming and polling on the same task.
- **CAS/versioning unchanged:** the `a2a_tasks` optimistic-concurrency version
  ([store.rs](../../services/control-plane/src/a2a/store.rs)) governs the *mapping snapshot*; the
  event log is append-only and needs no CAS (inserts never contend on an existing row — each `seq`
  is new).

### Consequences

- **Good:** R6 is satisfied *structurally* — ordering, cross-subscriber consistency, and
  reconnect-replay are properties of an append-only, per-task-sequenced log, not of hand-rolled
  fan-out code that tests can only sample. The role stays stateless; any replica serves any
  subscription.
- **Good:** streaming and polling are the same projection of the same state, so they can never
  disagree (the status-update append rides the `set_task_status` transaction).
- **Good:** no new infrastructure — reuses Postgres + `LISTEN`/`NOTIFY`, which the queue already
  depends on. No RFC-0005 dependency; Phase 2 is Postgres-only (RFC-0006 R7).
- **Bad:** write amplification — every status transition on an A2A-fronted task now also appends an
  event row, and long/re-reviewed tasks accumulate rows. Bounded by the coarse event set (a handful
  of status transitions + a terminal artifact per run) and reaped by the `a2a_tasks` TTL cascade.
- **Bad:** tail latency is `NOTIFY`-wake plus a fallback poll interval, not push-instant. Acceptable
  for a surface whose unit of progress is a review phase, not a token; the alternative (external
  streaming broker) is disproportionate.
- **Neutral:** mid-run progress granularity is deferred pending incremental transcript submission;
  the shipped spine is spec-compliant without it.

## Pros and Cons of the Options

### A. Append-only per-task event log, replay-then-tail (chosen)

- Good: ordering + cross-subscriber consistency + reconnect-replay are inherent to the log; stateless
  across replicas; reuses Postgres + `NOTIFY`; terminal-close is a boolean on a row.
- Good: streaming/polling parity by construction (shared transaction with `set_task_status`).
- Bad: write amplification and a retention obligation; tail latency bounded by the poll fallback.

### B. In-process broadcast fan-out

- Good: lowest latency; trivially ordered *within one process*; no new table.
- Bad: **fails R6 and horizontal scaling** — a subscriber on replica X cannot see a run driven from
  replica Y; reconnect after a pod restart loses the stream; "identical events across subscribers"
  holds only if every subscriber and the producer share one process, which we cannot guarantee at
  replicas: 2. Rejected.

### C. External broker (Redis Streams / NATS JetStream)

- Good: purpose-built ordered log + low-latency pub-sub + consumer cursors; offloads fan-out.
- Bad: a new stateful dependency and operational surface for a benefit Postgres already delivers here
  (RFC-0006 deliberately keeps Phases 1–3 Postgres-only; RFC-0001 likewise deferred Redis because pg
  `NOTIFY` suffices). Dual-store consistency (the event truth in the broker, task truth in Postgres)
  reintroduces exactly the divergence option A avoids. Rejected now; revisitable if latency or fan-out
  volume ever outgrows Postgres.

## Risk register

| # | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| S1 | **Event ordering / gaps under concurrent producers** — two transitions racing could duplicate or skip a `seq` | Low | High | `seq = MAX(seq)+1` assigned under the row write inside the status-change transaction; `(a2a_task_id, seq)` PK makes a duplicate a hard constraint violation (retryable), not a silent gap; readers order by `seq` and never trust wall-clock. Producers for one task are effectively serialized by `set_task_status` on that `tasks` row |
| S2 | **Backpressure / slow consumer** — a subscriber that reads SSE slowly stalls a connection/holds a DB cursor | Medium | Medium | Tail loop selects a bounded batch per wake and holds **no** long-lived cursor/transaction (stateless `seq`-cursor re-query); per-connection send timeout + max lifetime; a stalled client only backs up its own socket, never a shared broadcast buffer; body/rate limits from Phase 1 (`MAX_BODY_BYTES`, per-identity quota) still apply |
| S3 | **Log growth / retention** — events accumulate unbounded | Medium | Medium | `ON DELETE CASCADE` from `a2a_tasks` reaps events with the parent under the **`a2a_tasks` TTL sweep (the #308 follow-up)** — this ADR makes shipping that TTL a prerequisite, not optional; coarse event set keeps per-task row count small |
| S4 | **Replay cost for long (2 h deep) reviews** — a late/reconnecting subscriber replays a long history | Low | Low | The spine is a handful of coarse events per run (not per-token), so even a 2 h run replays a tiny sequence; a caller-supplied cursor (`seq >`) skips already-seen history on reconnect; PK range scan on `(a2a_task_id, seq)` is index-only |
| S5 | **Re-review / multiple runs on one PR** — a second run reuses/creates task state; which stream does a subscriber see? | Medium | Medium | Events are keyed on **`a2a_task_id`**, not the PR or the underlying `tasks` row, so each A2A task has its own isolated sequence. Phase-1 idempotency (RFC-0006 R5) already maps a dedup'd A2A submission onto the existing underlying run; the event log follows the same mapping — a re-review that produces a *new* underlying run appends to whichever `a2a_task_id` fronts it, and streams stay per-A2A-task-isolated |
| S6 | **`NOTIFY` missed / coalesced under load** → a subscriber never wakes | Medium | Low | `NOTIFY` is a wake hint only; a bounded fallback poll interval guarantees progress even with zero notifications, and the `seq`-cursor SELECT is authoritative — a missed notify costs latency, never correctness |
| S7 | **Terminal event never appended** (crash between the run finishing and the terminal-event append) → a stream tails forever | Low | Medium | Terminal `status-update` shares the finalize transaction; a tail also derives terminality defensively from the live `GetTask` state (already terminal ⇒ close) as a backstop, and connection max-lifetime caps a truly stuck tail |

## Out of scope (later phases)

- **Push notifications (Phase 3).** Webhook egress (`CreateTaskPushNotificationConfig` et al.) with
  the SSRF policy (RFC-0006 R3) and the house egress discipline. `push_notifications` stays `false`
  on the card. Not this ADR.
- **`input-required` (Phase 4).** The `INPUT_REQUIRED` pause/resume needs an awakeable, gated on
  [RFC-0005](../rfc/0005-durable-orchestration-on-restate.md) Phase B ([ADR-0074](0074-restate-egress-pilot.md)
  is Phase A). A streaming task in Phase 2 only ever moves `SUBMITTED → WORKING → terminal`.
- **`ListTasks` (Phase 4).** Cursor-paginated list over a caller's own tasks — still
  `unsupported_operation`. Note the store's `list` is intentionally **not** caller-scoped and MUST
  gain an `AND caller_id = …` predicate before that endpoint is turned on
  (documented in [store.rs](../../services/control-plane/src/a2a/store.rs)); untouched here.
- **Mid-run fine-grained progress events.** Deferred pending incremental transcript submission from
  the runner (see production §3); the shipped status/artifact spine is spec-compliant without it.

## More Information

- [RFC-0006](../rfc/0006-a2a-agent-surface.md) — the A2A agent surface; §Phase 2 (streaming) is the
  sketch this ADR fills in, and R6 is the streaming risk it discharges.
- [ADR-0034](0034-agent-run-transcript-and-observability.md) — the transcript/progress events that a
  later increment would surface as coarse stream events.
- [ADR-0029](0029-focused-review-not-generic-runner.md) — the scope boundary this does not reopen
  (streaming is a read projection; no operator-defined execution, no reach into the Job).
- [ADR-0062](0062-two-tier-review-fast-auto-deep-on-demand.md) — the 2 h deep runs that motivate
  streaming over tight-loop polling.
- Phase 1 code this extends: [`a2a/mod.rs`](../../services/control-plane/src/a2a/mod.rs),
  [`a2a/handler.rs`](../../services/control-plane/src/a2a/handler.rs),
  [`a2a/store.rs`](../../services/control-plane/src/a2a/store.rs),
  [`a2a/card.rs`](../../services/control-plane/src/a2a/card.rs),
  [`a2a/mapping.rs`](../../services/control-plane/src/a2a/mapping.rs),
  [0025_a2a_tasks.sql](../../services/control-plane/migrations/0025_a2a_tasks.sql) (#308).
