# ADR-0085: The agent execution plane — one binary, mode × host

- **Status:** Proposed
- **Date:** 2026-07-12
- **Deciders:** @stephane-segning

## Context and Problem Statement

Agent execution — the code that clones a repo and does work over the checkout — is today scattered
across two Job images (`agent-runner` for indexing, the slim `agent-review` from #207) and was about
to gain a third home ([ADR-0082](0082-restate-durable-agent-runtime.md)'s proposed long-lived
`agent-worker` role). Each is a different binary or role with an overlapping-but-forked substrate
(checkout, workspace, tools, the loop). Meanwhile the `control-plane` binary already carries six
coordination roles. How do we give checkout-bearing work **one coherent surface** with a clean trust
boundary, right-sized resources, and the freedom to run it as an isolated Job *or* a shared
Deployment — without deciding that topology at the architecture level?

This ADR is the load-bearing decision of [RFC-0007](../rfc/0007-control-plane-v2-planes.md).

## Decision Drivers

- **Architectural clarity, not cosmetics.** "Where does execution happen?" must have a one-line
  answer.
- **A structural trust boundary.** The agent surface holds no forge and no DB credentials
  ([ADR-0002](0002-rust-control-plane-trust-boundary.md)/[ADR-0017](0017-agent-runner-control-plane-bootstrap.md)/[ADR-0037](0037-agent-acts-via-mediated-tools.md));
  that must survive as *structure*, not convention.
- **Resource reduction.** A review is IO-bound reads; it must not carry indexing-sized limits.
- **Topology freedom.** Isolated per-task Jobs ([ADR-0004](0004-one-k8s-job-per-task.md)) and a
  shared Deployment are both legitimate for different modes and different moments; the choice should
  be reversible config, not a rewrite.
- **Reuse the R1 seam.** [ADR-0082](0082-restate-durable-agent-runtime.md)'s extraction already made
  the loop generic over `StepRuntime` and host-agnostic; build on it, don't refork.

## Considered Options

- **Option A — one `agent-plane` binary selected by `mode × host`** (this ADR).
- **Option B — keep separate binaries/images per kind** (`agent-runner`, `agent-review`,
  `agent-worker`), the status quo extended.
- **Option C — fold agent execution into the `control-plane` binary as more roles.**

## Decision Outcome

Chosen option: **Option A — a single `agent-plane` binary**, distinct from `control-plane`, selected
on two independent axes:

- **Mode** — *what work*: `index`, `review`, `open` ([ADR-0088](0088-open-mode-autonomous-ticket-agent.md)).
- **Host** — *how deployed*: `run-once` (do one task, exit — a dispatcher-spawned Job) or `serve`
  (a long-lived Deployment + HPA accepting many tasks).

This is the [RFC-0001](../rfc/0001-horizontally-scalable-control-plane.md) one-binary-role-selected
pattern applied to execution, with the host axis added so **topology is a deployment knob**.

The two binaries and their credentials:

| Binary | Owns | Holds | Never |
|---|---|---|---|
| `control-plane` | ingress + orchestration + egress | DB, forge creds | a checkout |
| `agent-plane` | `{index, review, open} × {run-once, serve}` | checkout, LLM key, runner token | DB, forge creds |

**Why the split works now:** [ADR-0086](0086-in-house-code-graph-crate.md) removes Python/Graphify,
so `index` and `review` no longer need different images — the #207 split *dissolves* and one lean
Rust binary serves every mode. The loop is already host-agnostic through `StepRuntime`
([ADR-0082](0082-restate-durable-agent-runtime.md)), so `run-once` vs `serve` is an entrypoint
choice over the *same* mode-loop, exactly as `control-plane`'s `serve` vs `dispatcher` are today.

### The mode × host matrix and its routing rules

|  | `run-once` (Job) | `serve` (Deployment + HPA) |
|---|---|---|
| `index` | **default** (bursty, restartable, elastic) | allowed |
| `review` | **default today** (isolated, k8s lifecycle for free) | allowed after measurement |
| `open` | **required** | **forbidden** |

Structural rules, not conventions:

- **`open` is `run-once` only.** It executes untrusted repo code and its own generated code; a
  shared `serve` tenant cannot sandbox arbitrary execution (Linux user namespaces isolate *files*,
  not execution). Enforced at dispatch.
- **Any execution-needing task** routes to `run-once` for the same reason. The opengrep SAST pass
  ([ADR-0061](0061-sast-deterministic-finding-source.md)) stays its own sandboxed step regardless.
- **`serve` for `review` is deferred to a measurement** (RFC-0007 Migration): post-Graphify the
  resource case is thin and its real payoff is centralized observability + a live per-review status
  API. Decide with the measured footprint, not up front.

### The trust boundary, made structural

In every mode — including `open`, which *writes code* — the agent-plane holds **no DB and no forge
credentials**. It reports findings and transcript, journals durable steps
([ADR-0087](0087-durable-replay-checkpoint-runtime.md)), and — for `open` — hands a local branch to
egress, all through the **mediated internal API** ([ADR-0037](0037-agent-acts-via-mediated-tools.md)).
The `reconciler` (holding forge creds) performs every external write. The durable-step store lives
in the orchestration `replay` role, never on the agent pod. This is ADR-0002/0017 preserved as
structure: the credential-bearing plane and the checkout-bearing plane are different binaries with
different secrets.

### Consequences

- **Good:** one place for "where execution happens," one substrate (checkout, workspace, the graph
  crate, the loop) reused across modes instead of three forks.
- **Good:** resource right-sizing per mode (lean `index` post-Graphify, tiny `review`) and the
  Job-vs-worker decision demoted to reversible config.
- **Good:** the trust boundary is now a binary boundary — the highest-risk pods (`open`) provably
  hold no forge/DB secret.
- **Bad:** a `serve` host re-owns concurrency bounding, stale-process reclaim, and (for execution)
  sandboxing that k8s Jobs give free — so `serve` is opt-in and gated, not the default.
- **Bad:** bundling modes in one binary means the image carries all modes' code; acceptable only
  because Graphify's removal keeps it lean (a Python dep would have regrown the review image).
- **Neutral:** the `restate-worker` serving role persists only while Restate backs egress/task
  lifecycle; it is egress plumbing, not part of this plane.

## Pros and Cons of the Options

### Option A — one `agent-plane`, mode × host

- Good: one substrate; structural trust boundary; topology as config; reuses the R1 seam.
- Good: enables per-mode resource classes and per-mode default topologies.
- Bad: `serve` re-owns k8s-free lifecycle guarantees; one image carries all modes.

### Option B — separate binaries/images per kind

- Good: minimal change; each image is already scoped.
- Bad: the forked substrate keeps drifting; three homes for "execution"; no shared path to replay or
  a live status API; the sprawl this RFC exists to end.

### Option C — fold execution into `control-plane`

- Good: fewer binaries nominally.
- Bad: **breaks the trust boundary** — puts checkouts (and, for `open`, code execution) in the same
  binary as the DB and forge credentials. Rejected on mechanism.

## Risk register

| # | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| A1 | One binary bundling `open`'s write/exec toolset ships that capability everywhere it runs | Medium | High | Capabilities gated by mode at startup; `open` toolset unreachable in `index`/`review`; write/exec tools registered only for `open` |
| A2 | `serve` host re-owns lifecycle (concurrency, stale reclaim) and does it worse than k8s | Medium | Medium | `serve` deferred + gated on measurement; `run-once` remains the default; a `replay`/reaper-owned sweep for `serve` scratch |
| A3 | Mode/host routing bug lets an execution task land on a shared `serve` tenant | Low | High | Routing rule enforced at dispatch (`open`/execution ⇒ `run-once`), asserted in tests |
| A4 | Migration drift: consolidating two images into one changes review/index behavior | Medium | Medium | `run-once` host first, behavior-identical; golden transcripts (ADR-0082) as the merge bar; images converge only after parity |

## More information

- Parent architecture: [RFC-0007](../rfc/0007-control-plane-v2-planes.md).
- Modes: `index` consumes [ADR-0086](0086-in-house-code-graph-crate.md); `review` is the existing
  loop; `open` is [ADR-0088](0088-open-mode-autonomous-ticket-agent.md).
- Durability across hosts: [ADR-0087](0087-durable-replay-checkpoint-runtime.md).
- Seam and prior extraction: [ADR-0082](0082-restate-durable-agent-runtime.md) (R1 stands; its
  `RestateRuntime`-in-loop direction is superseded by ADR-0087).
