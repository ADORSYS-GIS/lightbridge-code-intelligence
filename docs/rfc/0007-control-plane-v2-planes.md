# RFC-0007: Control-plane v2 — three planes and the agent execution plane

- **Status:** Proposed
- **Author(s):** Stephane Segning (@stephane-segning)
- **Date:** 2026-07-12
- **Resulting ADRs:** [ADR-0085](../adr/0085-agent-execution-plane.md) (the agent-plane),
  [ADR-0086](../adr/0086-in-house-code-graph-crate.md) (in-house code-graph crate),
  [ADR-0087](../adr/0087-durable-replay-checkpoint-runtime.md) (durable replay via `CheckpointRuntime`),
  [ADR-0088](../adr/0088-open-mode-autonomous-ticket-agent.md) (the `open` autonomous ticket agent)

## Summary

Restructure the system into **two binaries with a hard trust boundary**, replacing the accreted
role sprawl of one `control-plane` binary that now selects among six roles plus two ever-growing
Job images. The **control-plane** binary owns coordination as **three planes** — *ingress*
(`serve`/`a2a`/`mcp`), *orchestration* (`dispatcher`/`replay`), *egress* (`reconciler`/`notifier`) —
holds the database and forge credentials, and **never touches a repository checkout**. A new
**agent-plane** binary owns everything that *does* touch a checkout — indexing, review, and a new
autonomous `open` mode — selected by **mode** (`{index, review, open}`) and deployed by **host**
(`{run-once, serve}`), so the Job-vs-worker question becomes a per-mode *deployment knob*, not an
architectural commitment. Two enabling changes make it lean and durable: an **in-house Rust
code-graph crate** retires the Python Graphify dependency (and the 4 Gi index Job with it), and a
home-grown **`CheckpointRuntime`** gives the agent loop the replay-resume it actually needed —
without contorting Restate, which stays exactly where it earns its keep: egress.

## Motivation

The pain is concrete and it is felt, not aesthetic.

**Role sprawl.** `services/control-plane/src/main.rs` now selects among **six** roles — `serve`,
`dispatcher`, `reconciler`, `restate-worker`, `a2a`, `notifier` — each a Deployment, and
[ADR-0082](../adr/0082-restate-durable-agent-runtime.md) proposed a seventh (`agent-worker`).
Separately, agent execution is scattered across **two** Job images (`agent-runner` for indexing,
the slim `agent-review` from #207). Each addition was locally reasonable; the aggregate is a system
no single person can hold in their head, where "where does X happen?" has no short answer.

**Resource waste with the wrong shape.** A review is IO-bound reads plus LLM orchestration — it
should cost tens of megabytes — yet Job specs carry indexing-sized limits (4 Gi). The 4 Gi is not
the review; it is not even the Job model. It is **Graphify**, the Python graph extractor that pulls
a Python runtime into the indexer image and drives its footprint (the review image was already
split slim to escape it, #207). The dominant *legitimate* per-task cost is the checkout itself, and
that cost is identical under any execution model.

**No mid-loop durability, and the intended fix does not fit.** A deep review can run up to two hours
over 150 turns; a pod death at turn 140 today loses everything and re-pays from turn 0. The stated
end goal of adopting Restate ([RFC-0005](0005-durable-orchestration-on-restate.md),
[ADR-0082](../adr/0082-restate-durable-agent-runtime.md)) was to make that loop *resume at the step
it left*. But every way to run the loop **inside** Restate collides with a hard requirement — a
per-task **isolated, growable checkout** (today `read_file`; tomorrow `grep`/`find_files`; for
`open`, execution): a shared long-lived worker means co-resident checkouts and noisy neighbours; an
orchestrator/executor split is a distributed monolith; serving file reads from a git API dies on
`grep`/`find_files` and on rate limits; a per-task Restate endpoint fights the engine's immutable
deployment model. When every implementation of an idea is rejected by legitimate constraints, the
idea is the wrong tool. **Restate is a poor fit for the loop and a good fit for egress** (structural
single-writer per `platform:installation`, at-least-once delivery — value independent of any UI).

**The trigger.** The concrete ask was simply "add replay on top of what we built." Discovering that
replay does not sit cleanly on the current architecture *is* the finding: the architecture is not
maintainable as it stands. This RFC redraws it so replay — and the next capability, and the one
after — has an obvious home.

## Guide-level explanation

Think of the system as **two programs**, not seven roles.

### The control-plane binary — coordination, three planes

One binary, role-selected (the [RFC-0001](0001-horizontally-scalable-control-plane.md) pattern,
kept), grouping its roles into three planes you can name:

- **Ingress** — the front doors: `serve` (GitHub webhooks + REST API + admin), `a2a` (the agent
  protocol surface, [RFC-0006](0006-a2a-agent-surface.md)), and an `mcp` surface. One job:
  *authenticate a trigger, turn it into a task.*
- **Orchestration** — the core: `dispatcher` (claim a task, launch the agent-plane, reap) and
  `replay` (owns the durable execution-state store; see ADR-0087).
- **Egress** — the back doors: `reconciler` (all GitHub writes, single-writer per installation) and
  `notifier` (A2A push, SSRF-isolated). One job: *deliver a result to an external party,
  at-least-once, isolated.*

The control-plane holds the **database and the forge credentials**. It **never clones a repository**
and never runs a checkout.

### The agent-plane binary — execution, mode × host

A second binary that owns **everything that touches a checkout**. It is selected on two independent
axes:

- **Mode** (*what work*): `index` (build the code graph + embeddings), `review` (the read-only
  review loop), `open` (the write-capable autonomous ticket agent, ADR-0088).
- **Host** (*how deployed*): `run-once` (do one task and exit — the dispatcher spawns a Kubernetes
  Job, [ADR-0004](../adr/0004-one-k8s-job-per-task.md)) or `serve` (a long-lived Deployment + HPA
  that accepts many tasks over time).

The agent-plane holds the **checkout, the LLM gateway key, and the runner token** — and **no
database access and no forge credentials**. It reports findings, transcript, and durable steps only
through the **mediated internal API** ([ADR-0037](../adr/0037-agent-acts-via-mediated-tools.md)),
preserving the [ADR-0002](../adr/0002-rust-control-plane-trust-boundary.md)/[ADR-0017](../adr/0017-agent-runner-control-plane-bootstrap.md)
trust boundary structurally.

The headline: **topology is a deployment knob, not an architecture decision.** The same
mode-loop runs under `run-once` or `serve`; you can start every mode as isolated Jobs (today's
model, safe) and later flip `review` to a `serve` Deployment for centralized observability — without
rewriting the plane, and reversibly. The loop is already host-agnostic through the `StepRuntime`
seam (the R1 extraction, [ADR-0082](../adr/0082-restate-durable-agent-runtime.md) §The code
revamp), so this is a host entrypoint choice, not new machinery.

### The two enabling changes

- **Kill Graphify → an in-house Rust code-graph crate** ([ADR-0086](../adr/0086-in-house-code-graph-crate.md)).
  Tree-sitter directly (already used for chunking), a structurally-resolved call/reference graph,
  embeddings for semantic search, PDF text extraction, and a configurable ignore-list. No Python →
  the index Job right-sizes, and the `review`/`index` images collapse into **one lean binary**,
  which is what makes the single agent-plane binary viable.
- **Replay without Restate** ([ADR-0087](../adr/0087-durable-replay-checkpoint-runtime.md)). A third
  `StepRuntime` implementation, `CheckpointRuntime`, persists each completed step's *result* to a
  `durable_step` store (owned by the `replay` role, written via the mediated internal API). A pod
  death → the loop replays completed steps from storage and continues live at the first gap. Success
  purges the state; a configurable TTL sweeps the rest. This is the resume you asked for, in the
  isolated Job model, with no engine to operate.

### What Restate keeps doing

Restate stays the durable substrate for **egress** (Phase A, live —
[ADR-0074](../adr/0074-restate-egress-pilot.md)) and remains the intended home for the **task
lifecycle** ([ADR-0076](../adr/0076-restate-task-lifecycle-workflow.md)) and A2A durable promises
([RFC-0006](0006-a2a-agent-surface.md)). It is **not** extended into the agent loop. ADR-0082's
`RestateRuntime`-in-the-loop direction is superseded by ADR-0087; ADR-0082's R1 extraction (the
seam, the tool registry, the unit-testable policies) stands entirely and is what makes this cheap.

## Reference-level explanation

### Target topology

| Plane | Role(s) | Holds | Never |
|---|---|---|---|
| Ingress | `serve`, `a2a`, `mcp` | request auth context | forge creds needed only to *deliver* |
| Orchestration | `dispatcher`, `replay` | DB, the `durable_step` store | a checkout |
| Egress | `reconciler`, `notifier` | forge creds, push token | a checkout |
| **Agent** | `{index, review, open} × {run-once, serve}` | checkout, LLM key, runner token | DB, forge creds |

Two binaries: `control-plane` (ingress+orchestration+egress, role-selected) and `agent-plane`
(mode×host). The `restate-worker` serving role remains **only** as long as Restate backs egress/task
lifecycle; it is *plumbing for egress*, not a plane of its own.

### The mode × host matrix (and the routing rules)

|  | `run-once` (Job) | `serve` (Deployment + HPA) |
|---|---|---|
| `index` | **default** — bursty, restartable, elastic to zero | allowed, rarely worth it |
| `review` | **default today** — isolated, k8s-native lifecycle | allowed — for centralized observability + a live status API, once footprint is measured |
| `open` | **required** — executes untrusted + generated code; needs a real pod sandbox | **forbidden** — namespaces isolate files, not execution |

Routing rules that are *structural*, not conventions:

- **`open` is `run-once` only.** It executes code; a shared tenant cannot sandbox execution.
- **Any execution-needing task** (a future review that runs tests, SAST —
  [ADR-0061](../adr/0061-sast-deterministic-finding-source.md)) maps to `run-once` for the same
  reason. SAST stays its own sandboxed step regardless.
- **`serve` review** is deferred to a measurement (see Migration), because post-Graphify the
  resource case for it is thin and its real advantage is observability.

### Trust boundary (unchanged in spirit, now structural)

The agent-plane has **no DB and no forge credentials** in any mode — including `open`, which
*produces code*. It writes to a **local branch in its sandbox** and hands the branch/patch to the
egress plane through the mediated internal API; `reconciler` (which holds the forge creds) pushes
and opens the PR. This extends [ADR-0037](../adr/0037-agent-acts-via-mediated-tools.md) from
comments to code changes and keeps the highest-risk, code-executing pod credential-free. Durable
steps are likewise journaled *through* the internal API to the `replay` role, so the `durable_step`
store lives in the orchestration plane, never on the agent pod.

### Durable execution-state (`durable_step`)

Postgres, execution-state only (the [RFC-0005](0005-durable-orchestration-on-restate.md) line:
Restate/we own *execution state*, Postgres owns *domain data*). Keyed by
`(task_id, run_epoch, step_name)` → `{ result | offload_ref, content_hash, created_at }`, with the
same offload rule as ADR-0082's journal for over-cap payloads. Owned and swept by the `replay` role.
Full mechanics in [ADR-0087](../adr/0087-durable-replay-checkpoint-runtime.md).

### Invariants preserved (the merge bar for any slice)

- **Single-writer egress** per `platform:installation` ([ADR-0059](../adr/0059-reconciler-owns-all-github-egress.md),
  structural via Restate today) — untouched.
- **SSRF isolation** of A2A push egress ([ADR-0079](../adr/0079-a2a-push-notifications-webhook-egress.md)) — untouched.
- **Per-task isolation** of execution — preserved by `run-once` for `open`/execution and by
  single-tenant scheduling for any `serve` mode.
- **At-least-once + idempotency** — every side-effectful step (findings, `add_comment`, `finalize`,
  and now `propose_pr`) carries a dedup key; replay never double-acts.
- **Trust boundary** — agent-plane credential-light; DB + forge creds stay in the control-plane.

## Drawbacks

- **It is a re-architecture on live infrastructure by a one-person team — the highest-risk move
  available.** The only safe execution is the strangler this RFC prescribes; a big-bang rewrite
  would reintroduce the lease/idempotency/replay correctness the current system already paid for.
  The prod-hardened machinery is an asset, retired one seam at a time, never deleted ahead of its
  replacement.
- **`CheckpointRuntime` is durability we own.** [RFC-0005](0005-durable-orchestration-on-restate.md)'s
  doctrine is *stop hand-rolling durability*; this hand-rolls it for the loop. The justification is
  bounded: it is **one** localized runtime behind an existing seam plus one table and a sweep — not
  the sprawling per-feature re-derivation RFC-0005 targets — and it buys resume the engine could not
  give the isolated loop. Restate still owns egress and the task lifecycle, so we do not fragment
  the *orchestration* durability model.
- **Re-owning what k8s Jobs give free.** A `serve` host must re-implement concurrency bounding,
  stale-process reclaim, and — for anything executing — sandboxing that a per-task pod provides for
  nothing. This is why `serve` is deferred and gated on a measurement, not adopted up front.
- **Graph-extraction parity risk.** An in-house extractor must match Graphify's symbol/edge
  accuracy; mitigated by golden + parity tests and a language-by-language migration (ADR-0086).
- **`open` is a genuinely new, high-blast-radius capability** — write access plus code execution.
  Its safety rests on sandbox isolation, mediated push, and a mandatory human PR gate (ADR-0088);
  it is not a small feature and is scoped as its own slice.

## Alternatives

- **Do nothing / keep accreting roles.** The status quo: add `agent-worker` as a seventh role, keep
  two Job images, keep the 4 Gi ghost, keep no replay. Rejected — it is the maintainability problem
  that triggered this RFC.
- **Restate in the loop (ADR-0082 Option A).** Rejected here on the checkout-isolation collision
  above; superseded by ADR-0087 for the loop while retained for egress.
- **Shared multi-tenant `agent-worker` for reviews.** Rejected: co-resident checkouts and noisy
  neighbours; namespace isolation addresses files but not the execution case, and the resource win
  is largely illusory once Graphify is gone (the checkout dominates, and it co-locates rather than
  shrinks).
- **A cosmetic role-rename into "planes" without the binary split.** Rejected: the ask is
  architectural clarity *and* resource reduction; a rename delivers neither. The two-binary trust
  split and the Graphify/replay changes are the substance.

## Unresolved questions

- **`serve`-review adoption is explicitly a measurement, not a decision here.** After Graphify is
  gone and review is right-sized, measure the real per-review footprint and the observability gain
  before moving `review` off `run-once`. Deferred to a follow-up ADR with data.
- **PDF-parser dependency choice** for the graph crate (untrusted-input safety) — ADR-0086
  Unresolved questions.
- **`open` large-diff transport** through the internal API (offload sizing) and the exact
  `propose_pr` egress contract — ADR-0088.
- **Does the `mcp` surface warrant its own ingress role or fold into `serve`?** Scaling-profile
  question; resolved during implementation.
- **Sequencing of the Restate task-lifecycle (Phase B, ADR-0076)** relative to these slices — it is
  orthogonal (orchestration durability) and can proceed in parallel; this RFC neither blocks nor
  requires it.

### Migration (strangler slices, in order)

1. **Graph crate** ([ADR-0086](../adr/0086-in-house-code-graph-crate.md)) — retire Graphify;
   index Job right-sizes; images converge. Independently valuable.
2. **Agent-plane consolidation** ([ADR-0085](../adr/0085-agent-execution-plane.md)) — one binary,
   `mode × host`, `run-once` host first (behavior-identical to today's Jobs).
3. **Replay** ([ADR-0087](../adr/0087-durable-replay-checkpoint-runtime.md)) — `CheckpointRuntime` +
   `durable_step` + the `replay` role; resume-on-requeue on the existing Job model.
4. **`open` mode** ([ADR-0088](../adr/0088-open-mode-autonomous-ticket-agent.md)) — the sandbox, the
   write toolset, mediated PR egress.
5. **`serve` host + live status API** — *if* the measurement justifies it for `review`.

Each slice ships behind the existing seams, preserves the invariants above as its merge bar, and is
independently valuable — no slice depends on the last one landing.
