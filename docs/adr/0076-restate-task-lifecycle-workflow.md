# ADR-0076: Restate Phase B — the task lifecycle becomes a workflow (RFC-0005 Phase B)

- **Status:** Proposed
- **Date:** 2026-07-09
- **Deciders:** @stephane-segning

## Context and Problem Statement

[RFC-0005](../rfc/0005-durable-orchestration-on-restate.md) proposes adopting Restate as the
control plane's durable-execution substrate via a strangler migration, one seam at a time.
Phase A — the `PlatformEgress` virtual object ([ADR-0074](0074-restate-egress-pilot.md)) — is
built and deployed: egress delivery now runs as an engine-serialized handler, and the
single-writer-per-installation invariant is structural rather than a `replicas=1` comment.

This ADR records the **next** decision: **which orchestration moves second, and in what shape.**
The candidate is the task lifecycle itself — today spread across the dispatcher claim loop
([`queue/dispatcher.rs`](../../services/control-plane/src/queue/dispatcher.rs)), the stuck-task
reaper ([`queue/reaper.rs`](../../services/control-plane/src/queue/reaper.rs)), and the
idempotency + index-gating machinery in [`db.rs`](../../services/control-plane/src/db.rs)
(`claim_next_task`, `create_task` / `create_explicit_task`, `INITIAL_TASK_STATUS_SQL` /
`release_reviews_waiting_on_index`). That is the correctness-critical path — it launches one
Kubernetes Job per task ([ADR-0004](0004-one-k8s-job-per-task.md)), waits for the runner's
completion report, and reaps Jobs that die without reporting. RFC-0005 §Phase B sketches turning
it into **one Restate `workflow` per task**; this ADR turns that sketch into a real, gated
design.

The problem is narrow and specific: *express the task's full lifecycle — dedup, index-gate wait,
Job launch, completion-or-timeout, finalize + egress — as a single durable handler, without
changing the runner contract, without migrating the `tasks` table in place, and without a
flag-day cutover.*

## Decision Drivers

- Delete the hand-rolled durable-execution primitives RFC-0005 catalogues (claim/lease/reaper
  state machine, `waiting_for_index` parking, `23505`-retry idempotency, `LISTEN/NOTIFY` wakeups)
  — not because they are broken, but to stop re-deriving their correctness for every future
  timer/retry/fan-out feature.
- **Preserve the runner contract and the trust boundary.** The agent-runner
  ([ADR-0017](0017-agent-runner-control-plane-bootstrap.md) /
  [ADR-0002](0002-rust-control-plane-trust-boundary.md)) must keep reporting to the internal API
  and hold no Restate credentials.
- **Reversible, observable cutover.** The correctness-critical path cannot take a flag day; two
  systems will run side by side, and every incident must be answerable with "which engine owned
  this task?".
- **Unblock what depends on it.** A2A `input-required`
  ([RFC-0006](../rfc/0006-a2a-agent-surface.md) Phase 4) needs these awakeables; Phase C
  (retiring the dispatcher/reaper) needs this to land first.

## Considered Options

- **Option A — one `workflow` per task, hosted by `restate-worker`** (this ADR). The workflow ID
  is the existing idempotency tuple + `run_epoch`; the `run` handler owns the lifecycle end to
  end; the runner resolves an awakeable via the unchanged internal API.
- **Option B — keep the dispatcher/reaper; only move the index-gate wait onto a durable
  promise.** A smaller step, but it leaves the claim loop, lease, and reaper in place — most of
  the hand-rolled surface RFC-0005 wants gone — while still paying the dual-system tax. Half a
  migration.
- **Option C — a Restate `service` (stateless durable function) per dispatch, not a
  `workflow`.** Services give durable retries but no per-instance uniqueness key and no
  externally-resolvable durable promises; we would re-add idempotency and the awakeable-address
  registry by hand. The workflow primitive exists precisely for "exactly one, addressable,
  resolvable-from-outside" — which is what a task is.

## Decision Outcome

Chosen option: **Option A — one Restate `workflow` per task**, replacing the dispatcher-claim +
reaper + `waiting_for_index` machinery. **Gated on ADR-0074's Phase A exit gate passing live**
(≥ 3 weeks in prod, zero lost/duplicate posts, dead-letter exercised, one SDK upgrade absorbed):
Phase B does not begin implementation until the pilot has proven the engine on the bounded seam.

The workflow is served by the existing `restate-worker` role
([`restate_worker.rs`](../../services/control-plane/src/restate_worker.rs)) — the same binary and
Deployment ADR-0074 stood up — alongside the `PlatformEgress` object it already hosts.

### The `run` handler

Workflow ID = the existing idempotency tuple + `run_epoch`
(`repository_id, target_type, target_id, command_text, head_sha, run_epoch` — the columns of
`tasks_idempotency_idx`). "Exactly one workflow per task" is therefore **the same dedup we have
today**, enforced by workflow-instance uniqueness instead of the partial unique index and the
`23505`-retry loop in `create_explicit_task`. The `run_epoch` stays in the key so a re-review or
re-index (a fresh epoch over the same natural key, per
[`create_explicit_task`](../../services/control-plane/src/db.rs) /
`create_index_task_for_repo`) is a *distinct* workflow, exactly as it is a distinct row today.

Sketch of `run` (refined against the current code; each numbered step maps to a named
`ctx.run` journaled step unless noted):

1. **Attach the `tasks` row** (`ctx.run`). Upsert the domain row for this workflow ID. The
   `status` column becomes **derived / reporting-only** — Grafana and the console keep reading it
   ([ADR-0046](0046-observability-dashboard-deployment.md)), but the engine's journal, not the
   status string, drives execution. The idempotency INSERT/`ON CONFLICT` logic collapses into
   "does the row exist for this workflow ID?".
2. **Index gate** (durable promise). If this is a non-`index` task and an `index` task is in
   flight for the repo (today's `INITIAL_TASK_STATUS_SQL` `EXISTS` check), **await a durable
   promise** that the repo's index-task workflow resolves on completion. This replaces the
   `waiting_for_index` parking state and the `release_reviews_waiting_on_index` release
   transition ([ADR-0055](0055-review-waits-for-index-readiness.md)). The wait is a suspended
   awakeable, not a busy row — the workflow costs nothing while parked.
3. **Launch the Job** (`ctx.run`). `TaskLauncher::launch`, idempotent by `job_name` (the name is
   derived from the task id, as today), so a replay after a crash between "launched" and
   "journaled" does not create a second Job. This subsumes the dispatcher's
   `launch → set_task_job → 👀 react` sequence; the 👀 work-started reaction
   ([ADR-0068](0068-reaction-driven-review-lifecycle.md)) is enqueued here via `PlatformEgress`.
4. **Await completion, racing a deadline.** Await the **runner-completion awakeable**, selected
   against a durable timer (`ctx.sleep`) set to the Job's `activeDeadlineSeconds` + slack. The
   runner's existing report to the internal API
   ([`http/internal.rs`](../../services/control-plane/src/http/internal.rs) `set_status` /
   `finalize`) **resolves the awakeable** — the runner contract does not change and it still
   holds no Restate credentials (ADR-0017). The control plane, which already authenticates and
   applies that report, additionally resolves the awakeable by the workflow's awakeable id.
   - **Completion branch:** the report carries the terminal status; proceed to finalize.
   - **Timeout branch:** the timer wins. Run today's reaper `decide()` logic
     ([`reaper.rs`](../../services/control-plane/src/queue/reaper.rs)) — check the Job's real
     liveness via `ctx.run` (`job_liveness`), because the timer firing does **not** prove the Job
     is dead (an `Active` Job means the report was merely slow/lost). `Active` → renew and keep
     waiting; `Succeeded` → treat as a lost success report (settle, do not re-run — never
     re-post); `Failed`/`Gone` with attempts remaining → **requeue as a new invocation** (delete
     the dead Job so its task-id-derived name is free, then loop back to step 3);
     attempts exhausted → **fail** + failure notice via `PlatformEgress`
     ([ADR-0057](0057-poller-posts-failure-notice-on-uncatchable-kill.md) /
     [ADR-0059](0059-reconciler-owns-all-github-egress.md)). `MAX_ATTEMPTS` and the
     exponential-with-cap backoff carry over unchanged.
5. **Finalize + egress** (`ctx.run`). Finalize the domain rows (the `finalize_review` /
   `set_task_status` writes), then hand egress intents to the `PlatformEgress` object (Phase A):
   the verdict reaction (👍/👎/😕), any review body, the failure notice. Egress is *already*
   durable and serialized per installation — Phase B simply calls into it from the workflow
   instead of from the dispatcher/reaper.

The producer side (`serve`'s webhook handler) changes from "INSERT + NOTIFY" to "INSERT the
`tasks` row **and** submit the workflow (idempotency key = workflow ID)". The INSERT stays so the
row is the durable audit/reporting record from the first moment; the submit replaces the NOTIFY.

### Determinism rules (inherited from RFC-0005 / ADR-0074, enforced from day one)

These are the Restate programming-model constraints; Phase B's handler is longer than Phase A's,
so they bind harder here. Reviews enforce them:

- **All I/O inside `ctx.run`** — every sqlx query, `TaskLauncher` / k8s API call, `CodePlatform`
  call. Results are journaled and *not* re-executed on replay; consequently each `ctx.run` result
  must be **small** (task id, status enum, `job_name`, a liveness enum — never a transcript,
  diff, or review body). This is the same discipline the `DeliverOutcome` enum already follows in
  `restate_worker.rs`.
- **No `Context` use inside a `ctx.run` closure** (SDK constraint — no nested journal ops).
- **Handler code outside `ctx.run` must be deterministic** — no wall-clock reads, no RNG, no
  config re-reads that can differ across replays. Deadlines use `ctx.sleep` and journaled values,
  not `Instant::now()`. The reaper's current `requeue_backoff(attempts)` is already a pure
  function of `attempts`, so it ports cleanly.
- **No concurrent `Context` fan-out** until [sdk-rust #89](https://github.com/restatedev/sdk-rust/issues/89)
  (buffered-stream freeze) is resolved. The completion-vs-timeout race in step 4 is the one place
  Phase B *needs* to await two futures; use the SDK's own select/`DurableFuturesUnordered`
  (0.10+), never a hand-rolled `futures` combinator over `Context` operations. **This must be
  verified against #89 before implementation** — it is the single riskiest primitive in the
  design (R2 below).
- **Journal-compatible evolution:** changing the order/set of journaled steps in a handler with
  in-flight invocations is a breaking change, managed by deployment versioning (register the new
  revision, drain old ones on their pinned revision). Handlers stay short; anything long-lived
  hangs on an **awakeable**, not a long code path. This is exactly what keeps a 2 h deep-review
  tier cheap to hold open — and what makes "never edit a journaled step sequence in a patch
  release" a hard rollout rule for this workflow (R4).
- **Version pins:** `restate-sdk` (0.x, breaking minors) is pinned exactly in the workspace
  `Cargo.toml`; upgrades are deliberate, alongside a server-compat-matrix check — as ADR-0074
  already requires.

### Migration and cutover (strangler; no in-place table migration)

The `tasks` table is **not** migrated in place. **Both systems write the same rows; exactly one
system owns any given task**, decided at creation:

- **New tasks** (by a creation-time flag / the task-creation timestamp) are submitted as
  workflows by `serve`. Their `tasks` row still exists (written in step 1) so all reporting keeps
  working, but the dispatcher never claims them — they are marked as engine-owned so
  `claim_next_task`'s `SELECT … FOR UPDATE SKIP LOCKED` skips them (a `owner_engine = 'restate'`
  predicate, or equivalently they are never inserted in a dispatcher-claimable status).
- **In-flight legacy tasks** keep draining on the old path: the dispatcher claim loop + reaper
  continue to run and reconcile every pre-cutover row until the legacy backlog is empty. A 2 h
  deep review that started on the dispatcher **finishes** on the dispatcher — it is never handed
  mid-flight to the engine.
- Each run is **stamped with the engine that owns it** (a `run_config_b64`-style marker, as
  RFC-0005 R8 prescribes) and dashboards show engine-per-task, so an incident starts from a known
  path rather than a guess.
- **Phase C** ([RFC-0005](../rfc/0005-durable-orchestration-on-restate.md) Phase C) deletes the
  dispatcher drain/reap/prune ticks, the lease columns, the `waiting_for_index` status, and the
  `LISTEN/NOTIFY` wakeups **only once the legacy backlog is drained and the workflow path has
  soaked** — it is a separate ADR, not part of this one.

### `waiting_for_index` release across engines during cutover

The index gate is the one cross-engine handoff, because an **index** task and the **review** task
it gates can straddle the cutover line. The rule: **the release always flows through the internal
API, never engine-to-engine.**

- **Both new (workflows):** the review workflow awaits a durable promise; the index workflow
  resolves it on completion (step 5). Pure engine-internal.
- **Index legacy, review new:** the legacy index task completes on the old path and, in
  `set_task_status` → `release_reviews_waiting_on_index`, additionally **resolves the awaiting
  workflow's promise via the internal API** (the same mechanism the runner uses to resolve an
  awakeable — the control plane, not the engine, does the resolve). So a dispatcher-run index
  still wakes an engine-parked review.
- **Index new, review legacy:** the index workflow, on completion, performs the existing
  `release_reviews_waiting_on_index` UPDATE (`status = 'queued'`) inside a `ctx.run` step, so a
  dispatcher-parked review is flipped and the dispatcher claims it as usual.

This keeps ADR-0055's semantics intact regardless of which engine owns either side, and it
matches RFC-0005's own resolution of its open question ("likely via the promise being resolved
from the internal API rather than engine-to-engine").

## Consequences

- **Good:** the claim/lease/reaper state machine, `waiting_for_index` parking, `23505`
  idempotency retries, and `LISTEN/NOTIFY` wakeups collapse into one workflow handler + engine
  primitives — the bulk of RFC-0005's hand-rolled-durability catalogue, gone. Future
  timer/retry/fan-out features stop re-proving safety from SKIP-LOCKED first principles.
- **Good:** the runner contract, the one-Job-per-task model (ADR-0004), and the trust boundary
  (ADR-0002/0017) are untouched. This is a control-plane-internal change; nothing in the Job or
  its bootstrap moves.
- **Good:** it unblocks A2A `input-required` (RFC-0006 Phase 4 parks on exactly this awakeable)
  and is the prerequisite for Phase C.
- **Bad:** this is the **correctness-critical** path, and the migration window runs **two
  orchestration systems** over the same `tasks` table. Every incident during cutover starts with
  "which engine owned this task?" — mitigated by hard partition-at-creation + per-run engine
  stamping + dashboards, not eliminated.
- **Bad:** the journal-vs-code-evolution problem now bites the 2 h deep-review tier — a hotfix
  that reorders a journaled step breaks in-flight deep reviews. Mitigated by immutable-deployment
  versioning + the "awakeables for long waits, short handlers" discipline + the "never edit a
  journaled step sequence in a patch release" rollout rule; not free.
- **Neutral:** Restate becomes load-bearing for task dispatch, not just egress — the RocksDB
  StatefulSet's availability now gates *starting* work, not only *delivering* it. The `serve`
  webhook path keeps persisting `tasks` rows if the engine is briefly down, so work queues rather
  than drops (R6); a runbook is a Phase-B entry requirement.

### Risk register

Reuses [RFC-0005](../rfc/0005-durable-orchestration-on-restate.md)'s register where it applies,
sharpened for the lifecycle path. IDs align with the RFC.

| # | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| R2 | [sdk-rust #89](https://github.com/restatedev/sdk-rust/issues/89): concurrent `Context` use freezes a handler — and step 4's completion-vs-timeout race is where Phase B *needs* two awaited futures | Medium | High (stuck workflow on the critical path) | The race uses **only** the SDK's own select / `DurableFuturesUnordered` (0.10+), never a hand-rolled `futures` combinator over `Context`; verify #89 is resolved (or the select primitive is unaffected) **before** implementation — this is a hard entry gate for Phase B, above and beyond ADR-0074's |
| R4 | Journal replay vs code evolution: a hotfix reorders a journaled step and breaks in-flight 2 h deep reviews | Medium | Medium | Immutable-deployment versioning + operator drain of old revisions; short handlers, awakeables for the long wait; rollout rule: never edit a handler's journaled step sequence in a patch release |
| R8 | Migration-window confusion: two paths own tasks over one `tasks` table | High | Medium | Hard partition by creation flag (a task is born on exactly one engine and stays there); per-run engine stamping (`run_config_b64`-style); dashboards show engine-per-task; legacy tasks drain on the old path, never handed mid-flight |
| R11 | Index-gate cross-engine handoff drops a release (a legacy index fails to wake an engine-parked review, or vice-versa) | Medium | Medium (a review stalls parked) | All releases flow through the internal API, never engine-to-engine (see above); the legacy `release_reviews_waiting_on_index` already logs loudly on failure so a stall is visible; workflow-parked reviews also carry the deadline timer as a backstop |
| R5 | RocksDB PVC loss / corruption loses execution position for in-flight tasks | Low | High | Domain truth stays in Postgres — recovery re-submits unfinished tasks from `tasks` rows (the same reconciliation the reaper does today); PVC snapshots via the storage class |
| R6 | Restate server outage now halts *task dispatch*, not just egress (a broadened SPOF vs Phase A) | Low–Med | High | Single-node server is a supervised StatefulSet (restart = journal replay, no loss); `serve` keeps accepting webhooks and persisting `tasks` rows, so work queues rather than drops; runbook is a Phase-B entry requirement |
| R7 | Split-brain: a `ctx.run` wrote to Postgres, then the process died before the journal acked | Medium | Medium | Every `ctx.run` block is idempotent (status-guarded UPDATEs, upserts, `job_name`-idempotent launch — the codebase already writes this way); treat the journal as driver, Postgres as record |
| R1 | sdk-rust breaking changes on 0.x minors outpace us | High | Medium | Pin exactly; upgrade on our cadence with the compat matrix; handlers stay thin so churn surface is small |
| R10 | Phase A validated but Phase B stalls, leaving a permanent dual system (reaper *and* engine) | Medium | Medium | Phase B is gated on Phase A's exit gate first; hard partition means a stall is stable (each task path is self-contained), not corrupting; Phase C is a distinct go/no-go |

## Alternatives considered

- **Option B — move only the index-gate wait onto a durable promise, keep dispatcher/reaper.**
  Smaller blast radius, but it leaves the claim loop, lease, and reaper — most of the hand-rolled
  surface RFC-0005 targets — in place while already paying the dual-system tax. It also does not
  produce the runner-completion awakeable that A2A Phase 4 depends on. Rejected: it is half a
  migration for most of the risk.
- **Option C — a Restate `service` per dispatch instead of a `workflow`.** A service gives
  durable retries but no per-instance uniqueness key and no externally-resolvable durable
  promise; we would re-implement idempotency and an awakeable-address registry by hand — rebuilding,
  in the engine, the very primitives the `workflow` type provides. Rejected.
- **Do nothing / finish RFC-0001 by hand** (the unbuilt `scheduler`, outbox pruning). The proven
  path, but it re-derives durable-execution correctness for every future feature and never yields
  the awakeable A2A needs. This is RFC-0005's standing "real competitor"; the argument that the
  *third* re-derivation (A2A) tips the balance is made in the RFC and not re-litigated here.
- **Pilot Phase B first (instead of egress).** Rejected already by ADR-0074: this path is
  correctness-critical and carries the journal-evolution problem, so it must be learned on the
  bounded egress seam first. This ADR is the payoff of that sequencing, not a reversal of it.

## More Information

- [RFC-0005](../rfc/0005-durable-orchestration-on-restate.md) — the proposal this ADR implements
  Phase B of; the determinism rules and the base risk register live there.
- [ADR-0074](0074-restate-egress-pilot.md) — Phase A (deployed); the `PlatformEgress` object this
  workflow hands egress to, the `restate-worker` role that hosts this workflow, and the exit gate
  Phase B is gated on.
- [RFC-0006](../rfc/0006-a2a-agent-surface.md) — A2A; its Phase 4 (`input-required`) is gated on
  this workflow's awakeables.
- [ADR-0055](0055-review-waits-for-index-readiness.md) — the `WaitingForIndex` gate whose parking
  + release this workflow replaces with a durable promise.
- [ADR-0059](0059-reconciler-owns-all-github-egress.md) /
  [ADR-0058](0058-rename-poller-role-to-reconciler.md) /
  [ADR-0057](0057-poller-posts-failure-notice-on-uncatchable-kill.md) — the egress + failure-notice
  path the finalize/timeout branches route through (via Phase A).
- [ADR-0004](0004-one-k8s-job-per-task.md) / [ADR-0017](0017-agent-runner-control-plane-bootstrap.md)
  / [ADR-0002](0002-rust-control-plane-trust-boundary.md) — the unchanged execution and
  trust-boundary model: Restate orchestrates around the Job; the runner reports to the internal
  API and holds no Restate credentials.
- [ADR-0068](0068-reaction-driven-review-lifecycle.md) — the 👀/👍/👎/😕 reactions the workflow
  emits via `PlatformEgress` at launch and finalize.
- Current implementation being replaced:
  [`queue/dispatcher.rs`](../../services/control-plane/src/queue/dispatcher.rs),
  [`queue/reaper.rs`](../../services/control-plane/src/queue/reaper.rs),
  [`db.rs`](../../services/control-plane/src/db.rs) (`claim_next_task`, `create_task` /
  `create_explicit_task`, `INITIAL_TASK_STATUS_SQL`, `release_reviews_waiting_on_index`),
  [`http/internal.rs`](../../services/control-plane/src/http/internal.rs) (the runner completion
  report that will resolve the awakeable), and
  [`restate_worker.rs`](../../services/control-plane/src/restate_worker.rs) (the host role).
