# ADR-0093: Restate egress pilot — no-go; revert to the reconciler drain

- **Status:** Accepted
- **Date:** 2026-07-16
- **Deciders:** @stephane-segning
- **Supersedes:** [ADR-0074](0074-restate-egress-pilot.md)

## Context and Problem Statement

[ADR-0074](0074-restate-egress-pilot.md) (RFC-0005 Phase A) adopted Restate as the substrate for
**platform egress**: forge posts (review/reply/reaction/etc.) are delivered by a `PlatformEgress`
virtual object keyed `{platform}:{installation}`, whose engine-guaranteed per-key serialization
makes the single-writer-per-key invariant *structural* rather than enforced only by a code comment
plus a Helm `replicas: 1`. It shipped as a **pilot** with an explicit exit gate: after ≥ 3 weeks in
prod, either graduate to RFC-0005 Phase B or "send it back to dev-only evaluation or remove it, and
**record the outcome in a superseding ADR**." This is that superseding ADR.

## Decision

**No-go.** Revert forge egress to the reconciler outbox **drain** ([ADR-0059](0059-reconciler-owns-all-github-egress.md))
as the sole path, disable the Restate infrastructure (the `PlatformEgress` worker + the single-node
`restate` server), and remove the Restate code from the control-plane. Re-adopt Restate only if forge
egress genuinely needs to scale past a single writer.

## Rationale — the soak evidence

The pilot soaked ~6 days in prod (2026-07-09 → 2026-07-15) and was **operationally flawless**:

| metric | value |
|---|---|
| egress ops via `PlatformEgress` | 622, all `posted` |
| dead-letters | 0 |
| outbox rows with errors | 0 |
| stuck-pending (> 1h) | 0 |
| restate-worker WARN/ERROR (7d) | 0 |
| `restate-0` restarts | 0 |

It works. It just does not **earn its keep**:

- The entire rationale for ADR-0074 was to let the reconciler scale **past `replicas: 1`** without
  breaking single-writer ordering. The reconciler is `replicas: 1` at ~89 posts/day, and there is no
  foreseeable need to shard it. At one writer the single-writer invariant is **already held for free**,
  and the `outbox` table already provided the audit ledger, dedup keys, and retry that the pilot's
  value case leaned on.
- So Restate is a whole StatefulSet + a control-plane worker role of operational surface (a
  durable-execution server to run, upgrade, and reason about) for **no marginal correctness or
  throughput gain at current scale**.
- The soak did not reach the ADR-0074 three-week window, but the decision does not hinge on more
  clean-soak days — it hinges on the **scaling need**, which has not materialized and is not near.
  Carrying the infra for two more weeks to re-confirm what is already clear is not worth it.

## Decision Drivers

- **Simplicity** — one egress path (drain), no extra durable-execution server in the operational
  footprint. Fewer moving parts a one-person team must hold in their head.
- **Safe, cheap reversal** — ADR-0074 deliberately designed a single-ledger (outbox) rollback, so
  reverting is lossless and the re-adoption path stays open at low cost.

## Consequences

- **Good:** egress returns to the proven pre-pilot ADR-0059 reconciler drain (single-replica,
  single-writer) — unchanged, battle-tested. The operational surface shrinks.
- **Neutral:** the config disable ships via ai-helm-values (`egress.mode: drain`, `restate-worker`
  off, `restate` server `replicaCount: 0`) — reversible; the server is *scaled to 0, not deleted*, so
  its PVC (durable log + registered endpoint) survives for a future re-enable.
- **Bad (tracked):** until the code-removal ticket (#433) lands, the Restate code path stays dormant in the
  image behind `egress.mode: drain` (the code default). This violates the "no dormant code" posture
  and is why the removal is tracked, not deferred indefinitely.
- **RFC-0005 Phase B** (task lifecycle on Restate) is **not** pursued on this evidence. Reopen only if
  a concrete scaling or durability need appears that the home-grown mechanisms — `CheckpointRuntime`
  ([ADR-0087](0087-durable-replay-checkpoint-runtime.md)) for replay, the outbox
  ([ADR-0059](0059-reconciler-owns-all-github-egress.md)) for egress — do not already cover.

## Alternatives Considered

- **Finish the full 3-week soak, then decide.** Rejected: the decision hinges on the scaling horizon,
  not on more clean-soak days. The record is already flawless; two more weeks re-confirms nothing that
  changes the call, at the cost of carrying the infra.
- **Keep it running "in case".** Rejected: that pays a standing operational cost as insurance against
  a problem not on the horizon. The single-ledger rollback makes re-adoption cheap *if* the need ever
  appears — so there is no reason to hold the infra live now.

Supersedes [ADR-0074](0074-restate-egress-pilot.md) (status → Superseded). Egress delivery returns
to [ADR-0059](0059-reconciler-owns-all-github-egress.md).
