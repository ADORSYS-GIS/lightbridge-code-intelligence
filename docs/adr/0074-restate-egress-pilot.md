# ADR-0074: Restate pilot — platform egress becomes a virtual object (RFC-0005 Phase A)

- **Status:** Superseded by [ADR-0093](0093-restate-egress-pilot-no-go.md) (pilot no-go — reverted to the reconciler drain)
- **Date:** 2026-07-09
- **Deciders:** @stephane-segning

## Context and Problem Statement

[RFC-0005](../rfc/0005-durable-orchestration-on-restate.md) proposes adopting Restate as the
durable-execution substrate via a strangler migration. This ADR records the first concrete
decision: **which seam moves first, and under what gates and rollback terms.**

The candidate seams were the outbox/egress drain, the task-lifecycle (dispatcher + reaper), and
the unbuilt RFC-0001 scheduler. Egress is the strongest pilot for three reasons: it maps 1:1 to
Restate's most distinctive primitive (the virtual object's per-key serialized execution), it
deletes the system's only documentation-enforced invariant (the single-replica reconciler,
[ADR-0059](0059-reconciler-owns-all-github-egress.md) /
[`reconciler.rs:1-16`](../../services/control-plane/src/queue/reconciler.rs)), and its blast
radius is bounded — a failure delays or dead-letters a PR comment; it can never lose a review
run, because producers keep writing `outbox` intent rows first
([ADR-0056](0056-control-plane-owns-the-posted-output.md)).

## Decision

**Run the RFC-0005 Phase A pilot: a `PlatformEgress` virtual object, keyed
`{platform}:{installation_or_project_id}`, replaces the reconciler's outbox drain.** Gated,
reversible, and scoped as follows.

Mechanics (details and sequence diagram in RFC-0005; this ADR fixes the decisions):

- **Producers are unchanged.** `enqueue_outbox_post` keeps writing the fully-shaped intent row
  (`dedup_key` idempotency, same-transaction-as-domain-write where applicable); it additionally
  `send`s `PlatformEgress::post(outbox_id)` to Restate with idempotency key = `dedup_key`. The
  journal carries only the row id — payloads are re-read inside `ctx.run`.
- **The `outbox` table stays**, as the audit record, the dead-letter destination
  (`TerminalError` branch marks the row `failed`, as `mark_outbox_failed` does today), and the
  shared ledger that makes rollback safe.
- **The reconciler role survives the pilot**: it keeps the inbound feedback poll
  ([ADR-0035](0035-review-feedback-signal.md)) and its outbound drain code is retained but
  disabled by config — not deleted — for the entire pilot window.
- **Key choice** `{platform}:{installation}` (not per-repo): matches the granularity at which
  GitHub/GitLab rate limits and abuse detection operate, so serialization also serves as
  rate-limit alignment. Per-repo would allow more parallelism we do not currently need.
- **New infrastructure:** single-node Restate server (Helm chart, StatefulSet + RocksDB PVC) in
  `converse` via ai-helm/ArgoCD; a `restate-worker` Deployment running the new control-plane
  role (SDK hyper endpoint on `:9080`, metrics sidecar-listener as the other non-serve roles).
- **Determinism rules** from RFC-0005 apply from day one (all I/O in `ctx.run`, small journal
  payloads, no Context fan-out while [sdk-rust #89](https://github.com/restatedev/sdk-rust/issues/89)
  is open, exact version pins).

### Gates

1. **Entry gate (Phase 0 spike, before any pilot code):** single-node Restate in a dev
   namespace; a toy workflow proves `ctx.run`+sqlx, an awakeable, `ctx.sleep`, redeploy
   mid-invocation (drain behavior), and server 1.7 ↔ sdk-rust 0.10 compatibility (the published
   matrix stops at 1.6). Any failure = the RFC goes back to Proposed with findings.
2. **Exit gate (go/no-go for RFC-0005 Phase B), after ≥ 3 weeks in prod:** zero lost/duplicate
   posts (audited against `outbox` rows), dead-letter behavior exercised at least once
   (deliberately, against a deleted PR), one SDK upgrade absorbed, and an honest write-up of
   operational surprises. Failing the gate = flip the config flag back, keep the engine for
   dev-only evaluation or remove it, and record the outcome in a superseding ADR.

## Consequences

- **Good:** the replicas=1 invariant becomes structural — the engine guarantees one running
  handler per key regardless of pod count, which also unblocks scaling the surviving reconciler
  (poll-only) without ceremony.
- **Good:** retry/backoff/dead-letter logic (`mark_outbox_failed`, `attempts²` schedule, sweeper
  interplay) is replaced by the engine's retry policy + one explicit terminal branch — less
  hand-rolled correctness to maintain, which is RFC-0005's core motivation.
- **Good:** the pilot is independently valuable. Even if Phase B never happens, the egress path
  is better than before — this is deliberate strangler hygiene (RFC-0005 risk R10).
- **Bad:** one more stateful pod (RocksDB PVC) and a new operational mental model (journals,
  replay, immutable deployment versions) enter the GitOps estate for what is, today, a working
  drain loop. The pilot exists to price exactly this.
- **Bad:** during the pilot, egress correctness depends on *two* systems (Postgres ledger +
  Restate journal) agreeing; the mitigations are the status-guarded idempotent writes the drain
  already uses (RFC-0005 risk R7) and the single-ledger rollback design.
- **Neutral:** observability moves — egress health is read from the engine's introspection +
  the unchanged `outbox` rows; the Grafana egress panels keep working because they read
  Postgres ([ADR-0046](0046-observability-dashboard-deployment.md)).

## Alternatives considered

- **Pilot on the task lifecycle instead (RFC-0005 Phase B first).** Bigger payoff (deletes the
  dispatcher/reaper), but it puts the correctness-critical path and the journal-evolution
  problem (2 h deep reviews in flight across deploys) into the *learning* phase. Rejected:
  learn on the bounded seam.
- **Pilot the unbuilt scheduler.** Greenfield (no migration), but it exercises only timers —
  none of the primitives (virtual objects, awakeables) the rest of the migration depends on,
  and it delivers no independent value if we stop there. Rejected.
- **Shadow mode (Restate posts to a log, reconciler still posts for real).** Safer-looking, but
  egress is write-only against external APIs — a shadow can't validate the interesting half
  (idempotency under real 403/404/rate-limit responses), and dual-posting risk is exactly what
  the single-ledger design avoids. Rejected in favour of the config-flag rollback.
- **No engine; make the reconciler invariant structural ourselves** (e.g. Postgres advisory
  lock leader election). Fixes the invariant, builds nothing toward RFC-0006/A2A, and adds yet
  another hand-rolled coordination mechanism — the pattern RFC-0005 argues against. Rejected.

## References

- [Runbook — activating the Restate egress pilot](../runbooks/restate-egress-pilot-activation.md) —
  the concrete operator steps to turn this pilot on/off (credentials → register endpoint → flip
  `egress.mode`), the verification checklist, and the exit-gate criteria.
- [RFC-0005](../rfc/0005-durable-orchestration-on-restate.md) — the proposal this ADR implements
  Phase A of; risk register and determinism rules live there.
- [ADR-0059](0059-reconciler-owns-all-github-egress.md) — the single-writer egress decision
  whose *mechanism* this replaces (per-key serialization keeps its semantics; the transactional
  outbox intent-row design of ADR-0056/0059 is retained).
- [ADR-0058](0058-rename-poller-role-to-reconciler.md) — the reconciler role, which survives
  with its inbound half.
- [ADR-0029](0029-focused-review-not-generic-runner.md) — the scope boundary this pilot does
  not reopen (no operator-extensible execution; internal substrate only).
- [ADR-0004](0004-one-k8s-job-per-task.md) / [ADR-0017](0017-agent-runner-control-plane-bootstrap.md)
  — unchanged execution and trust-boundary model.
