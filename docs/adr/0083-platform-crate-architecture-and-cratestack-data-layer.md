# ADR-0083: Platform crate architecture — domain crates, a `shared` role, and cratestack as the data layer

- **Status:** Proposed (design-only — nothing here is implemented; sequenced, see §Sequencing)
- **Date:** 2026-07-11
- **Deciders:** @stephane-segning
- **Relationships:** extends [ADR-0082](0082-restate-durable-agent-runtime.md)'s revamp from the
  agent loop to **every component**; **amends [ADR-0005](0005-cratestack-schema-first-control-plane.md)**
  (the codegen-deferral clause is superseded; the schema-first intent is finally executed).

## Context and Problem Statement

[ADR-0082](0082-restate-durable-agent-runtime.md) and its
[architecture companion](../restate-phase-d-agent-core-architecture.md) reorganize the **agent**
side of the workspace into a trait-seamed crate family. The rest of the platform has the same
disease in a bigger body: `services/control-plane` is a **~25,500-line single crate** hosting six
role entrypoints (`serve`, `dispatcher`, `reconciler`, `restate-worker`, `a2a`, `notifier` —
[main.rs:544-558](../../services/control-plane/src/main.rs)) over undivided modules:
[`db.rs`](../../services/control-plane/src/db.rs) alone is **4,930 lines of hand-written SQLx**,
the A2A family is ~5,500 lines, HTTP ~3,500, the queue machinery ~2,800, forge integrations ~3,400
(GitHub + GitLab since [ADR-0072](0072-platform-abstraction-layer.md)). Every role
compiles all of it; every domain's tests, deps, and reviewers travel together.

Three specific problems this ADR decides:

1. **No crate boundaries.** A change to A2A push rebuilds and re-reviews the dispatcher; `kube`,
   `sqlx`, and the whole HTTP surface are compile-time dependencies of a role that uses none of
   them. ADR-0082 already draws the seam style; the platform needs the same cut.
2. **A missing role: `shared`.** The role-per-Deployment model is right for our prod, but a
   customer/single-node/dev install wants **one process that runs everything**. Today that takes
   six Deployments or nothing. There is no all-in-one composition, and the singleton invariants
   (ADR-0059's single egress drain, the notifier's single claimer) are enforced by `replicas: 1`
   *convention*, which an all-in-one role would trample without a structural guard.
3. **The data layer never became schema-first.** [ADR-0005](0005-cratestack-schema-first-control-plane.md)
   *accepted* cratestack in June 2026 but deferred codegen ("0.4.x too young"); the `.cstack` file
   froze at the early data model while the live schema grew to **27 migrations** (platform_*,
   a2a_*, outbox, feedback, telemetry…), and `db.rs` became the largest hand-rolled surface in the
   workspace. The owner's direction now: **adopt cratestack as the high-level ORM** — SQLx stays
   underneath, hand-written SQL stops being the default.

What cratestack 0.4.9 actually offers (verified against cratestack.dev / rust-doc.cratestack.dev,
2026-07-11): facade crate **`cratestack-pg`** (Postgres); compile-time
`include_server_schema!("….cstack", db = Postgres)` emitting **SQLx-backed model delegates**
(`find_unique`/`find_many` with typed filters, `create_record`/`update_record`/`delete_record`,
batch ops, `.include(...)` relations, `.select(...)` projections), **`run_in_isolated_tx_with_retries`**
(configurable isolation, auto-retry on serialization failure/deadlock), banking-grade primitives
(idempotency, optimistic locking, audit logging, soft delete, forward-only migrations) — and,
decisive for us: **the data layer is usable standalone, without mounting the generated Axum
routes.** It is a v0-track framework: young, moving, and to be contained accordingly.

## Decision Drivers

- Same drivers as ADR-0082's revamp, workspace-wide: seams over monoliths, deps pinned to single
  manifests, unit-testable domains, hosts as thin assembly.
- **One binary, role-selected** (RFC-0001) has proven right operationally — keep it; add the
  missing composition instead of inventing a second deployment model.
- Singleton invariants should be **structural** everywhere, not `replicas: 1` comments — the same
  doctrine that motivated the Restate egress pilot (ADR-0074), applied to what stays outside
  Restate.
- 4,930 lines of hand-written SQL is the drift-and-drudgery ceiling on every schema change;
  ADR-0005's schema-first intent was right and never landed.
- Customer installs (and the AsciiDoc handbook, [ADR-0084](0084-customer-handbook-modular-asciidoc.md))
  need a deployment story simpler than six Deployments.

## Considered Options

**Data layer:**
- **A — cratestack-pg delegates as the data layer, ORM-only adoption** (this ADR): generated
  delegates behind per-domain repository traits; generated HTTP routes **not** mounted; raw SQLx
  escape hatch retained for the exotic queries.
- **B — stay on hand-written SQLx.** The proven path; keeps paying the 4,930-line tax and abandons
  ADR-0005's accepted intent.
- **C — a mainstream ORM (SeaORM/Diesel).** Mature, but abandons the `.cstack` schema-first
  lineage, the banking primitives, and the typed-client future ADR-0005 chose cratestack for; a
  second migration later if cratestack is still wanted.

**All-in-one:**
- **D — a `shared` role in the same binary** (this ADR): compose every role's run-loop in one
  process, singletons guarded structurally.
- **E — a compose/Helm profile that runs six single-replica Deployments.** No code, but six pods
  on a single node is exactly what a small install doesn't want, and it still leaves the singleton
  guards as convention.

## Decision Outcome

Adopt **A + D**, organized as a platform crate decomposition that mirrors ADR-0082's style.

### 1. The workspace DAG (all components)

The control plane decomposes into **five domain crates + the host binary** (packages take the
`lci-*` prefix — see the naming note in the [agent-core architecture](../restate-phase-d-agent-core-architecture.md)); together with
ADR-0082's agent family this is the whole workspace (arrows = depends-on, downward):

```
                 ┌────────────────┐    ┌────────────────┐
                 │  agent family  │    │    lci-data    │ cratestack schema + repositories
                 │   (ADR-0082)   │    │ (repositories) │ (the ONLY cratestack-pg/sqlx manifest)
                 └───────┬────────┘    └─┬────┬────┬────┘
                         │       ┌───────┘    │    └─────────┐
                 ┌───────▼───────▼┐   ┌───────▼────────┐   ┌─▼──────────────────┐
                 │  lci-platform  │   │   lci-egress   │   │      lci-a2a       │
                 │ CodePlatform + │   │ outbox shaping,│   │ protocol handler,  │
                 │ github, gitlab │   │ router, the    │   │ store, events,     │
                 └───────┬────────┘   │ PlatformEgress │   │ card, ssrf, push   │
                         │            │ handler (lib)  │   │ crypto + notifier  │
                 ┌───────▼────────┐   └───────┬────────┘   │ delivery (lib)     │
                 │   lci-queue    │           │            └─┬──────────────────┘
                 │ dispatch, reap,│           │              │
                 │ sweep + the k8s│           │              │
                 │ Job launcher   │           │              │
                 └───────┬────────┘           │              │
                         └───────────┬────────┴──────────────┘
                             ┌───────▼───────┐
                             │ control-plane │  the HOST: role mux (serve, dispatcher,
                             │   (binary)    │  reconciler, restate-worker, a2a, notifier,
                             │ http/, jwt,   │  NEW `shared`), Axum routers, config, metrics
                             │ config, main  │
                             └───────────────┘
```

| Crate (`lci-…`) | Takes over (today) | Single-manifest deps it owns |
|---|---|---|
| `data` | `db.rs` (4,930) decomposed into per-domain repositories; the `.cstack` schema; `types.rs` rows; `integrations/neo4j.rs` (it is a store client) | **`cratestack-pg`** (and thereby `sqlx`), `neo4rs` |
| `platform` | `integrations/platform.rs` + `github.rs` + `gitlab.rs` (ADR-0072 trait + impls) | forge HTTP (via workspace `reqwest`) |
| `egress` | `egress.rs`, `outbox.rs`, and `restate_worker.rs`'s `PlatformEgress` handler **as a library** (the role stays a host entrypoint) | — |
| `queue` | `queue/{dispatcher,reaper,a2a_sweeper}.rs` + `integrations/k8s.rs` (the Job launcher) | **`kube`** |
| `a2a` | the `a2a/` module family (handler, store, mapping, events, card, ssrf, push_crypto) + `queue/notifier.rs`'s delivery loop as a library | `a2a-server-lf` SDK, `ring` (push crypto) |
| `control-plane` (host) | `main.rs` role mux, `http/` (webhook, internal, admin, api), `review.rs` orchestration, `jwt.rs`, `config.rs`, metrics | `axum`, **`restate-sdk`** (serves the egress handler; ADR-0082's `agent-worker` is the other SDK host) |

Rules carried over from ADR-0082 verbatim: crates are **born on edition 2024** (workspace bump is
R1 step 0 there); the `xtask` dependency-hygiene check extends to the table above (`cratestack-pg`,
`sqlx`, `kube`, `restate-sdk` each in exactly one manifest); domain crates never depend on each
other sideways except through the arrows drawn (e.g. `queue` calls `egress` for the 👀 reaction, it
does not reach into `platform` directly).

`queue/reconciler.rs` splits along the seam it already has: the **drain** halves live in `egress`
(they are the delivery path), the **feedback poll** in `platform` (it reads forge reactions).

### 2. Roles — kept, plus the missing `shared`

The role mux stays exactly as it is (`CONTROL_PLANE_ROLE`, one image, RFC-0001), with each role's
body shrinking to assembly over the domain crates. One addition:

**`shared` — the all-in-one role.** `CONTROL_PLANE_ROLE=shared` runs, in one process under one
supervisor: the `serve` HTTP surface, the dispatcher loop, the reconciler (drain + feedback poll),
the a2a surface, the notifier, and the restate-worker SDK endpoint *when configured* (absent a
Restate server it degrades to drain-mode exactly as the dedicated roles do). Intended for
single-node/customer installs, dev, and the handbook's "smallest footprint" story
([ADR-0084](0084-customer-handbook-modular-asciidoc.md)); prod at scale keeps dedicated roles.

The invariant problem `shared` must not trample: several subsystems are **singletons by
convention** today — the reconciler's egress drain (ADR-0059), the notifier's claim loop, the
dispatcher's reap/prune/purge ticks. A `shared` replica running next to a dedicated reconciler (or
`shared` scaled to 2) would double-post. The guard becomes **structural**:

- Every singleton subsystem takes a **Postgres advisory lock** (`pg_try_advisory_lock` on a
  stable per-subsystem key) before running its loop; the non-holder idles and retries on a slow
  tick, logging that it is standing by. Locks are session-scoped on the existing pool — crash =
  release = takeover, no lease table needed.
- This upgrades the `replicas: 1` comments to enforcement **for every deployment shape** (it also
  hardens the dedicated roles during RollingUpdate overlap windows), is orthogonal to the Restate
  egress path (in `egress.mode=restate` the drain singleton is simply never contended), and costs
  one round-trip per loop start.

The `shared` role composes what the six roles compose — there is no seventh code path; that is
what the crate decomposition buys. Config remains one file; subsystems read their own sections.

### 3. The data layer — cratestack as the high-level ORM

**Shape of adoption (ORM-only, contained):**

- `lci-data` holds the regenerated `.cstack` and the single
  `include_server_schema!("schema/control-plane.cstack", db = Postgres)` invocation. **The
  generated Axum routes are not mounted** — delegates only. Generated types stay `pub(crate)`.
- Every domain gets a **repository trait** (`TaskRepo`, `OutboxRepo`, `A2aTaskRepo`, `PushRepo`,
  `FeedbackRepo`, `TelemetryRepo`, …) defined next to its consumers' needs and implemented in
  `lci-data` over the delegates. Domain crates and hosts speak repositories, never
  cratestack types — the same seam discipline as ADR-0082's `Tool`/`StepRuntime`, and the swap/test
  seam if cratestack v0 churns (R20). `agent-testkit`-style in-memory fakes come for free.
- **Transactions:** multi-row invariants move onto `run_in_isolated_tx_with_retries` (the
  serialization-retry loop we hand-roll around `23505` today is exactly what it automates).
- **The raw-SQL escape hatch is a listed, deliberate residue** — cratestack is SQLx underneath, so
  the same pool serves both. Stays raw: the `FOR UPDATE SKIP LOCKED` claim queries
  (`claim_next_task`, outbox claim, push-config claim), `LISTEN/NOTIFY`, the CTE-heavy/EXPLAIN-tuned
  sweeps (`sweep_terminal_a2a_tasks`, purge), and pgvector operators. Each raw survivor is named in
  the repo impl with a `// raw:` comment stating why. Target steady state: delegates for the ~80%
  CRUD/lookup surface, raw for the hot/exotic ~20%.
- **Schema truth and migrations:** the `.cstack` is **regenerated from the live 27-migration
  schema** and resumes as the reviewable source of truth (ADR-0005's intent, executed). The
  existing forward-only SQL migration pipeline **stays** — cratestack's own migration engine is
  *not* adopted in this ADR; a CI gate (`just validate-schema` grown up: validate the `.cstack` +
  diff it against a migrated scratch database) makes schema drift a build failure instead of a
  code-review hope.
- **Version containment:** pin `cratestack-pg` exactly (v0 track, breaking minors expected — same
  posture as `restate-sdk =0.10.0`); single-manifest rule; upgrades deliberate with the parity
  suite green.

**Why now, when ADR-0005 said "too young":** the deferral was correct for *binding the whole
server* to a two-day-old codegen. What changed: adoption is **ORM-only behind repository traits**
(the blast radius of churn is one crate's impls, not the platform), the delegate/transaction
surface we need is verified real, and the alternative is compounding 4,930 hand-written lines. The
containment — pin + seam + parity tests + escape hatch — is the difference between this ADR and
the bet ADR-0005 rightly declined.

## Sequencing (P-series; interleaves with ADR-0082's R-series, independent of Restate gates)

- **P0 — schema truth:** regenerate `.cstack` from the live schema; CI validate + drift gate. No
  runtime change.
- **P1 — data spike (the S1 gate for cratestack):** `lci-data` skeleton; port **one
  low-risk domain** (review-run telemetry) to delegates behind its repo trait, with a parity
  suite (same `#[sqlx::test]` corpus, old vs new implementations, identical results) and a perf
  sanity check on a claim-adjacent path. **Failing P1 falls back to Option B for the data layer**
  (repositories stay, impls stay raw SQLx) — the crate decomposition does not depend on cratestack.
- **P2 — domain-crate extraction:** `platform`, `egress`, `queue`, `a2a` crates carved out
  (mechanical moves, importers updated; same move-don't-change discipline as R1).
- **P3 — `shared` role + advisory-lock singletons:** locks land first on the dedicated roles (a
  hardening no-op), then the `shared` composition ships behind them.
- **P4…Pn — `db.rs` drawdown:** repository-by-repository migration to delegates, each slice
  parity-tested; `db.rs` deleted when empty.

## Consequences

- **Good:** the platform gets the agent family's properties — reviewable domains, single-manifest
  deps, thin hosts; a role touching A2A no longer rebuilds the dispatcher.
- **Good:** `shared` gives customers/dev a one-process install, and the advisory-lock guards make
  *every* singleton structural — including during RollingUpdate overlaps on today's dedicated
  roles.
- **Good:** the schema returns to one reviewable source of truth with a drift gate; ~80% of 4,930
  hand-written SQL lines become generated delegates; serialization-retry and optimistic-locking
  stop being hand-rolled.
- **Bad:** cratestack is v0 — API churn lands on us (contained to `lci-data` by the
  repository seam, but real; R20).
- **Bad:** the migration window runs two data paths (delegates + raw) over one pool; parity suites
  and the per-slice sequencing bound it, but reviewers must hold the line on "every P-slice ships
  parity-green" (R23).
- **Bad:** `shared` is a new supportable surface — a customer running it will hit combinations
  (e.g. restate-worker absent, notifier on) prod never runs; the handbook (ADR-0084) and the role
  matrix in CI smoke tests carry that.
- **Neutral:** generated Axum routes and typed clients remain unused capacity; if the platform
  later wants them (e.g. for the admin surface), that is a new ADR on an already-adopted schema.

### Risk register (extends ADR-0082's; IDs continue)

| # | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| R20 | cratestack v0 churn breaks `lci-data` on upgrade | High | Medium | Exact pin; repository seam confines the blast radius to one crate's impls; parity suite re-run per upgrade; Option-B fallback (raw impls behind the same traits) recorded |
| R21 | Advisory-lock semantics surprise (pool reconnects release session locks; a wedged holder starves the subsystem) | Medium | Medium | Session-scoped locks on a dedicated connection with health-checked renewal; standby logs loudly; the existing per-loop metrics show a silent-standby immediately |
| R22 | `shared` role config/behavior drift vs dedicated roles (combinations prod never exercises) | Medium | Medium | `shared` is pure composition of the same crate entrypoints (no seventh code path); CI smoke matrix boots `shared` and asserts each subsystem's readiness; handbook documents the supported matrix |
| R23 | Dual data-path window: a repo migrated to delegates diverges subtly from the raw twin it replaced | Medium | High (silent data drift) | Per-slice parity suites on the real corpus; slices small; `// raw:` residue is named and reviewed; drift gate keeps schema honest |
| R24 | The `.cstack` regeneration mis-models a live corner (27 migrations of accretion) | Medium | Medium | P0 diffs the `.cstack`-derived DDL against a scratch DB migrated by the real pipeline; discrepancies block at CI, not in prod |

## More Information

- [ADR-0005](0005-cratestack-schema-first-control-plane.md) — the original adoption + deferral
  this ADR executes and supersedes-in-part (see its 2026-07-11 update note).
- [ADR-0082](0082-restate-durable-agent-runtime.md) + the
  [agent-core architecture](../restate-phase-d-agent-core-architecture.md) — the agent half of the
  workspace and the seam style this extends; the R-series this P-series interleaves with.
- [ADR-0072](0072-platform-abstraction-layer.md) — the `CodePlatform` trait that
  becomes `lci-platform`'s core.
- [ADR-0059](0059-reconciler-owns-all-github-egress.md) / [ADR-0074](0074-restate-egress-pilot.md)
  — the singleton-egress invariant the advisory locks generalize and the Restate path they
  complement.
- [ADR-0084](0084-customer-handbook-modular-asciidoc.md) — the customer handbook that documents
  the role matrix, including `shared`.
- cratestack: <https://cratestack.dev> · <https://rust-doc.cratestack.dev/cratestack> ·
  facade `cratestack-pg`, verified 0.4.9 (2026-06-17).
- Current implementation being decomposed:
  [`db.rs`](../../services/control-plane/src/db.rs) (4,930),
  [`a2a/`](../../services/control-plane/src/a2a/handler.rs) (~5,500),
  [`queue/`](../../services/control-plane/src/queue/dispatcher.rs) (~2,800),
  [`integrations/`](../../services/control-plane/src/integrations/platform.rs) (~3,400),
  [`main.rs`](../../services/control-plane/src/main.rs) (role mux).
