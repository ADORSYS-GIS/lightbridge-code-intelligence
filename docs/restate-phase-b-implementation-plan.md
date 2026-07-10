# Restate Phase B — implementation plan (ADR-0076)

> **⛔ DO NOT START IMPLEMENTATION UNTIL BOTH GATES ARE GREEN.**
>
> This is a *planning* artifact. No `.rs` file changes on this branch. ADR-0076 gates its own
> build behind two hard entry conditions, and neither is met today:
>
> 1. **Phase A exit gate (ADR-0074) must pass live.** ≥ 3 weeks in prod with zero lost/duplicate
>    posts, the dead-letter branch exercised, and one `restate-sdk` upgrade absorbed. Phase A is
>    **deployed but flag-OFF** (`egress.mode = drain` by default — `restate-worker` serves
>    `PlatformEgress` but nothing invokes it). The soak clock has **not started**. → **NOT MET.**
> 2. **The completion-vs-timeout race primitive must be verified safe** against
>    [restatedev/sdk-rust#89](https://github.com/restatedev/sdk-rust/issues/89). See §1 — the gate as ADR-0076
>    literally words it ("restatedev/sdk-rust#89 resolved") is **BLOCKED** (issue open), but the substantive risk is
>    likely mis-anchored; read §1 before acting on it.
>
> This document is the "ready when it is" build sequence: it turns ADR-0076's design into a
> reviewed, file-by-file plan so the owner can pull the trigger once the gates flip, not before.

- **Design of record:** [ADR-0076](adr/0076-restate-task-lifecycle-workflow.md) — *the task lifecycle
  becomes a workflow*. This plan does not re-decide anything ADR-0076 decides; it grounds the design
  in the current code and sequences the build.
- **Umbrella proposal:** [RFC-0005](rfc/0005-durable-orchestration-on-restate.md) (strangler adoption
  of Restate); [ADR-0074](adr/0074-restate-egress-pilot.md) is Phase A (deployed).
- **Status:** Plan only. Reviewed by the author; the gates are **not** approved and the build is
  **not** authorized (that is the owner's call).

---

## 1. Entry-gate checklist

All must be green *before any code lands.* Today: **one BLOCKED, one NOT MET.**

### 1.1 restatedev/sdk-rust#89 — the completion-vs-timeout race primitive (ADR-0076 risk R2)

**Verdict: BLOCKED as literally worded; but the gate is probably mis-framed — read the mechanism.**

**Issue state (verified 2026-07-09 via `gh issue view 89 --repo restatedev/sdk-rust`):**

- **Status: OPEN.** Title: *"Concurrent use of Context in stream causes a freeze."* Opened
  2026-02-11; last activity 2026-02-23. No labels, no linked PR, no milestone, not closed.
- **Reporter's repro:** `StreamExt::buffered(n)` with `n ≥ 2` over futures that each use the Restate
  `Context` freezes the handler indefinitely after the first items; `buffered(1)` is fine. Seen on
  restate-sdk 0.7 and 0.8, Restate server 1.6.1.
- **Maintainer diagnosis (tillrohrmann, CONTRIBUTOR, 2026-02-23):** this is **working-as-designed**,
  not a fixable bug. Restate's `Context` "cannot be used concurrently as the underlying journal is
  index based and every run of the handler (including the replay) needs to be deterministic." A
  `buffered(n>1)` stream uses `FuturesUnordered` under the hood to race futures, which is inherently
  non-deterministic and corrupts the index-based journal. Recommendation: **don't use `n > 1`.** The
  freeze itself is an internal concurrent-use guard tripping, not a race that a patch will remove.

**Why the ADR's own R2 mitigation is the actual answer, and why restatedev/sdk-rust#89 does not block it:**

ADR-0076 already says the race must use "the SDK's own select / `DurableFuturesUnordered` (0.10+),
never a hand-rolled `futures` combinator over `Context` operations." That is the correct — and
categorically different — mechanism, and it is **present in our exact pin, restate-sdk 0.10.0**:

- `restate_sdk::prelude::DurableFuturesUnordered` is re-exported
  (`restate-sdk-0.10.0/src/lib.rs:519`, `src/context/mod.rs:18`).
- Its `next()` does **not** poll multiple `Context` operations concurrently. It collects the
  in-flight durable-future *handles*, then calls the engine's own **`ctx.select(handles)`**, which
  the engine journals as a single deterministic "which completed first" decision, and only then
  awaits the winner (`restate-sdk-0.10.0/src/context/select_any.rs:104-122`). The completion order is
  chosen by the journal on replay, not by wall-clock task-poll order.

So the completion-vs-timeout race in step 4 (await the runner awakeable **vs** a `ctx.sleep`
deadline) is exactly the two-handle case `ctx.select` exists for. It never constructs a
`FuturesUnordered`/`buffered` over `Context` ops, so it never enters the code path restatedev/sdk-rust#89 describes.
**restatedev/sdk-rust#89 and the select primitive are different mechanisms;** the open issue is evidence *against*
hand-rolled combinators (which we already forbid), not evidence against `ctx.select`.

**Therefore, calibrated:**

- **The gate as ADR-0076 phrases it ("verify restatedev/sdk-rust#89 is resolved before implementation") will likely
  never go green** — by the maintainer's own diagnosis restatedev/sdk-rust#89 is a by-design constraint, not a bug
  awaiting a fix, and may sit open indefinitely. Treating "restatedev/sdk-rust#89 closed" as the trigger would deadlock
  Phase B forever.
- **The gate should be re-scoped** to its real intent: *"the completion-vs-timeout race uses only
  `ctx.select` / `DurableFuturesUnordered`, and that primitive is verified safe under crash/replay on
  our pinned server+SDK."* On paper that sub-gate reads **PASS** (the primitive exists, is journaled,
  and sidesteps restatedev/sdk-rust#89's mechanism).
- **What is NOT yet proven, and blocks the build:** this assessment is a **source read, not a
  runtime proof.** No one has yet run the two-handle awakeable-vs-`ctx.sleep` race against a live
  Restate server, killed the worker mid-race, and confirmed the journal replays to the same winner.
  That live proof is a **build-entry deliverable** (see §5 test strategy, "completion-vs-timeout
  race"). Until it exists, treat R2 as **open**.

**Owner action:** decide whether to keep ADR-0076's literal "restatedev/sdk-rust#89 resolved" wording (which will not
happen) or amend R2 to the re-scoped primitive-safety gate above. This plan assumes the latter.

### 1.2 Phase A exit gate (ADR-0074) — **NOT MET**

ADR-0076's Decision Outcome gates Phase B on Phase A's exit gate passing live: ≥ 3 weeks in prod,
zero lost/duplicate posts, dead-letter branch exercised, one SDK upgrade absorbed.

Current reality: `restate-worker` is deployed and serves `PlatformEgress`, but the default
`egress.mode = drain` (`config.rs:39-55`, `egress.rs:73` — `EgressMode::Drain` is the default and the
reconciler drain remains the sole egress) means **nothing invokes it in prod**. The soak has not
begun. This gate is a hard prerequisite and is **NOT MET**; everything below is "ready when it is."

Sub-conditions to track once the flag flips to `restate` in prod:

- [ ] ≥ 21 days continuous with `egress.mode = restate`.
- [ ] Zero lost posts and zero duplicate posts over the window (cross-check outbox `posted` vs
      platform-side comment counts on the observability dashboards, ADR-0046).
- [ ] The dead-letter branch (`dead_letter_step`, `restate_worker.rs:262`) fired at least once on a
      real permanent failure and parked the row `failed` without a re-post.
- [ ] One `restate-sdk` patch/minor bump absorbed with the server-compat matrix checked.

### 1.3 `restate-sdk` version pin — **OK**

- Workspace pin: `Cargo.toml:46` → `restate-sdk = "=0.10.0"` (exact pin, per ADR-0074's discipline).
- `services/control-plane/Cargo.toml:41` → `restate-sdk.workspace = true`.
- Lockfile: `Cargo.lock` → `restate-sdk 0.10.0`.
- 0.10.0 ships the select primitives Phase B needs (`DurableFuturesUnordered`, `ctx.select`,
  `ctx.sleep`, `RunRetryPolicy`). **No version bump is required to build Phase B** — one more reason
  R2 is about a primitive we already have, not a pending release.

---

## 2. File-by-file change map (grounded in current code)

Every path below is `services/control-plane/src/…`. Line ranges are from `main` at the time of
writing (commit context: post-#314); treat them as anchors, not literals.

### 2.1 `restate_worker.rs` — **ADD** the `run` workflow (host role already exists)

- Today it serves `Health` and `PlatformEgress` (`restate_worker.rs:56-204`), binds them in `run`
  (`:285-327`), and enforces the determinism discipline in its module doc (`:15-30`). It is the
  correct host — ADR-0076 §Decision Outcome puts the workflow here, "the same binary and Deployment
  ADR-0074 stood up."
- **Add a `#[restate_sdk::workflow] trait TaskRun { async fn run(...) }`** and a `TaskRunImpl` that
  holds the same cheap clones `PlatformEgressImpl` holds (`pool`, `platforms`, `review` —
  `:102-106`), plus a `TaskLauncher` handle and the k8s liveness client the reaper uses. Bind it in
  `run` next to `PlatformEgress` (extend the `match` at `:298-314`).
- **Reuse, do not duplicate:** the small-journaled-result rule (`DeliverOutcome`, `:110-116`), the
  factored pure-decision pattern (`preflight`, `:122-136` → mirror it for the reaper `decide` reuse),
  and the `.name(...)`-per-`ctx.run` convention (`:163,184,198`). The handler is longer than
  `PlatformEgress::post`, so these bind harder (ADR-0076 §Determinism rules).
- The `PlatformEgress` object stays exactly as is; the workflow **calls into it** for egress
  (step 5) rather than re-implementing delivery.

### 2.2 `queue/dispatcher.rs` — **REPLACED** for engine-owned tasks (not deleted in Phase B)

- The claim loop `drain` → `claim_next_task` → `dispatch` → `launcher.launch` + `set_task_job` +
  `react_work_started` (`dispatcher.rs:243-349`) is exactly what the workflow's steps 1/3 subsume.
- Phase B does **not** delete this file. Per ADR-0076 §Migration, legacy tasks keep draining here; the
  dispatcher simply stops *claiming* engine-owned rows (see §4 — `owner_engine` predicate in
  `claim_next_task`). Deleting the dispatcher/reaper/`waiting_for_index`/`LISTEN,NOTIFY` is **Phase C**
  (a separate ADR), only after the legacy backlog drains.
- `react_work_started` (`:309-349`) — the 👀 work-started reaction — is **ported into workflow step 3**
  (enqueue via `PlatformEgress`), not called from the dispatcher for engine-owned tasks.

### 2.3 `queue/reaper.rs` — **PORTED** into workflow step 4's timeout branch

- The pure policy is already isolated and unit-tested: `decide(liveness, attempts, max_attempts)`
  (`reaper.rs:55-67`), `requeue_backoff(attempts)` (`:70-74`), `MAX_ATTEMPTS = 5` (`:32`),
  `BACKOFF_BASE`/`BACKOFF_CAP` (`:38-39`). ADR-0076 step 4 says these "carry over unchanged."
- Port `decide`/`requeue_backoff`/the constants into (or `pub use` from) the workflow module. They are
  **pure functions of `(liveness, attempts)`** — no wall-clock, no RNG — so they satisfy the
  "deterministic outside `ctx.run`" rule directly (ADR-0076 §Determinism, which explicitly names
  `requeue_backoff` as porting cleanly).
- The *effects* keyed off each `ReapAction` (`reap_once`, `:77-178`) become the branches of step 4:
  - `RenewLease` → **not needed as a DB lease op**; in the workflow, "still Active" means loop back to
    the `ctx.select` await (the workflow costs nothing while parked — there is no lease to renew).
  - `MarkSucceeded` → treat the timer-win-on-a-`Succeeded`-Job as a lost success report: finalize,
    **never re-run/re-post** (the `reap_marks_completed_job_succeeded` invariant, `:441-460`).
  - `Requeue` → `delete_dead_job` (`:182-188`) then loop back to step 3, gated by `attempts`.
  - `Fail` → failure notice + 😕 via `PlatformEgress` (the `enqueue_reaper_failure_notice` body,
    `:195-234`, which already routes through the egress outbox).
- The `list_cancelled_with_job` sweep (`:155-176`) is a *cross-cutting* reaper duty (a closed PR stops
  its Job). During cutover it still runs in the legacy reaper for legacy tasks; for engine-owned tasks
  cancellation is an **awakeable-driven** signal into the workflow (a future increment — note it,
  don't build it into the first cut unless PR-close cancellation of engine tasks is in scope).

### 2.4 `db.rs` — **REPARTITIONED**, not rewritten

- `claim_next_task` (`db.rs:1601-1624`): add the `owner_engine` skip predicate (§4). The
  `FOR UPDATE SKIP LOCKED` claim is unchanged for legacy rows.
- `create_task` / `create_explicit_task` (`:1281-1370`): the idempotency INSERT + `ON CONFLICT DO
  NOTHING` (`:1287-1288`) and the `23505`-retry epoch loop (`:1362-1366`) are what workflow-instance
  uniqueness replaces (ADR-0076 §The `run` handler: workflow ID = the `tasks_idempotency_idx` tuple +
  `run_epoch`). For engine-owned tasks, the producer still INSERTs the row (audit/reporting) but
  stamps it engine-owned and submits the workflow instead of relying on the conflict machinery to
  dedup. **Keep both functions** — legacy path still uses them.
- `INITIAL_TASK_STATUS_SQL` (`:1251-1254`): the `waiting_for_index` `EXISTS` gate becomes workflow
  step 2's durable promise for engine-owned tasks. The SQL stays for legacy rows.
- `release_reviews_waiting_on_index` (`:2097-2137`) + its `set_task_status` trigger (`:2162-2165`):
  this is the cross-engine handoff (§4). It stays and gains the "also resolve the awaiting workflow's
  promise via the internal API" behaviour for the *legacy-index → new-review* straddle.
- New: `create_task`/`create_explicit_task`/`create_index_task` gain an engine stamp; a small
  `owner_engine`-aware helper decides submit-workflow vs NOTIFY (§4).

### 2.5 `http/internal.rs` — **HOOK** the awakeable resolution (runner contract unchanged)

- The runner's terminal report lands at `set_status` (`internal.rs:1564-1611`) → `set_task_status`
  (`db.rs:2139-2167`), and the review flush at `finalize_review` (`:1086`). ADR-0076 step 4: the
  runner **does not change** and holds no Restate creds — the **control plane** resolves the workflow
  awakeable after it authenticates and applies the report.
- Add, inside `set_status` (and/or `finalize_review`) after a successful `set_task_status`, a call
  that resolves the workflow's runner-completion awakeable by its awakeable id — but **only for
  engine-owned tasks** (look up `owner_engine`; a legacy task has no awakeable to resolve, and the
  existing `handle_review_failure` spawn at `:1596-1602` stays for legacy failures). The resolve is
  the same "control-plane-resolves-a-durable-promise" mechanism the index-gate release uses (§4).
- The awakeable id must be recoverable from the task id — store it on the `tasks` row (or derive it
  from the workflow ID) when the workflow registers it in step 4, so `set_status` can look it up.

### 2.6 `serve` webhook producer path — **INSERT + submit-workflow** instead of INSERT + NOTIFY

- Callers today: `create_review_task`/`create_explicit_review_task` (`http/webhook.rs:931-960`),
  auto-index (`webhook.rs:438,777`), admin re-index (`http/admin.rs:174`), A2A
  (`a2a/handler.rs:284,905,1177`). All funnel into `db::create_task`/`create_explicit_task`/
  `create_index_task`, which end in `notify_or_log_initial_status` → `pg_notify(TASK_QUEUED_CHANNEL)`
  (`db.rs:1256-1274`).
- For engine-owned tasks: keep the INSERT (the row is the durable audit/reporting record from moment
  zero — ADR-0076 §producer side) but **replace the NOTIFY with a workflow submit** keyed on the
  workflow ID (idempotency key = the `tasks_idempotency_idx` tuple + `run_epoch`). Submitting the same
  key twice is a no-op — that is the dedup, replacing `ON CONFLICT`/`23505`.
- This is the one producer change; mirror the `EgressMode::{Drain,Restate}` config-flag shape
  (`config.rs:39-55`) so the engine-vs-dispatcher dispatch path is a **flagged, reversible** choice,
  exactly as ADR-0074 made egress reversible.

---

## 3. The 5-step `run` handler sketch (with the determinism rule each step obeys)

Signature (workflow): `async fn run(&self, ctx: WorkflowContext<'_>, req: TaskRunReq) -> HandlerResult<()>`.
Workflow ID = `repository_id, target_type, target_id, command_text, head_sha, run_epoch` (the
`tasks_idempotency_idx` columns; ADR-0076). Each numbered step is a **named** `ctx.run` unless noted.

1. **Attach the `tasks` row** — `ctx.run("attach", …)`.
   Upsert the domain row for this workflow ID (idempotent; the row may already exist from the
   producer INSERT). `status` becomes **derived/reporting-only** — Grafana keeps reading it, the
   journal drives execution.
   *Rule:* all I/O inside `ctx.run`; the journaled result is tiny (the task id / a status enum), never
   a payload. No `Context` inside the closure.

2. **Index gate** — durable promise (**not** a `ctx.run` side-effect; a suspended await).
   If this is a non-`index` task and an index task is in flight for the repo (today's
   `INITIAL_TASK_STATUS_SQL` `EXISTS`, `db.rs:1251-1254`), **await a durable promise** the repo's
   index workflow resolves on completion. Replaces `waiting_for_index` parking +
   `release_reviews_waiting_on_index`. Costs nothing while parked.
   *Rule:* the wait is an awakeable/promise, not a busy loop or a `ctx.run` poll; the "is an index in
   flight?" probe that *decides whether to wait* is itself a small `ctx.run`.

3. **Launch the Job** — `ctx.run("launch", …)`.
   `TaskLauncher::launch` (idempotent by `job_name`, derived from the task id — `dispatcher.rs:274`,
   `set_task_job` `db.rs:1627`), so a replay between "launched" and "journaled" does not double-launch.
   Enqueue the 👀 work-started reaction here via `PlatformEgress` (ported `react_work_started`,
   `dispatcher.rs:309-349`).
   *Rule:* the journaled result is just `job_name` (a short string), never the launch spec. No
   `Context` in the closure.

4. **Await completion, racing a deadline** — `ctx.select` over two durable futures.
   Await the **runner-completion awakeable** (resolved by the control plane in `set_status`, §2.5)
   **vs** a `ctx.sleep(active_deadline + slack)` durable timer. Use **`DurableFuturesUnordered` /
   `ctx.select`** (0.10.0), **never** a `futures` combinator over `Context` (the restatedev/sdk-rust#89 mechanism, §1).
   - **Awakeable wins →** the report carries the terminal status → step 5.
   - **Timer wins →** the timer firing does **not** prove death. Run `decide(liveness, attempts,
     MAX_ATTEMPTS)` (ported reaper, §2.3) after a `ctx.run("job_liveness", …)` real k8s liveness
     check: `Active` → loop back to the step-4 await (no lease to renew); `Succeeded` → lost success
     report, settle without re-run; `Failed`/`Gone` with attempts left → `delete_dead_job` in a
     `ctx.run` then loop to step 3; attempts exhausted → fail + failure notice via `PlatformEgress`.
   *Rule:* **deadlines are `ctx.sleep`, never `Instant::now()`** (contrast the legacy dispatcher's
   `Instant::now()` at `dispatcher.rs:273`, which must NOT appear in the handler). The race is the one
   place Phase B awaits two futures — it must go through the SDK's journaled select. `decide` /
   `requeue_backoff` are pure, so they may run outside `ctx.run`; the liveness probe and job delete
   are I/O, so they are each their own `ctx.run`.

5. **Finalize + egress** — `ctx.run("finalize", …)` then hand off to `PlatformEgress`.
   Finalize the domain rows (`finalize_review` / `set_task_status` writes), then hand egress intents
   (verdict reaction 👍/👎/😕, review body, any failure notice) to the `PlatformEgress` object — which
   is *already* durable and serialized per installation (Phase A). If this is an **index** workflow,
   resolve the awaiting review workflows' promises here (step 2's counterpart) and, for any
   dispatcher-parked legacy reviews, perform the `release_reviews_waiting_on_index` UPDATE in this
   `ctx.run` (§4).
   *Rule:* finalize writes are status-guarded/idempotent (R7 split-brain: journal drives, Postgres
   records); egress is a call into another durable object, not inline posting; journaled result stays
   small.

**Handler-length discipline (R4):** anything long-lived (a 2 h deep review) hangs on the step-4
awakeable, **not** a long code path — so the journaled step *sequence* stays short and stable, which
is what makes "never edit a journaled step sequence in a patch release" enforceable (§5 runbook).

---

## 4. Migration / cutover mechanics (strangler; no in-place table migration)

**Both systems write the same `tasks` rows; exactly one system owns any given task, decided at
creation.** No in-place migration of live rows.

### 4.1 `owner_engine` partition column

- New migration **`0026_task_owner_engine.sql`** (latest is `0025_a2a_tasks.sql`): add
  `owner_engine text NOT NULL DEFAULT 'dispatcher'` (values `'dispatcher' | 'restate'`) to `tasks`.
  All existing rows default to `'dispatcher'` — a pure add-column, no backfill of behaviour.
- `claim_next_task` (`db.rs:1601-1624`): add `AND owner_engine = 'dispatcher'` to the inner
  `SELECT … FOR UPDATE SKIP LOCKED` so the dispatcher **never claims engine-owned rows**. Equivalent
  fallback ADR-0076 allows: never insert engine-owned rows in a claimable status — the explicit
  predicate is clearer and cheaper to reason about during an incident.
- `list_reapable_tasks` (the reaper's candidate query, `db.rs:~1664`) gets the same
  `owner_engine = 'dispatcher'` guard so the legacy reaper never reconciles an engine-owned task.

### 4.2 Per-run engine stamping

- Stamp each run with its owning engine (a `run_config_b64`-style marker, per RFC-0005 R8 and the
  existing review-run telemetry, migration `0023_review_run_telemetry.sql`) so dashboards show
  engine-per-task and an incident starts from a known path, not a guess.
- The producer chooses the engine behind a config flag mirroring `EgressMode` (`config.rs:39-55`):
  default `dispatcher` (no behaviour change on merge), flip to `restate` to route **new** tasks to the
  workflow. Reversible: flip back and new tasks go to the dispatcher again; in-flight tasks are
  unaffected because ownership is fixed at creation.

### 4.3 In-flight legacy tasks drain on the old path

A 2 h deep review that started on the dispatcher **finishes** on the dispatcher — never handed
mid-flight to the engine. The dispatcher claim loop + reaper keep running and reconciling every
pre-cutover row until the legacy backlog is empty. Deleting them is Phase C.

### 4.4 Cross-engine `waiting_for_index` release — always via the internal API, never engine-to-engine (all three straddle cases)

The index gate is the one cross-engine handoff (an index task and the review it gates can straddle the
cutover). ADR-0076 §"`waiting_for_index` release across engines":

- **Both new (workflows):** the review workflow awaits its step-2 durable promise; the index workflow
  resolves it in step 5. Pure engine-internal — no DB release.
- **Index legacy, review new:** the legacy index completes on the old path;
  `set_task_status → release_reviews_waiting_on_index` (`db.rs:2162-2165`, `:2097-2137`) additionally
  **resolves the awaiting workflow's promise via the internal API** — the same control-plane-resolves
  mechanism the runner report uses (§2.5). A dispatcher-run index still wakes an engine-parked review.
- **Index new, review legacy:** the index workflow, on completion (step 5), performs the existing
  `release_reviews_waiting_on_index` UPDATE (`status = 'queued'`, `run_after = now()`) **inside a
  `ctx.run`**, then the dispatcher claims the flipped review as usual.

This keeps ADR-0055's semantics intact regardless of which engine owns either side. The existing
loud-on-failure log in `release_reviews_waiting_on_index` (`db.rs:2114-2121`) stays as the stall
detector; engine-parked reviews also carry the step-4 deadline timer as a backstop (R11).

---

## 5. Test strategy (for when it is built) + rollout/runbook requirements

### 5.1 Tests

- **Workflow-level (happy path):** submit a `run` workflow against a live Restate server; the runner
  awakeable resolves → finalize + egress fire; the `tasks` row reaches `succeeded`; exactly one Job,
  exactly one posted review. (Mirrors the `PlatformEgress` post-merge live check, #297/#296.)
- **Completion-vs-timeout race (R2 — the build-entry proof, §1.1):** drive step 4 with (a) awakeable
  resolves before the deadline, (b) deadline fires first with the Job `Active` (→ loop, no re-run),
  (c) deadline fires with the Job `Succeeded` (→ lost-report settle, **no re-post** — the
  `reap_marks_completed_job_succeeded` invariant, `reaper.rs:441-460`), (d) deadline with
  `Failed`/`Gone` under and at `MAX_ATTEMPTS` (→ requeue vs fail). **Crucially: kill the worker
  mid-race and confirm the journal replays to the same winner** — this is the live evidence the R2
  gate needs and does not yet exist. Assert `ctx.select` / `DurableFuturesUnordered` is the only race
  primitive (a lint/review check that no `FuturesUnordered`/`buffered` touches `Context`).
- **Idempotency via workflow-instance uniqueness:** submit the same workflow ID twice (e.g. GitHub
  `opened` then `synchronize` for one head) → one execution, one Job, one review. This is the
  replacement for `create_task`'s `ON CONFLICT DO NOTHING` and `create_explicit_task`'s `23505` loop
  (`db.rs:1287-1288`, `:1362-1366`); port those functions' existing tests as the oracle.
- **Cross-engine index-gate release (all three §4.4 cases):** especially *index-legacy → review-new*
  (dispatcher completion resolves an engine promise via the internal API) and *index-new →
  review-legacy* (workflow does the `release_reviews_waiting_on_index` UPDATE). Assert no review
  stalls parked. Reuse the ADR-0055 release tests as the oracle.
- **Split-brain / replay (R7):** a `ctx.run` writes to Postgres then the process dies before the
  journal acks → on replay the step re-runs and is a no-op (status-guarded UPDATE / upsert /
  `job_name`-idempotent launch). Every step body must be independently replay-safe.
- **Reuse the ported pure policy tests verbatim:** `decide`/`requeue_backoff` already have full unit
  coverage (`reaper.rs:240-294`); they must pass unchanged after the port (that is the point of R2's
  "carry over unchanged").

### 5.2 Rollout / runbook (ADR-0076 names these as **Phase-B entry requirements**)

- **Immutable-deployment versioning (R4):** register each handler revision as a new immutable
  deployment; drain old revisions on their pinned revision. **Never edit a journaled step sequence in
  a patch release** — reordering/adding/removing a `ctx.run` step breaks in-flight 2 h deep reviews.
  This is why the handler stays short and long waits hang on awakeables (§3).
- **Server-outage = queues, not drops (R6):** `serve` keeps accepting webhooks and persisting `tasks`
  rows even if the Restate server (the RocksDB StatefulSet) is briefly down — work queues rather than
  drops. The runbook must state: on engine outage, submits buffer as rows; on recovery, re-submit
  unfinished tasks from `tasks` (the same reconciliation the reaper does today, R5). A runbook for
  this is an explicit entry requirement (ADR-0076 Consequences, Neutral).
- **Engine-per-task observability:** dashboards must show `owner_engine` per task before cutover so
  every incident starts from "which engine owned this task?" (§4.2, R8).
- **Reversibility drill:** confirm flipping the dispatch flag back to `dispatcher` routes new tasks to
  the old path with zero effect on in-flight engine tasks (ownership fixed at creation), mirroring the
  `egress.mode` flip-back Phase A already supports.

---

## 6. Risks & unknowns in *this plan* (what to sanity-check)

This is a plan, so the risk is **plan inaccuracy**, not runtime failure. Lower-confidence points:

1. **R2 framing (highest).** ADR-0076's literal "verify restatedev/sdk-rust#89 resolved" gate will likely never go green
   (§1.1 — the maintainer calls it by-design). I re-scoped it to "the `ctx.select` primitive is
   verified safe," which reads PASS *on a source read* but has **no live crash/replay proof yet**. If
   the owner insists on the literal wording, Phase B is blocked indefinitely on an issue that will not
   close. Sanity-check: is re-scoping R2 acceptable, and is the live race test accepted as the real
   gate?
2. **Awakeable id storage/lookup (§2.5).** The plan assumes the control plane can recover the workflow
   awakeable id from the task id at `set_status` time (store it on the `tasks` row or derive from the
   workflow ID). I did not find an existing awakeable-registry mechanism in the code (`PlatformEgress`
   uses none). Whether the SDK's awakeable-id addressing supports "resolve from an external process by
   a stored id" for *workflow* promises (vs the `PlatformEgress` object model) needs confirming
   against the 0.10.0 API before this is load-bearing.
3. **Cancellation of engine-owned tasks (§2.3).** The legacy reaper's `list_cancelled_with_job` sweep
   (closed-PR → stop Job) has no obvious engine equivalent in ADR-0076's 5-step sketch; I flagged it
   as an awakeable-driven signal but did not design it. If PR-close cancellation of *engine* tasks is
   in first-cut scope, that is an unspecified sixth concern.

Secondary unknowns: exact `owner_engine` column vs "never claimable status" choice (§4.1 — I picked
the explicit predicate); whether index-gate "is an index in flight?" is best a `ctx.run` probe or a
producer-time decision; and the precise `WorkflowContext` vs `Context` API surface in 0.10.0 for
promises (the sketch assumes workflow durable promises exist and are externally resolvable).

---

## More Information

- [ADR-0076](adr/0076-restate-task-lifecycle-workflow.md) — the design this plan implements.
- [ADR-0074](adr/0074-restate-egress-pilot.md) — Phase A, whose exit gate this plan waits on.
- [RFC-0005](rfc/0005-durable-orchestration-on-restate.md) — the strangler proposal and base risk
  register.
- restatedev/sdk-rust#89 — `https://github.com/restatedev/sdk-rust/issues/89` (open; see §1.1 for the calibrated
  read).
