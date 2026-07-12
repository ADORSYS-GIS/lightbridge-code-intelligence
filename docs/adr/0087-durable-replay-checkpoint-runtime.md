# ADR-0087: Durable replay via `CheckpointRuntime` (the loop resumes from storage, not Restate)

- **Status:** Proposed
- **Date:** 2026-07-12
- **Deciders:** @stephane-segning
- **Supersedes:** the `RestateRuntime`-in-the-loop direction of
  [ADR-0082](0082-restate-durable-agent-runtime.md) (Option A). ADR-0082's **R1 extraction stands
  unchanged**; only its R2 endpoint changes.

## Context and Problem Statement

The agent loop must **resume at the step it left**: fail at turn 47 of 150 and the next run should
replay turns 0–46 from storage and continue *live* at 47 — offline, retryable execution state, not
retry/backoff. [ADR-0082](0082-restate-durable-agent-runtime.md) chose to get this by running the
loop **inside Restate** (`RestateRuntime`). But the agent-plane's hard requirement is a per-task
**isolated, growable checkout** ([RFC-0007](../rfc/0007-control-plane-v2-planes.md),
[ADR-0085](0085-agent-execution-plane.md)) — `read_file` today, `grep`/`find_files` next, code
*execution* for [`open`](0088-open-mode-autonomous-ticket-agent.md) — and every way to host the loop
inside Restate collides with that isolation (shared worker → co-resident checkouts; orchestrator/
executor split → distributed monolith; git-API reads → no `grep`, rate limits; per-task endpoint →
fights immutable deployments). How do we get resume **without** contorting the loop into Restate?

## Decision Drivers

- **Resume, not restart** — crash cost O(one step), not O(run), in tokens and wall clock.
- **Preserve per-task isolation** (RFC-0007) — the solution must live *inside* the isolated
  execution unit, not force a shared host.
- **Keep the trust boundary** — the agent-plane holds no DB; it may only journal *through* the
  mediated internal API ([ADR-0037](0037-agent-acts-via-mediated-tools.md)).
- **Bounded self-ownership** — [RFC-0005](../rfc/0005-durable-orchestration-on-restate.md)'s doctrine
  is *stop hand-rolling durability*; any home-grown replay must be small and localized, not a second
  sprawling engine.
- **Self-purging** — success deletes the state; a configurable TTL removes the rest.

## Considered Options

- **Option A — `RestateRuntime`: run the loop as a Restate invocation** (ADR-0082's original R2).
- **Option B — `CheckpointRuntime`: persist each step's result to a `durable_step` store, replay on
  requeue** (this ADR; ADR-0082's named third `StepRuntime` impl, formerly the fallback).
- **Option C — no mid-loop durability**: Restate owns egress + task lifecycle only; a pod death
  requeues the whole loop from turn 0.

## Decision Outcome

Chosen option: **Option B — `CheckpointRuntime`**, the third implementation of the existing
`StepRuntime` seam ([ADR-0082](0082-restate-durable-agent-runtime.md) §The code revamp), alongside
`Passthrough` (no durability) and the now-retired-for-the-loop `RestateRuntime`.

**Mechanism.** `step(name, f)` first asks the `replay` role (via the mediated internal API) whether
a result exists for `(task_id, run_epoch, step_name)`. If yes, it returns the stored result — the
effect is **not** re-executed. If no, it runs `f`, persists the result, and returns it. On a pod
death the dispatcher/reaper requeues the **same `run_epoch`**; the fresh execution unit re-runs the
loop from turn 0, but each `step` replays from storage until the first gap (turn 47), then continues
live. The loop's derived state (conversation, budget counters, wind-down, coverage) is *re-derived*
from the replayed step results — it is the deterministic glue between steps, not journal data (the
same model ADR-0082 specified). Step names are the existing constants (`llm_turn:{n}`, `tools:{n}`,
`write_tool:{n}:{id}`), already a stability-tested contract.

**Why this over Restate for the loop.** When *every* implementation of "Restate runs the loop" is
rejected by the isolation requirement, Restate is the wrong tool *for the loop*. `CheckpointRuntime`
lives entirely inside the isolated execution unit (Job or single-tenant `serve` replica), so it
needs no shared host and no per-task Restate endpoint. It is the same journaling model — persist
step results, replay them — with the journal in a table we own instead of the engine's, and none of
the engine's operational surface (no worker deployment, no immutable-revision drains, no SDK pin, no
h2c discovery) intruding on the loop.

**Restate is not removed — it is scoped.** It remains the durable substrate for **egress** (Phase A,
live — [ADR-0074](0074-restate-egress-pilot.md)) and the intended home for the **task lifecycle**
([ADR-0076](0076-restate-task-lifecycle-workflow.md)) and A2A durable promises. Egress is where its
value is structural (single-writer per `platform:installation`, at-least-once) and independent of
its UI. We do **not** fragment the *orchestration* durability model; we decline to extend it into
the *loop*, where it never fit.

### The `durable_step` store

Postgres, **execution-state only** — the RFC-0005 line: we own *execution state*, Postgres owns
*domain data*. Owned by the orchestration `replay` role; written by the agent-plane only through the
mediated internal API (so the agent pod keeps no DB credential).

| Column | Meaning |
|---|---|
| `task_id`, `run_epoch` | the run identity ([ADR-0076](0076-restate-task-lifecycle-workflow.md) idempotency tuple) |
| `step_name` | `llm_turn:{n}` / `tools:{n}` / `write_tool:{n}:{id}` (the stability-tested contract) |
| `result` \| `offload_ref` | the journaled result, or a content-hashed pointer for over-cap payloads (the [ADR-0082](0082-restate-durable-agent-runtime.md) offload rule) |
| `content_hash` | replay verifies it rehydrates the same bytes |
| `created_at` | drives the TTL sweep |

Unique on `(task_id, run_epoch, step_name)` — the replay-idempotent upsert key.

### Purge and retention (configurable)

- **On success:** `finalize` deletes `WHERE task_id = ? AND run_epoch = ?` once the review/PR is
  committed and egress is handed off — the completed run's execution state is gone immediately.
- **TTL sweep:** the `replay` role periodically deletes rows older than a **configurable retention**
  (`DURABLE_STEP_RETENTION`, default e.g. 6 h), **success or failure** — the backstop for abandoned,
  failed, or cancelled runs, mirroring the outbox prune and the k8s Job `ttlSecondsAfterFinished` it
  functionally replaces for the loop. **The retention MUST be validated `> 0` at load** (reject/clamp a
  zero or negative value): a misconfigured `0` would make the age cutoff `now()` and sweep *every*
  in-flight run's state, silently disabling resume — a config footgun the loader guards against, not a
  runtime surprise.

### Idempotency of side-effectful steps (the at-least-once seam)

`step` journals the *result*, not the *effect*: die after the effect but before the persist and
replay re-executes it — inherent to at-least-once, and true of Restate too. Per step:

| Step | Effect | Replay behavior | Verdict |
|---|---|---|---|
| `llm_turn` | Paid gateway call | Re-pays **one** turn (vs. all turns) | Accepted window |
| retrieval / `read_file` / `grep` | Read-only | Harmless | Safe |
| `add_review_comment` | Buffers a finding | Last-write-wins per `(file, line)` | Safe as-is |
| `add_comment` | Appends a reply | **Duplicates** — needs dedup key `(task_id, run_epoch, call_id)` | Required |
| `propose_pr` ([open](0088-open-mode-autonomous-ticket-agent.md)) | Hands a branch to egress | Dedup key `(task_id, run_epoch)` → egress opens one PR | Required |
| `finalize` | Enqueues egress | Outbox `dedup_key` already dedups ([ADR-0059](0059-reconciler-owns-all-github-egress.md)) | Safe as-is |

The dedup keys are the one hard prerequisite (ADR-0082's G4), and they are required by *any* durable
execution, engine or home-grown.

### Works identically across hosts

`CheckpointRuntime` is Postgres-backed, not host-bound, so replay is the same under `run-once`
(resume-on-requeue) and `serve` (resume-on-another-replica). Replay is therefore **decoupled from
the Job-vs-worker decision** ([ADR-0085](0085-agent-execution-plane.md)) — it ships on the current
Job model now and survives whichever host endgame is chosen.

### Consequences

- **Good:** resume-at-the-step inside the isolated execution unit — the requirement, met — with no
  shared host, no Restate operational surface, and the trust boundary intact (journal via the
  internal API).
- **Good:** self-purging by construction (success-delete + TTL sweep), so the store stays small and
  no run's state lingers.
- **Good:** decoupled from the host decision and from Restate's roadmap; ships on today's Jobs.
- **Bad:** it *is* durability we own — a `durable_step` table, a `CheckpointRuntime`, a sweep, and
  the dedup keys. Justified as **one** localized runtime behind an existing seam, not the per-feature
  re-derivation RFC-0005 targets; but it is a maintenance surface, and the step-name/result contract
  is now a compatibility obligation on the loop's hottest file.
- **Bad:** the `llm_turn` at-least-once window can re-pay one turn's tokens on a crash-in-the-ack
  window (bounded, visible via the eaig billing ledger).
- **Neutral:** if resume ever proves not worth even this, Option C (no mid-loop durability) is the
  honest floor — Restate keeps egress + lifecycle and the loop requeues from zero.

## Pros and Cons of the Options

### Option A — `RestateRuntime` in the loop

- Good: engine-native replay; no hand-rolled journal; the RFC-0005 doctrine, honored.
- Bad: **no implementation survives the checkout-isolation requirement** (RFC-0007) — every host
  shape is rejected on mechanism. Right tool, wrong place.

### Option B — `CheckpointRuntime` (chosen)

- Good: lives inside the isolated unit; host-agnostic; trust-boundary-preserving; self-purging.
- Bad: durability we maintain; the at-least-once token window; a journal-contract obligation.

### Option C — no mid-loop durability

- Good: zero new machinery; honors the doctrine most purely.
- Bad: a mid-run crash re-pays the whole loop; loses the resume that motivated the work. Retained as
  the explicit floor if Option B's cost outweighs the crash frequency in practice.

## Risk register

| # | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| C1 | Journal-contract drift: a loop change reorders/ renames steps and breaks in-flight resume | High | Medium | Step names are stability-tested constants; "no step-sequence edits without a `run_epoch` bump" review rule; resume tolerates a missing-suffix gap by continuing live |
| C2 | At-least-once double-effect on a side-effectful step | Medium | Medium–High | Dedup keys on `add_comment`/`propose_pr`; LWW on `add_review_comment`; outbox dedup on `finalize` |
| C3 | Hand-rolled replay has a correctness bug the engine would not | Medium | High | Small surface (one runtime + one table); golden-transcript replay tests (kill at turns 10/75/149 → zero duplicate gateway calls); parity with `Passthrough` on the no-crash path |
| C4 | Store growth / stale rows if purge or sweep fails | Low | Medium | Success-delete + TTL sweep + a unique key that caps rows per run; the sweep is idempotent and logged |
| C5 | Two durability models (Restate egress + home-grown loop) confuse operators | Low | Low | Clear boundary: Restate = *orchestration/egress* durability; `CheckpointRuntime` = *loop* execution-state; documented in RFC-0007 |

## More information

- Parent architecture: [RFC-0007](../rfc/0007-control-plane-v2-planes.md); the seam and R1
  extraction: [ADR-0082](0082-restate-durable-agent-runtime.md) (stands); the doctrine reconciled:
  [RFC-0005](../rfc/0005-durable-orchestration-on-restate.md).
- Consumed by every agent mode via the `StepRuntime` seam; the `replay` role owns the store; `open`
  ([ADR-0088](0088-open-mode-autonomous-ticket-agent.md)) relies on `propose_pr` dedup.
