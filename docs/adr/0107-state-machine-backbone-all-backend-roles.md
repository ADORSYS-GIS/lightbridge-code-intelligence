# ADR-0107: The state-machine backbone extends across all backend roles, not just the review loop

- **Status:** Accepted
- **Date:** 2026-07-26
- **Deciders:** @stephane-segning
- **Extends:** [ADR-0087](0087-durable-replay-checkpoint-runtime.md), RFC-0007

## Context and Problem Statement

[ADR-0087](0087-durable-replay-checkpoint-runtime.md) gave the **review/open agent loop** a durable
`StepRuntime` seam (`Passthrough` today, `CheckpointRuntime` behind the `durable_step` SQL table,
gated on #363). That trait and its SQL backbone are proven — landed via PR #362 — but scoped to the
agent loop only. Webhook ingress, the dispatcher, the reconciler, and the A2A role each have their
own hand-rolled state handling (status columns, ad-hoc retry counters, the `github_outbox`
status/attempts machinery) that is not expressed through the same trait, so a future durable
substrate (Restate or otherwise, per [ADR-0093](0093-restate-egress-pilot-no-go.md)'s "reopen only
if a concrete need appears") would have to be adopted per-role from scratch rather than by
implementing one seam once. This ADR does not reopen Restate — [ADR-0093](0093-restate-egress-pilot-no-go.md)'s
no-go stands — it generalizes the trait boundary so that decision, if ever revisited, has one seam
to target instead of four.

## Decision Drivers

- One `StepRuntime`-shaped trait boundary for every backend role's state transitions, so a future
  durable substrate is a `StepRuntime` implementation swap, not a per-role rewrite.
- SQL (`durable_step`, plus role-specific tables like `github_outbox`, `tasks`) stays the backbone —
  this ADR does not introduce a new datastore, it generalizes how roles talk to the one that exists.
- Multi-layer error handling: `thiserror` for typed, per-crate error enums at each boundary
  (`agent-step`, `control-plane::db`, `control-plane::integrations`, etc.), `anyhow` only at the
  outermost role-entrypoint (`main.rs` role dispatch) where errors are logged/reported and don't need
  to be matched on. This formalizes the pattern [ADR-0083](0083-platform-crate-architecture-and-cratestack-data-layer.md)
  gestured at but never pinned down.

## Considered Options

- **A — Leave role-specific ad-hoc state handling as-is; only the agent loop gets `StepRuntime`.**
  Rejected: this is the status quo this ADR changes; it leaves three roles (ingress, reconciler, A2A)
  without a common seam.
- **B — Generalize `StepRuntime` (or a sibling trait with the same shape) to webhook ingress, the
  dispatcher, the reconciler, and A2A's task lifecycle, each backed by the existing SQL tables
  through `DurableStepStore`-style implementations; standardize `thiserror`/`anyhow` layering
  across every crate that currently mixes ad-hoc `Result<T, String>` or one-off error types.**
  Chosen.
- **C — Re-open Restate/RFC-0005 Phase B now, as the shared substrate.** Rejected outright by
  [ADR-0093](0093-restate-egress-pilot-no-go.md); no new evidence has appeared to revisit that call.
  This ADR deliberately keeps the door open for a *future* re-evaluation by making the seam uniform,
  without re-evaluating Restate itself now.

## Decision Outcome

Chosen option: **B**. Extend the `StepRuntime` trait family (`services/agent-step`) so webhook
ingress, dispatcher, reconciler, and A2A task-lifecycle transitions are each expressed as steps
through the same seam, backed by SQL (`durable_step` where a generic step fits, existing
role-specific tables where a role already has one, e.g. `github_outbox`). `Passthrough` remains the
production default until each role's migration is individually verified — this is an additive,
role-by-role rollout of one already-accepted mechanism, not a new one. Error handling across
`control-plane` and `agent-*` crates standardizes on `thiserror` per-crate error enums with
`#[from]` conversions at internal boundaries, and `anyhow::Result` only at role `main.rs` entry
points.

### Consequences

- Good, because every backend role's state transitions are now inspectable/replayable through one
  trait shape, which is the actual prerequisite for ever safely re-evaluating a durable substrate
  like Restate — the generalization this ADR makes is what would let that future decision be made
  cheaply, without contradicting [ADR-0093](0093-restate-egress-pilot-no-go.md) today.
- Good, because a consistent `thiserror`/`anyhow` layering makes error provenance traceable across
  role boundaries instead of collapsing to strings at the first `?`.
- Bad, because this touches every role's control-flow code — it is a multi-story migration
  (tracked as new stories under Epic #353), not a single PR, and each role's cutover needs its own
  verification pass per this repo's refactor-discipline rule (behavior-neutral, full test suite
  unchanged).
- Neutral, because promoting `CheckpointRuntime` itself to the production default stays blocked on
  #363's two open P1s regardless of this ADR's broader scope — this ADR generalizes the seam, it
  does not by itself flip any role's default runtime.

## More Information

This is scoped as new stories under the existing Epic #353 (RFC-0007, ADR-0085/86/87/88), not a
competing epic — #353 already delivered the trait and the SQL store this ADR generalizes.
