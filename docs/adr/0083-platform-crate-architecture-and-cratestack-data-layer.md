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

### 4. Workspace tooling & crates baseline

The revamp is the moment to fix the toolbox, once, workspace-wide. Verdict per candidate —
**adopt** / **already in** / **targeted** (specific crates only) / **declined** (with the reason,
so it isn't re-litigated per PR). Everything adopted goes through `[workspace.dependencies]`
(single-version rule) and, where marked, the `xtask` single-manifest check.

| Crate / tool | Verdict | Where & why |
|---|---|---|
| `bon` | **Adopt — the builder idiom** | Typed, compile-time-checked builders via `#[derive(Builder)]` / `#[builder]` on functions; zero runtime cost. The designated boilerplate killer for the wide constructor surfaces this revamp creates: `AgentLoop::new`, `ToolSpec`, `ChatRequest`, repository DTOs, config structs. House rule: **never hand-roll a builder again**; `bon` before any bespoke macro (§5). |
| `thiserror` | **Adopt** | Typed error enums (`StepError`, per-crate errors) replacing stringly `anyhow` at crate boundaries; `anyhow` stays at binary edges. |
| `clap` (derive + `env` + version) | **Adopt** | Replaces the hand-rolled `std::env::args` in the control-plane role mux and `xtask`. The `env` feature keeps the deploy contract intact: `--role` falls back to `CONTROL_PLANE_ROLE`, so charts change nothing. All bins gain `--version` (release/image traceability). |
| `config` (config-rs) | **Adopt, contract-preserving** | Becomes the loading engine inside `lci-config` (file + env layering, defaults). The external contract is frozen: same JSON shapes, `deny_unknown_fields`, read-once-at-boot, and the ADR-0021 `{env:VAR:-default}` markers keep working (prod ConfigMaps embed them) — a compat test renders a prod-shaped config and asserts identical resolution. |
| `camino` | **Adopt** | `Utf8PathBuf` for every path that serializes (JSON, logs, journal payloads — most of ours). Convert at `std::path` interop edges only. |
| `cargo-deny` (+ `deny.toml`) | **Adopt** | Advisories (subsumes `cargo-audit`'s DB), license allowlist, duplicate-version bans, source pinning — wired into `xtask` + CI. The **single-manifest placement rule stays in `xtask`** (deny can't express "this dep only in that crate"). Standalone `cargo-audit`: not needed. |
| `rustfmt.toml` | **Adopt (committed)** | Stable-channel options only: `edition` (2024 after R1a), `newline_style = "Unix"`, `use_field_init_shorthand`, `use_try_shorthand`. Nightly-only options (`imports_granularity`, `group_imports`, `wrap_comments`) deliberately excluded until fmt runs on nightly in CI — noted so nobody adds them and breaks stable contributors. |
| `schemars` | **Adopt (agent crates)** | Derives the JSON schema for tool argument structs — `ToolSpec` schemas stop being hand-written strings; pairs with the `Tool` derive macro (§5). |
| `insta` | **Adopt (dev-dep)** | Snapshot testing for the golden-transcript harness (`agent-testkit`) and the config compat tests — reviewable `.snap` diffs instead of hand-maintained fixtures. |
| `ignore` | **Adopt (runner/worker)** | Gitignore-aware repo walking (ripgrep's walker) for changed-file discovery and future repo iteration — the mature answer to the niche walkers below. |
| `mimalloc` | **Already in** | Workspace dep + `#[global_allocator]` in the service binaries (ADR-0080 musl rationale); the `agent-worker` bin adopts the same. |
| `tokio` | **Already in** | Workspace dep. New crates inherit; no second runtime. |
| `rayon` | **Targeted only** | For provably CPU-bound parallel work — realistically the indexer (tree-sitter parse/chunk). Bridged via `spawn_blocking`; **forbidden inside async handlers** (a rayon pool inside tokio workers is a latency footgun). Not a workspace default. |
| `crossbeam` | **Declined (for now)** | No current need tokio channels + `std::sync` don't cover; `std` scoped threads are stable. Revisit only with a measured contention case. |
| `fff` (`fff-search`) | **Targeted, future tool** | Verified healthy (9.6k★, v0.9.6 2026-06). An in-memory frecency/fuzzy/grep index built for AI agents in long-running processes — the natural engine for a future `find_files`/`grep_repo` **review tool** in the Phase-D `agent-worker` (a long-lived process amortizes the index; the one-shot Job cannot). Used as an in-process crate, not via its MCP server (knowledge-tools stay remote-HTTP-only, ADR-0066). A capability decision → its own slice, not a baseline dep. |
| `dir-structure` | **Declined** | Neat idea (typed directory layouts), but 1 star and zero published releases — the infrastructure maturity bar isn't met (the same bar ADR-0005 applied to cratestack 0.4.x, and that one had strategic commitment behind it). `ignore` + plain `std::fs` cover the actual need. |
| `fp-core.rs` | **Declined** | FP type-classes (Functor/Monad/HKT emulation) fight the borrow checker, wreck inference and compile errors, and create a dialect every contributor must learn; the crate is effectively unmaintained. `Option`/`Result`/`Iterator` + `?` already deliver the railway style; `itertools` as a dev-convenience covers most of the rest. Functional *discipline* yes — FP *framework* no. |

### 5. Repetition strategy: `bon` → `macro_rules!` → `lci-macros`

Repetitive code gets eliminated in a fixed escalation order, so macros stay a tool and never
become the codebase's personality:

1. **Ecosystem derives first** — `serde`, `bon`, `thiserror`, `clap`, `schemars` cover most
   boilerplate classes. A pattern one of these can express never gets a bespoke macro.
2. **Local `macro_rules!`** when a pattern repeats **≥ 3×** within one crate and a function or a
   generic can't express it. Lives next to its uses; not exported across crates.
3. **`lci-macros`** (a proc-macro crate, created **only when first needed**) for patterns that are
   cross-crate, non-`macro_rules!`-expressible, and **stable** — never macro a trait that is still
   moving. Every proc macro ships `trybuild` UI tests + a `cargo expand` snapshot (via insta) so
   the expansion stays reviewable.

Identified candidates, mapped to the ladder:

| Repetition | Mechanism | When |
|---|---|---|
| Wide constructors/builders everywhere | `bon` (level 1) | From R1a |
| Tool impls: arg parse + schema + kind/replay + error framing per tool | `#[derive(ReviewTool)]`-style attribute in `lci-macros` (level 3); schemas via `schemars` | **After** the `Tool` trait stabilizes (post-R1e) — macro a frozen trait, never a moving one |
| Repository CRUD forwarding to cratestack delegates | `delegate_repo!` `macro_rules!` in `lci-data` (level 2) | With the P4+ drawdown slices |
| `StepName` constants + the frozen-list stability test | `step_names!` `macro_rules!` (level 2) | R1a |
| Config env-fallback plumbing | **No macro** — absorbed by config-rs + serde defaults | The `lci-config` slice |
| Singleton advisory-lock wrapper | **No macro** — a plain `run_singleton(name, key, fut)` function | P3 |

Anti-goals, stated once: no DSLs; no macro-generated *public* API a reader can't follow in
rustdoc; `bon` over any hand-rolled builder macro; a macro that saves fewer lines than its own
definition + tests costs is deleted in review.

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
| R25 | Macro creep: `lci-macros` grows into a house DSL that hides control flow and stalls contributors | Medium | Medium | The §5 escalation ladder (ecosystem derive → local `macro_rules!` ≥3× → proc macro only for stable cross-crate patterns); trybuild + expansion snapshots make every macro reviewable; the "saves fewer lines than it costs → delete" rule |

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
