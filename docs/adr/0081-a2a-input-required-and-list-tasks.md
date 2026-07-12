# ADR-0081: A2A `input-required` + `ListTasks` (RFC-0006 Phase 4)

- **Status:** Proposed
- **Date:** 2026-07-10
- **Deciders:** @stephane-segning

## Context and Problem Statement

[RFC-0006](../rfc/0006-a2a-agent-surface.md) exposes Lightbridge's `review` agent over A2A. The
surface has shipped bottom-up: Phase 1 (card + `SendMessage` + `GetTask` + `CancelTask`, polling)
is live (#308); Phase 2 (streaming over the append-only per-task `a2a_task_events` log,
[ADR-0077](0077-a2a-streaming-event-log.md)) is merged (#320); Phase 3 (SSRF-guarded webhook push,
[ADR-0079](0079-a2a-push-notifications-webhook-egress.md)) shipped (#325–#330). Every task so far
moves in one direction — `SUBMITTED → WORKING → {COMPLETED, FAILED, CANCELED, REJECTED}` — and the
caller only ever *reads* the task it holds a `taskId` for.

This ADR is the design pass for **Phase 4**, which adds the two capabilities that turn A2A from a
one-shot request/response into a conversation and a queryable surface:

1. **`input-required` — human/agent-in-the-loop.** A deep review can hit a point where it needs
   clarification before it can judge honestly ("this PR rewrites a DB migration — confirm the
   intended target schema before I flag it", or the [ADR-0078](0078-a2a-natural-language-text-part.md)
   text-only/partial submission that today gets a *guided rejection*). The A2A model for this is
   `TASK_STATE_INPUT_REQUIRED`: the task **pauses**, emits a **question artifact**, and the caller
   answers with a **continuation `SendMessage` carrying the same `taskId` + `contextId`** (spec
   §3.4), which resumes it. This is the one A2A state that has **no home in the poll/stream/webhook
   model** — those are all *outward* projections; `input-required` needs an *inbound* answer that
   wakes a parked run.

2. **`ListTasks` — enumerate your own tasks.** A caller that has submitted several reviews (or
   reconnects after losing its `taskId`s) needs to enumerate its own tasks with status/context
   filters and cursor pagination. Today the SDK trait's `list` exists on the store but the handler
   returns `unsupported_operation` ([`handler.rs:760`](../../services/control-plane/src/a2a/handler.rs)).

The two capabilities have **very different dependency profiles**, and conflating them would hold the
cheap one hostage to the expensive one. `input-required` requires a durable pause/resume — a
**Restate awakeable** on the task-lifecycle workflow, which does not exist until
[ADR-0076](0076-restate-task-lifecycle-workflow.md) (RFC-0005 Phase B) lands. `ListTasks` is a
**pure Postgres read** over rows we already write. The central design question is therefore two
questions:

> **(a)** How does an A2A task pause on `INPUT_REQUIRED`, carry a question to the caller, and resume
> when the caller answers with a same-`taskId`/`contextId` continuation — *without* the `a2a` role
> holding Restate credentials or reaching into the Job (ADR-0029)? **(b)** How does `ListTasks`
> become a safe, caller-scoped, paginated read — and can it ship *before* (a), decoupled from the
> Restate gate?

## Decision Drivers

- **The Restate gate is real and asymmetric.** `input-required` is **hard-gated on
  [ADR-0076](0076-restate-task-lifecycle-workflow.md)** — it parks on the *same runner-completion
  awakeable machinery* that ADR-0076 builds (that ADR explicitly names A2A Phase 4 as what it
  unblocks). `ListTasks` carries **no** Restate dependency. The design must make this split
  structural, not incidental, so ListTasks can ship on its own schedule (RFC-0006 R7: worst case we
  ship a polling+webhook+list A2A server *without* `input-required` and it is still spec-compliant).
- **Caller-scoping is load-bearing for `ListTasks` (IDOR).** The store's `list` is **intentionally
  NOT caller-scoped today** and is documented as such
  ([`store.rs:305-316`](../../services/control-plane/src/a2a/store.rs)): it returns rows across *all*
  callers and is safe *only* because the handler returns `unsupported_operation`. Turning `ListTasks`
  on **without** adding an `AND caller_id = $caller` predicate is a direct cross-caller IDOR. This is
  the single concrete code change ListTasks needs, and the driver that dominates its risk register.
- **The continuation must be authenticated, scoped, idempotent, and bounded.** Only the task's owner
  may answer; a stale or duplicate answer must be a safe no-op; a parked task that is never answered
  must expire rather than hang forever.
- **Trust boundary unchanged ([ADR-0029](0029-focused-review-not-generic-runner.md)).** `ListTasks`
  is a read projection; the `input-required` continuation resolves an awakeable **through the
  internal API** (the control plane does the resolve, exactly as it does for the runner-completion
  report in ADR-0076), so the `a2a` role still holds **no Restate credentials** and never reaches the
  Job. A skill is a named entry point, not operator-defined execution.
- **Reuse the substrate.** The `a2a_task_events` log (ADR-0077) already carries status transitions;
  the `INPUT_REQUIRED` transition and its question artifact ride that log, so streaming (ADR-0077)
  and push (ADR-0079) deliver the question with **zero new plumbing** — a caller can learn it needs
  to answer by poll, stream, *or* webhook, and answers over the ordinary `SendMessage` ingress.

## Considered Options

**For the phasing:**

- **P-A. Ship `ListTasks` first (decoupled), `input-required` when ADR-0076 Phase B lands
  (chosen).** ListTasks is a self-contained Postgres read gated only on the caller-scope predicate;
  it delivers caller value immediately and carries no Restate risk. `input-required` follows the
  awakeable.
- **P-B. Ship both together as one "Phase 4" unit.** Reject: binds the ungated read to the gated
  pause; if ADR-0076 slips (its own R2 `ctx.select` entry gate is the riskiest primitive in the
  estate), a done-and-safe ListTasks ships late for no reason.

**For `input-required` pause/resume:**

- **A. Park on a Restate awakeable on the task-lifecycle workflow; resume via a same-`taskId`
  continuation that resolves the awakeable through the internal API (chosen).** The direct realization
  of RFC-0006 Phase 4 on ADR-0076's machinery.
- **B. Poll a Postgres "waiting_for_input" row from the `a2a` role (no Restate).** Reject: this
  re-invents exactly the hand-rolled parking state ADR-0076 is deleting (`waiting_for_index` is the
  cautionary precedent), and it cannot durably hold a 2 h deep review parked-for-days without a busy
  row and a bespoke reaper — the awakeable is *free* while suspended. It would also fork the
  lifecycle: one parking mechanism for the engine path, another for A2A. Rejected as premature
  divergence from the Restate direction.
- **C. Emulate `input-required` inside a `contextId` conversation without a durable task pause** —
  cancel the run, ask, and resubmit a fresh task on the answer. Reject: it breaks the spec's
  same-`taskId` resume contract (§3.4), loses the run's accumulated context, and turns a clarification
  into a re-review (double cost, new idempotency tuple). It also can't express "the agent is
  *mid-run* and blocked" — only "a new run with more input".

## Decision Outcome

Chosen: **P-A + A.** Ship **`ListTasks` first** as a caller-scoped, cursor-paginated read (no Restate
dependency), and **`input-required` second**, gated on [ADR-0076](0076-restate-task-lifecycle-workflow.md)
Phase B, as a workflow awakeable resolved by a same-`taskId` continuation through the internal API.

### 1. `ListTasks` — caller-scoped, cursor-paginated read (ships first, ungated)

`ListTasks` is a projection of the same `a2a_tasks` rows `GetTask` already serves, filtered to the
caller and paginated. The handler replaces its `unsupported_operation` stub with a call into a
**caller-scoped** store method.

- **The load-bearing change: caller-scope the query.** `store.rs::list` is documented as
  *intentionally not caller-scoped* and must gain an `AND caller_id = $caller` predicate before the
  endpoint is enabled ([`store.rs:305-316`](../../services/control-plane/src/a2a/store.rs)). The SDK's
  `TaskStore::list(req)` trait method takes **no caller argument**, so Phase 4 threads the caller
  identity from `ServiceParams` (the same `caller.id` `load_owned` already uses) — either via a new
  `list_owned(caller_id, req)` store method the handler calls directly (preferred: it keeps the
  IDOR-critical predicate impossible to forget, mirroring `load_owned`), or by extending the request
  context. **A missing predicate is a cross-caller IDOR** (any caller enumerates every caller's
  tasks), so this is asserted by a dedicated test (a `svc-a` list must never contain a `svc-b` row),
  the same shape as the existing `load_owned_is_caller_scoped` test.
- **Filters (spec).** `status` (map the A2A wire state back to the stored `state` column, as
  `list` already does via `state_wire`) and `contextId` (exact match). Both are already implemented in
  the trait `list`; they only need the caller predicate added alongside.
- **Cursor pagination.** The Phase-1 `list` returns everything with an empty `next_page_token`; Phase
  4 makes it a real keyset cursor. Order by the existing `(created_at DESC, a2a_task_id DESC)` and
  encode the page token as the opaque `(created_at, a2a_task_id)` of the last row (keyset pagination,
  not `OFFSET`, so it is stable under concurrent inserts). `page_size` is clamped to a sane max
  (e.g. ≤ 100) to bound response size; `total_size` is best-effort or omitted for large sets (an exact
  count over a growing table is not worth a second scan).
- **Trust boundary.** `ListTasks` is a **pure read projection** — no forge reach, no Job, no
  Restate. It reads `a2a_tasks` (the mapping), never re-derives live state per row by fetching every
  underlying `tasks` row (that would be N point reads); the stored `state` column is the snapshot, and
  a caller wanting the *live* state of one task still calls `GetTask` (which does the live read). This
  keeps `ListTasks` cheap and index-only. (A caller-facing note: list `state` is the last-persisted
  snapshot; `GetTask` is authoritative for liveness — the same poll/list contract every task API has.)
- **Card + wiring.** No card capability flag governs `ListTasks` (it is a core method, not a
  capability like `streaming`/`push_notifications`); enabling it is purely the handler swap +
  the caller-scoped store method. **`input-required` is unaffected** — ListTasks ships without it.

### 2. `input-required` — workflow awakeable + same-`taskId` continuation (gated on ADR-0076)

**Hard prerequisite: [ADR-0076](0076-restate-task-lifecycle-workflow.md) Phase B is live.** The pause
*is* a Restate awakeable on the per-task `run` workflow; without that workflow there is nothing to
park on, and Option B (a hand-rolled Postgres parking row) is explicitly rejected above. This section
is a design commitment, **not** a build authorization — it does not begin until ADR-0076's own entry
gate (the `ctx.select` replay-safety verification, R2) passes and its workflow is deployed.

**Pausing (`WORKING → INPUT_REQUIRED`).** When the agent determines it needs caller input, the review
pipeline surfaces a *clarification request* through the existing runner→internal-API report channel
(the same channel that carries status/finalize today, [`http/internal.rs`](../../services/control-plane/src/http/internal.rs)).
The workflow, on receiving it:

1. **Appends an `INPUT_REQUIRED` status-update event** to `a2a_task_events` (ADR-0077) carrying a
   **question artifact** — a `TaskStatusUpdateEvent` whose message/artifact holds the question as a
   `text` part (the human-readable ask) plus, when the ask is structured (e.g. "supply `baseSha`" or
   "confirm target schema `X`"), a `data` part naming the exact fields expected back. Because it is an
   ordinary event on the log, **streaming (ADR-0077) tails it and push (ADR-0079) POSTs it** with no
   new delivery path — the caller is notified however it subscribed, and a poller sees it on the next
   `GetTask`.
2. **Suspends on a durable awakeable**, racing a **durable deadline timer** (`ctx.sleep`) via
   ADR-0076's `ctx.select` — the *same* completion-vs-timeout race pattern step 4 of that ADR already
   builds for the runner-completion awakeable. While suspended the workflow **costs nothing** (no busy
   row, no reaper poll) — the whole reason to build this on the awakeable and not on a
   `waiting_for_input` status column.

**The A2A state needs a home our task model doesn't yet have.** `task_state_from_status`
([`mapping.rs:25`](../../services/control-plane/src/a2a/mapping.rs)) maps `tasks.status` strings to
A2A states and has **no `INPUT_REQUIRED` arm** — there is no Lightbridge status that means "parked for
caller input" (an unknown status deliberately maps to `WORKING`, never a terminal or paused guess).
Phase 4 must therefore either (i) add an underlying status literal (e.g. `awaiting_input`) mapped to
`TASK_STATE_INPUT_REQUIRED`, or (ii) drive the A2A state directly off the workflow/event log rather
than the `tasks.status` string. **(ii) is the right seam under ADR-0076**, which makes `tasks.status`
*derived/reporting-only* — the engine journal, not the status string, drives execution. So the
authoritative `INPUT_REQUIRED` marker is the `a2a_task_events` row (with the question artifact) and
the parked awakeable; `GetTask` reports `INPUT_REQUIRED` from the mapping's stored `state` (advanced
by the same event append via the store's CAS), and `mapping.rs` gains the arm for completeness/back-compat.

**Resuming — the continuation `SendMessage` (same `taskId` + `contextId`, spec §3.4).** Today
`send_message` unconditionally calls `submit_review`
([`handler.rs:640-647`](../../services/control-plane/src/a2a/handler.rs)) — every message is a *new*
submission. Phase 4 makes `send_message` **branch on whether the message references an existing
parked task**:

- If the inbound message carries a `taskId` (spec §3.4 continuation) that **the caller owns** (the
  same `load_owned(taskId, caller.id)` scope check as `GetTask` — an unknown/foreign id is
  `TaskNotFound`, no existence leak) **and** that task is in `INPUT_REQUIRED`, the handler treats the
  message as the **answer**: it extracts the answer parts and **resolves the awakeable through the
  internal API** — the control plane resolves the workflow's awakeable by its id, exactly as it
  resolves the runner-completion awakeable in ADR-0076 step 4, so **the `a2a` role holds no Restate
  credentials**. The workflow's `ctx.select` wakes on the awakeable branch, the pipeline receives the
  answer, and the task transitions `INPUT_REQUIRED → WORKING` (a status-update event on the log →
  delivered to stream/push subscribers).
- If the referenced task is **not** in `INPUT_REQUIRED` (already terminal, or never parked), the
  continuation is a spec `TaskNotFound`/invalid-state error, not a silent new submission.
- If the message carries **no** `taskId`, it is a fresh submission → today's `submit_review` path,
  unchanged.

**Who may answer (authz).** Only the owning caller: `load_owned` scopes the continuation to
`caller.id`, so caller B cannot answer caller A's parked task (and cannot even learn it exists). This
is the same caller-scoping that guards `GetTask`/`CancelTask` and the Phase-3 push-config CRUD.

**Timeout / expiry.** A parked task must not hang forever. The `ctx.sleep` deadline in the
`ctx.select` race is the backstop: if the caller never answers within the window, the **timer branch
wins**, the workflow finalizes the task as **`FAILED`** (a terminal `input-required timed out` status
event, so stream/push subscribers see the close), and the underlying run is settled. The window is a
config knob (generous — clarification is a human-in-the-loop turnaround, not a request timeout) and is
distinct from the Job's `activeDeadlineSeconds` (the run is *parked*, not *executing*, while waiting).

**Idempotency / multi-answer race.** Restate awakeables **resolve exactly once**: a second
continuation for the same task (a retry, a double-click, two racing answers) finds the awakeable
already resolved — the resolve is a **no-op**, and the handler returns the current task snapshot
(now `WORKING` or beyond) rather than erroring or re-injecting the answer. The store's optimistic-
concurrency CAS on the `a2a_tasks` mapping ([`store.rs`](../../services/control-plane/src/a2a/store.rs))
independently prevents a lost update on the state flip. So "answer twice" is safe by construction, not
by a hand-rolled guard.

**Tie to ADR-0078 (the text-only branch becomes clarify-then-confirm).**
[ADR-0078](0078-a2a-natural-language-text-part.md) accepts a natural-language `text` part but, today,
**rejects a text-only or partial submission with actionable guidance** (it names the missing precise
fields). Phase 4 **upgrades that exact branch**: instead of rejecting, a text-only/partial `review`
submission transitions to `INPUT_REQUIRED` with a question artifact naming the missing fields, and the
caller confirms via the continuation — a real **clarify-then-confirm loop** rather than a one-shot
rejection. ADR-0078 was written to leave this seam open (its "forward-compatible with Phase 4" driver),
so this is a designed upgrade, not a rework.

### 3. What does not change

- **The runner contract and the trust boundary.** The runner still reports to the internal API and
  holds no Restate credentials (ADR-0076 / [ADR-0002](0002-rust-control-plane-trust-boundary.md));
  the clarification request and the answer both ride the *existing* report channel, and the control
  plane — not the engine, not the runner — resolves the awakeable.
- **Polling, streaming, push.** `GetTask` stays authoritative; a task may be polled, streamed, and
  webhook-pushed while parked — all three read the same `a2a_task_events` projection and cannot
  disagree (ADR-0077 / ADR-0079). The question artifact is delivered by whichever the caller uses.
- **Idempotency of the underlying run.** A continuation resumes the *same* underlying run (same
  `run_epoch`, same idempotency tuple); it is not a re-review and does not create a new task row.
- **The `ask` skill.** RFC-0006's separate `ask` conversational skill
  ([ADR-0033](0033-inbound-command-parsing-and-run-kinds.md) /
  [ADR-0075](0075-rig-for-new-agent-surfaces.md), multi-turn via `contextId`) is orthogonal to
  `input-required` (which is *the review agent* pausing mid-run, not a Q&A turn) and is not in scope
  here.

### Consequences

- **Good:** `ListTasks` ships **now**, decoupled from Restate — a caller-scoped, keyset-paginated
  read that reuses `a2a_tasks` and adds only the IDOR-critical caller predicate. No new datastore, no
  new dependency.
- **Good:** `input-required` reuses ADR-0076's awakeable + `ctx.select` race and ADR-0077's event log
  wholesale — the pause is *free while suspended*, the question is delivered over the existing
  stream/push paths, and the answer rides the existing runner→internal-API channel. No new egress, no
  Restate credentials in the `a2a` role.
- **Good:** the ADR-0078 text-only branch gets its designed upgrade — guided rejection becomes
  clarify-then-confirm — closing the one place the surface today says "no" where it could say "tell me
  more".
- **Bad:** `send_message` gains a **branch** (new submission vs continuation) — a behavioral fork on
  a hot path that must correctly distinguish "answer to a parked task I own" from "new submission" and
  from "continuation to a task that isn't parked". Covered by tests, but it is the first time the A2A
  ingress is stateful across messages.
- **Bad:** `input-required` inherits **all** of ADR-0076's risk (journal-vs-code evolution on the 2 h
  deep tier, the `ctx.select` replay-safety gate, dual-engine cutover). It cannot be safer than its
  gate; that is the price of a durable pause and the reason it ships second.
- **Neutral:** a new terminal path (`INPUT_REQUIRED` timeout → `FAILED`) and a new non-terminal state
  in the caller-visible lifecycle; both are spec states, and the state table in RFC-0006 already
  anticipates them.

## Risk register

| # | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| Q1 | **`ListTasks` IDOR** — the endpoint ships without the `AND caller_id = $caller` predicate and leaks every caller's tasks | Medium | **High** | A **caller-scoped store method** (`list_owned`, mirroring `load_owned`) threaded with `caller.id` from `ServiceParams`, so the predicate cannot be forgotten; a dedicated cross-caller test (a `svc-a` list never contains a `svc-b` row); the store's existing doc-comment already flags this as the required change |
| Q2 | **`input-required` ships before its gate** — built on an awakeable that ADR-0076 hasn't delivered | Low | High | Hard-gated: `input-required` design does not build until ADR-0076 Phase B is live (its own R2 `ctx.select` entry gate passed). ListTasks is decoupled and ships regardless (RFC-0006 R7) |
| Q3 | **Continuation authz** — caller B answers caller A's parked task | Low | High | The continuation is `load_owned(taskId, caller.id)`-scoped exactly as `GetTask`; a foreign/unknown id is `TaskNotFound`, no existence leak, no cross-caller resolve |
| Q4 | **Multi-answer race / duplicate continuation** — two answers to one parked task | Medium | Low | Restate awakeables resolve **exactly once**; the second resolve is a no-op returning the current snapshot; the `a2a_tasks` CAS independently blocks a lost update on the state flip |
| Q5 | **Parked task never answered → hangs forever** | Medium | Medium | The awakeable races a durable `ctx.sleep` deadline (ADR-0076 `ctx.select`); on timeout the workflow finalizes the task `FAILED` with a terminal event, so stream/push subscribers see the close and the run is settled |
| Q6 | **`send_message` branch misclassifies** a continuation as a new submission (or vice-versa), double-charging or dropping the answer | Medium | Medium | Explicit branch on `message.taskId` presence + `load_owned` + `INPUT_REQUIRED`-state check; a continuation to a non-parked task is an explicit error, never a silent new run; unit-tested on all three paths |
| Q7 | **No `INPUT_REQUIRED` mapping in `task_state_from_status`** — the A2A state has no underlying-status home | Low | Medium | Under ADR-0076 `tasks.status` is derived/reporting-only; the authoritative marker is the `a2a_task_events` row + the parked awakeable, and `GetTask` reports the mapping's stored `state`. `mapping.rs` gains the arm for back-compat; the state is not inferred from a raw status literal |
| Q8 | **`ListTasks` pagination instability / cost** — `OFFSET` drift under concurrent inserts, or an unbounded scan | Low | Low | Keyset cursor on `(created_at, a2a_task_id)` (stable under inserts), `page_size` clamped, `total_size` best-effort; the query is an index-order range scan over `a2a_tasks`, not a per-row live-state fetch |
| Q9 | **`input-required` re-opens the trust boundary** — the `a2a` role gains Restate reach to resolve the awakeable | Low | High | The resolve goes **through the internal API** (the control plane resolves the awakeable, as it does the runner-completion report in ADR-0076); the `a2a` role holds **no** Restate credentials and never reaches the Job (ADR-0029/0002) |
| Q10 | **Stale list snapshot confuses callers** — list `state` lags the live task | Low | Low | Documented contract: list `state` is the last-persisted snapshot, `GetTask` is authoritative for liveness — the same poll/list split every task API has; a stale entry costs a `GetTask`, never a wrong terminal claim |

## Out of scope (later phases / follow-ups)

- **Building `input-required`** — this ADR **designs** it and gates it on
  [ADR-0076](0076-restate-task-lifecycle-workflow.md) Phase B being live; the implementing PR is a
  separate, gated slice, not authorized here. `ListTasks` is the shippable half of Phase 4.
- **A rich structured question schema** — Phase 4 ships the question as a `text` part plus, when
  structured, a `data` part naming expected fields; a formal question/answer JSON schema (typed
  option lists, validation) is a follow-up once real clarify-then-confirm traffic exists.
- **Server-signed continuation correlation** — beyond the same-`taskId` match, a signed challenge in
  the question artifact that the answer must echo is a later hardening if a peer needs it; the
  caller-scoped `taskId` match is the Phase-4 baseline.
- **`ListTasks` cross-context aggregation / server-side search** — Phase 4 is exact-match
  `status`/`contextId` filters + keyset pagination; full-text or free-form query is not in scope.
- **The `ask` skill's own multi-turn semantics** — `contextId`-threaded Q&A
  ([ADR-0033](0033-inbound-command-parsing-and-run-kinds.md)/[ADR-0075](0075-rig-for-new-agent-surfaces.md))
  is orthogonal and unchanged.

## More Information

- [RFC-0006](../rfc/0006-a2a-agent-surface.md) — the A2A surface; §"Phase 4 — `input-required` +
  `ListTasks`" is the sketch this ADR fills in, and R7 is the gating risk it discharges (Phase 4 is
  explicitly gated on RFC-0005 Phase B; ListTasks is the still-compliant fallback if Restate stalls).
- [ADR-0076](0076-restate-task-lifecycle-workflow.md) — the task-lifecycle workflow whose awakeable
  `input-required` parks on; its step-4 `ctx.select` completion-vs-timeout race is the exact pattern
  this reuses, and its "resolve the awakeable through the internal API" rule is why the `a2a` role
  needs no Restate credentials. **This is the hard gate.**
- [ADR-0077](0077-a2a-streaming-event-log.md) — the `a2a_task_events` log the `INPUT_REQUIRED`
  status/question event rides; it already names `input-required` and `ListTasks` as Phase 4 out-of-scope.
- [ADR-0079](0079-a2a-push-notifications-webhook-egress.md) — the webhook egress that POSTs the
  question artifact to a registered receiver with no new delivery path; its "Out of scope" already
  points here.
- [ADR-0078](0078-a2a-natural-language-text-part.md) — the text-only branch whose guided rejection
  this upgrades into a clarify-then-confirm loop.
- [ADR-0029](0029-focused-review-not-generic-runner.md) / [ADR-0002](0002-rust-control-plane-trust-boundary.md)
  — the boundary this does not reopen: `ListTasks` is a read projection, and the continuation resolves
  an awakeable through the internal API — no forge reach, no Job, no Restate credentials in the `a2a` role.
- [ADR-0074](0074-restate-egress-pilot.md) — Restate Phase A (deployed); ADR-0076 Phase B (the gate
  here) is gated on its live exit gate in turn.
- [ADR-0055](0055-review-waits-for-index-readiness.md) — the `waiting_for_index` parking state whose
  hand-rolled shape is the cautionary precedent for *not* re-inventing a Postgres `waiting_for_input`
  row (Option B rejected).
- [ADR-0062](0062-two-tier-review-fast-auto-deep-on-demand.md) — the 2 h deep runs a parked
  `input-required` task must hold cheaply (awakeable-suspended, not a busy row).
- Phase 1–3 code this extends: [`a2a/handler.rs`](../../services/control-plane/src/a2a/handler.rs)
  (the `list_tasks` stub at ~L760 and the `send_message` branch at ~L640),
  [`a2a/store.rs`](../../services/control-plane/src/a2a/store.rs) (the intentionally-un-caller-scoped
  `list`, L305–L316 — the load-bearing ListTasks change),
  [`a2a/mapping.rs`](../../services/control-plane/src/a2a/mapping.rs) (`task_state_from_status`, which
  needs the `INPUT_REQUIRED` arm),
  [0025_a2a_tasks.sql](../../services/control-plane/migrations/0025_a2a_tasks.sql) /
  [0026_a2a_task_events.sql](../../services/control-plane/migrations/0026_a2a_task_events.sql).
