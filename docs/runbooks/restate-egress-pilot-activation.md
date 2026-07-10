# Runbook — activating the Restate egress pilot (RFC-0005 Phase A / ADR-0074)

**Audience:** the operator (repo owner). **Status of the system today:** the Restate server, the
`restate-worker` role, and the `PlatformEgress` virtual object are all **deployed and live but
idle** — egress still flows through the pre-existing reconciler drain because `egress.mode` defaults
to `drain`. This runbook is the checklist to *turn the pilot on* (and off), and — more importantly —
explains **what each step does and why**, because the three actions involved are easy to conflate.

> This is an operational runbook, not a design doc. The decision and its gates live in
> [ADR-0074](../adr/0074-restate-egress-pilot.md); the strangler rationale in
> [RFC-0005](../rfc/0005-durable-orchestration-on-restate.md). Read those for *why*; read this for
> *how*.

---

## The mental model (read this first)

Today, when the review pipeline wants to post something to a forge (a review body, a 👍/👎 reaction,
a failure notice), it writes an **`outbox` intent row** in Postgres and the **reconciler** role
drains that table and does the HTTP POST. That drain is the thing ADR-0059 kept single-replica by
*documentation* ("there must be exactly one").

The pilot replaces the *drain mechanism* (not the outbox table) with a Restate **virtual object**
called `PlatformEgress`, keyed per `platform:installation`. Restate guarantees exactly one handler
runs per key at a time — so the single-writer invariant becomes **structural** instead of a comment.

Turning the pilot on is **three distinct things**, and this is the part that trips people up:

| # | Action | What it changes | Who does the POST afterwards |
|---|--------|-----------------|------------------------------|
| **A** | **Give `restate-worker` the forge credentials** | The `PlatformEgress` handler *runs inside the `restate-worker` pod*. In `restate` mode, that handler is what actually calls the GitHub/GitLab API — so the worker now needs the App key the reconciler used to hold. | the worker |
| **B** | **Register the worker's endpoint with the Restate server** | Tells the Restate **server** *where the `PlatformEgress` handler lives* (which URL to route invocations to). Without this, the server accepts invocations but has nowhere to send them. | (routing only) |
| **C** | **Flip `egress.mode` from `drain` → `restate`** | Tells the **producers** to `send` a `PlatformEgress::post(outbox_id)` invocation to Restate instead of relying on the reconciler drain. This is the actual switch. | the worker's handler |

They are separate because they touch three different components (the worker's secret mount, the
Restate server's deployment registry, the producers' config). **Order matters** — see below.

Two ports you will use, both on the single-node Restate server in namespace `converse`:

- **Ingress `:8080`** — where *producers* send `PlatformEgress::post` invocations (this is
  `egress.restate_ingress_url`).
- **Admin `:9070`** — where *you* register/inspect deployments (step B).

The worker serves its SDK endpoint on **`:9080`** (plain h2c — no TLS, so `ring` stays the sole
rustls provider).

---

## Pre-flight

- [ ] Phase A **entry gate** (ADR-0074 §Gates) already passed in the Phase-0 spike (ctx.run+sqlx,
      awakeable, ctx.sleep, redeploy-mid-invocation, server 1.7 ↔ sdk-rust 0.10 compat). This is a
      one-time thing done before the pilot code merged; you are not repeating it.
- [ ] The Restate server is healthy: `aii-restate` ArgoCD app Synced/Healthy, the StatefulSet pod
      Running, RocksDB PVC bound. (See [prod deployment notes](../kubernetes-deployment.md) for the
      converse-ns / hetzner-prod context.)
- [ ] The `restate-worker` Deployment (`lightbridge-ci-restate-worker`) is Running and its readiness
      probe is green (it serves `:9080` even while idle).
- [ ] You can reach the Restate **admin** API (`:9070`) — e.g. `kubectl port-forward` to the server
      pod, or `restatectl`/`restate` CLI pointed at it.

---

## Activation

Do these **in order**. The order avoids a window where producers emit invocations that either can't
be routed (B not done) or fail to post for lack of credentials (A not done).

### Step A — give `restate-worker` the forge credentials  *(GitOps, in `ai-helm-values`)*

The `PlatformEgress` handler posts to the forge, so the worker pod needs the **same GitHub App
private key / GitLab token** the reconciler already mounts. Today the worker runs **without** them
(they were deliberately kept off the idle pod).

- In `ai-helm-values`, add the forge-credential secret mount / env to the `restate-worker`
  Deployment values (mirror what the `reconciler` role gets).
- Commit → ArgoCD syncs → the worker restarts with the credentials.
- Verify the worker still starts clean and its readiness probe stays green.

> Keep this reversible: leaving the credentials mounted while in `drain` mode is harmless (the
> handler is never invoked), so A can land ahead of B/C without turning anything on.

### Step B — register the worker endpoint with the Restate server  *(admin API, `:9070`)*

```bash
# Point the CLI at the server's admin API (port-forward or in-cluster).
restate deployments register \
  http://lightbridge-ci-restate-worker.converse.svc.cluster.local:9080

# Confirm the PlatformEgress service is now known to the server:
restate deployments list          # the worker endpoint + its revision
restate services list             # should include `PlatformEgress`
```

- Registration is **per deployment revision**. Restate uses immutable deployment versioning: when
  you ship a new `restate-worker` image that changes the handler, you **register the new endpoint
  revision** and let the old one drain its in-flight invocations. Re-running `register` for an
  unchanged endpoint is a no-op.
- If this step is skipped, `PlatformEgress::post` invocations from producers will sit **pending** on
  the server (Restate retries them) until a handler is registered — not data loss, but egress
  stalls.

### Step C — flip `egress.mode` to `restate`  *(GitOps, in `ai-helm-values`)*

`egress.mode` is **deploy-scoped** (read once at boot, not live-reloadable — like every other config
knob). Set it, plus the ingress URL, in the producer roles' config:

```yaml
egress:
  mode: restate
  restateIngressUrl: http://restate.converse.svc.cluster.local:8080   # the server INGRESS (:8080)
  # (or set RESTATE_INGRESS_URL in the env; the config value wins when both are present)
```

- Confirm the exact Service DNS name of the Restate ingress from the `restate-helm` release before
  committing (the code's own example uses `restate.converse.svc.cluster.local:8080`).
- `restate` mode **requires** `restate_ingress_url` to resolve — the role fails fast at boot
  otherwise (a clear config error, not a silent fallback).
- Commit → ArgoCD syncs → producers reboot → on the next boot they `send`
  `PlatformEgress::post(outbox_id)` (idempotency key = the outbox `dedup_key`) and the reconciler
  drain goes quiet. Because both modes share the `outbox` ledger, any row written just before the
  cutover that the drain hadn't posted yet is picked up by whichever consumer the new deploy runs —
  **switching direction across a deploy is safe**.

> ⚠️ No GitHub→ArgoCD webhook: a values-only change can lag the ~180s poll. Nudge with
> `argocd app annotate <app> argocd.argoproj.io/refresh=hard` if you don't want to wait.

---

## Verify it's actually working

- **Trigger an egress event** (e.g. an `@mention` review on an approved repo, or a reaction) and
  confirm it appears on the PR.
- **Restate introspection** (admin API / UI): `PlatformEgress` shows invocations completing per
  `platform:installation` key; no invocations stuck pending or dead-lettered unexpectedly.
- **Postgres `outbox`** (authoritative ledger): rows move to `posted`; no growing backlog of
  `pending`, no unexpected `failed`. The Grafana egress panels keep working because they read
  Postgres, not the engine ([ADR-0046](../adr/0046-observability-dashboard-deployment.md)).
- **Reconciler**: its outbound drain is now idle; its **inbound** feedback poll (👍/👎 signal,
  ADR-0035) keeps running — that half of the role is unchanged by the pilot.
- **No duplicate posts**: spot-check that a single intent produced a single forge post (the whole
  point of per-key serialization).

---

## Rollback (safe, and the ADR-0074 exit-gate "no-go" action)

Flip back to the drain — no data migration, no cleanup:

1. In `ai-helm-values`, set `egress.mode: drain` on the producer roles. Commit → sync → reboot.
2. On the next boot, producers stop sending to Restate and the **reconciler drain resumes** posting
   from the shared `outbox`. Any intent Restate hadn't yet posted is a still-`pending` row the drain
   now picks up — the single-ledger design makes this lossless.
3. The credentials on `restate-worker` (step A) and the registered endpoint (step B) can stay — they
   do nothing while `mode = drain`. Remove them only if you are abandoning the pilot entirely.

---

## Exit gate (go / no-go for Restate Phase B — ADR-0074 §Gates)

Do **not** treat the pilot as "proven" until, after **≥ 3 weeks in prod**:

- [ ] **Zero lost/duplicate posts**, audited against `outbox` rows.
- [ ] **Dead-letter behaviour exercised at least once, deliberately** (e.g. target a deleted PR and
      confirm the `TerminalError` branch marks the `outbox` row `failed` — it does not retry forever).
- [ ] **One SDK/server upgrade absorbed** (register-new-revision + drain-old, end to end).
- [ ] An honest write-up of operational surprises.

Passing this gate is the precondition [ADR-0076](../adr/0076-restate-task-lifecycle-workflow.md)
(Restate Phase B — the task lifecycle becomes a workflow) is **explicitly gated on**. Failing it =
roll back (above), keep the engine for dev-only evaluation or remove it, and record the outcome in a
superseding ADR.

---

## See also

- [ADR-0074](../adr/0074-restate-egress-pilot.md) — the decision, gates, and rollback terms.
- [RFC-0005](../rfc/0005-durable-orchestration-on-restate.md) — the strangler migration and the full
  risk register / determinism rules.
- [ADR-0059](../adr/0059-reconciler-owns-all-github-egress.md) — the single-writer egress invariant
  whose *mechanism* the pilot replaces (the outbox intent-row design is retained).
- [ADR-0076](../adr/0076-restate-task-lifecycle-workflow.md) — what the exit gate unlocks (Phase B),
  and its own separate, harder gates.
