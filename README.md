# Lightbridge Code Intelligence

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![GitHub App](https://img.shields.io/badge/GitHub-App-green.svg)](https://github.com/apps/lightbridge-assistant)

Lightbridge is a GitHub App for **intelligent code review and repository Q&A**. It listens for
GitHub webhook events, records work in a Rust control plane, and runs each task in an isolated,
short-lived Kubernetes Job. The work is backed by **repository-aware retrieval** over two
complementary indexes — a Neo4j knowledge graph (structure) and pgvector (semantics) — with reasoning
performed by a **native Rust review agent** that acts through mediated write tools
([ADR-0026](docs/adr/0026-native-review-agent.md),
[ADR-0037](docs/adr/0037-agent-acts-via-mediated-tools.md)).

Reviews run in **two tiers** ([ADR-0062](docs/adr/0062-two-tier-review-fast-auto-deep-on-demand.md)): a
**fast** automatic pass on every opened PR — deterministic [SAST](docs/adr/0061-sast-deterministic-finding-source.md)
plus a lean, diff-only LLM pass with no retrieval — and a **deep**, repo-aware review on `@mention`
(full graph + vector retrieval, multi-turn). See [docs/review-pipeline.md](docs/review-pipeline.md).

The Rust control plane is the **trust boundary**: it holds the GitHub App private key and mints
short-lived, per-task installation tokens — the App key itself never reaches a Job. The agent only
proposes; the control plane validates results and is the **sole writer** to GitHub — every write flows
through one outbox drained by the `reconciler` role
([ADR-0059](docs/adr/0059-reconciler-owns-all-github-egress.md)).

---

## System architecture

One control-plane binary runs in three roles (`serve`, `dispatcher`, and `reconciler` —
[ADR-0058](docs/adr/0058-rename-poller-role-to-reconciler.md)); the actual repository work runs in
disposable per-task Jobs. `serve` handles the HTTP API + reads, `dispatcher` consumes the work queue
and launches Jobs, and `reconciler` drains the GitHub-egress outbox (the single writer to GitHub) and
polls feedback. A Job never receives the GitHub App key — it bootstraps a short-lived installation
token at runtime — but it **is** injected with a per-task, short-lived runner token
(`AGENT_RUNNER_TOKEN`, minted per Job from `RUNNER_TOKEN_SIGNING_KEY`, [ADR-0092](docs/adr/0092-per-task-runner-tokens.md))
and the embeddings API key it needs to do its work (see [Secrets a Job holds](#secrets-a-job-holds)).

```mermaid
flowchart TD
    subgraph external["External services"]
        GH["GitHub App<br/>(webhooks + API)"]
        KC["Keycloak<br/>(OIDC IdP)"]
        EAIG["eaig / core-gateway<br/>(OpenAI-compatible embeddings)"]
    end

    WEB["Next.js web console<br/>(OIDC Auth Code + PKCE)"]

    subgraph cp["Rust control plane — trust boundary"]
        SERVE["role: serve<br/>HTTP API · mints tokens · validates JWT · mediated tools"]
        DISP["role: dispatcher<br/>queue consumer · launches Jobs"]
        RECON["role: reconciler<br/>GitHub-egress outbox · feedback poll"]
        Q[("Postgres<br/>work queue + outbox")]
    end

    subgraph job["Kubernetes Job (ephemeral, per task)"]
        RUNNER["agent-runner<br/>clone · index · review"]
        TS["tree-sitter chunker"]
        GFY["lci-codegraph<br/>(in-process, tree-sitter)"]
        AGENT["native review agent<br/>(in-process, ADR-0026/0037)<br/>SAST + LLM over mediated tools"]
    end

    VEC[("pgvector<br/>semantic")]
    NEO[("Neo4j<br/>structural")]

    KC -. login .-> WEB
    WEB -->|"Bearer JWT (read)"| SERVE
    GH -->|webhook| SERVE
    SERVE -->|enqueue| Q
    Q -->|"claim (SKIP LOCKED)"| DISP
    DISP -->|"one Job per task"| RUNNER
    SERVE <-->|"context · token · status · chunks · tool calls"| RUNNER
    RUNNER --> TS
    RUNNER --> GFY
    RUNNER --> AGENT
    TS -->|embeddings| EAIG
    EAIG --> VEC
    GFY --> NEO
    SERVE -. "retrieval (mediated)" .-> VEC
    SERVE -. "retrieval (mediated)" .-> NEO
    AGENT -. "findings (buffered)" .-> SERVE
    SERVE -->|"enqueue write"| Q
    Q -->|"claim outbox"| RECON
    RECON -. writeback .-> GH
```

> The full flow — clone → dual index → review → validated write-back — is **implemented and deployed**.
> The review agent is the **native in-process Rust loop** ([ADR-0026](docs/adr/0026-native-review-agent.md),
> [ADR-0037](docs/adr/0037-agent-acts-via-mediated-tools.md)); OpenCode has been retired. Reviews are
> two-tier ([ADR-0062](docs/adr/0062-two-tier-review-fast-auto-deep-on-demand.md)) and a deterministic
> SAST pass rides the same channel ([ADR-0061](docs/adr/0061-sast-deterministic-finding-source.md)).

See [docs/architecture.md](docs/architecture.md), [docs/jobs-and-lifecycle.md](docs/jobs-and-lifecycle.md),
and [docs/INDEX.md](docs/INDEX.md) for the full picture.

---

## Why two indexes? (semantic chunks **and** a structural graph)

This is the most common point of confusion, so it's worth being explicit: the two index passes are
**not** doing the same job twice. Both are driven from **one tree-sitter parse of the checkout** — the
semantic **chunker** and the in-house **lci-codegraph** structural extractor — and they feed **two
different stores that answer two different kinds of question** — the dual-retrieval design
([ADR-0003](docs/adr/0003-dual-retrieval-neo4j-pgvector.md),
[ADR-0010](docs/adr/0010-graphify-treesitter-indexing-baseline.md), superseded for the graph half by
[ADR-0086](docs/adr/0086-in-house-code-graph-crate.md)). A good code review needs both kinds of recall,
and no single store does both well.

```mermaid
flowchart LR
    SRC["Repo checkout<br/>(one clone, in the runner)"]

    SRC --> TS["tree-sitter chunker<br/>splits into semantic units"]
    SRC --> GFY["lci-codegraph<br/>extracts symbols + relationships<br/>(in-process, tree-sitter)"]

    TS --> VEC[("pgvector<br/>embedding per chunk")]
    GFY --> NEO[("Neo4j<br/>typed graph")]

    VEC --> QS["Semantic question:<br/>'where is similar behaviour?'"]
    NEO --> QG["Structural question:<br/>'what calls this? PR impact?'"]
```

| | **tree-sitter chunker → pgvector** | **lci-codegraph → Neo4j** |
|---|---|---|
| Kind of recall | **Semantic** (vector similarity) | **Structural** (graph traversal) |
| Question it answers | "where is similar code / behaviour?", natural-language search | "what calls this function?", "what does this PR touch?", containment, test ownership |
| What it emits | embedding-sized chunks with stable source ranges | nodes (symbols, files) + edges (contains, method, calls) |
| Why this tool | purpose-built, lightweight, in-process Rust we control; chunk boundaries are a *chunking* concern | in-house Rust crate over the same tree-sitter parse — no Python subprocess, no separate image ([ADR-0086](docs/adr/0086-in-house-code-graph-crate.md)) |
| Can the other store answer it? | ❌ a graph can't rank by semantic similarity | ❌ vector search can't enumerate exact callers |
| Status | ✅ built (slice 2) | ✅ built (slice 3; in-house crate, ADR-0086) |

Both run in the **same runner Job over the same checkout** — one indexes for *fuzzy* retrieval, the
other for *exact* retrieval. The reasoning agent (slice 5) then queries each store via MCP for the
question it's best at.

---

## Task lifecycle

A task is created from a webhook, parked in the Postgres queue, claimed by a dispatcher under a
lease, and executed in a Job. Statuses below are the ones the runner reports back to the control
plane.

```mermaid
stateDiagram-v2
    [*] --> queued: webhook → enqueue (idempotent)
    queued --> running: dispatcher claims (lease) + Job starts

    state running {
        [*] --> cloning
        cloning --> indexing: checkout ready
        indexing --> reasoning: indexes ready (or reused, ADR-0025)
        reasoning --> [*]: review submitted
    }

    running --> posting_result: validated write-back to GitHub
    posting_result --> succeeded
    running --> succeeded: index-only task

    running --> failed: error reported
    running --> timed_out: activeDeadline exceeded
    queued --> cancelled: superseded / cancelled

    succeeded --> [*]
    failed --> [*]
    timed_out --> [*]
    cancelled --> [*]
```

> There are **two job kinds**: an `index` task (on repo approval / default-branch push) and a `review`
> task. A review carries a **tier** ([ADR-0062](docs/adr/0062-two-tier-review-fast-auto-deep-on-demand.md)):
> `fast` on PR-opened, `deep` on `@mention`. A warm review **reuses the base index**
> ([ADR-0025](docs/adr/0025-review-reuses-base-index.md)). The authoritative state machine, cancellation,
> and data-purge flows live in [docs/jobs-and-lifecycle.md](docs/jobs-and-lifecycle.md).

---

## Indexing flow (sequence)

How a single task gets from a webhook to stored vectors. Note the runner never holds the GitHub App
key — it borrows a short-lived installation token from the control plane just-in-time
([ADR-0002](docs/adr/0002-rust-control-plane-trust-boundary.md),
[ADR-0017](docs/adr/0017-agent-runner-control-plane-bootstrap.md)).

```mermaid
sequenceDiagram
    participant GH as GitHub
    participant CP as Control plane (serve)
    participant DB as Postgres (+pgvector)
    participant DP as Dispatcher
    participant R as Agent runner (Job)
    participant E as eaig (embeddings)

    GH->>CP: webhook (PR / command)
    CP->>DB: enqueue task (idempotent)
    DP->>DB: claim next (SKIP LOCKED) + lease
    DP->>R: launch Kubernetes Job
    R->>CP: GET /internal/tasks/{id}
    CP-->>R: context + short-lived install token
    R->>GH: clone @ head SHA (token)
    R->>R: tree-sitter chunk (Rust/TS/JS/Python)
    loop batches of 32 chunks
        R->>E: POST /v1/embeddings
        E-->>R: vectors
        R->>CP: POST /internal/tasks/{id}/chunks
        CP->>DB: upsert code_chunks (vector)
    end
    R->>CP: POST /internal/tasks/{id}/status = succeeded
    Note over R,GH: A review task also runs lci-codegraph→Neo4j + the review agent → validated write-back
```

### Secrets a Job holds

The trust boundary is specifically about the **GitHub App private key**, which never leaves the
control plane — a Job mints a short-lived (~1h), installation-scoped token at runtime instead. A Job
is **not** credential-free, though. Today the dispatcher injects into every runner pod
([`k8s.rs`](services/control-plane/src/integrations/k8s.rs)):

| Secret | Source | Lifetime | Notes |
|---|---|---|---|
| GitHub installation token | minted per task by `serve` | ~1h, auto-expires | the only GitHub credential a Job sees |
| `AGENT_RUNNER_TOKEN` | plaintext env in the pod spec, but minted per task | scoped to ONE task, expires with its Job's `activeDeadlineSeconds` | signed by `RUNNER_TOKEN_SIGNING_KEY` (control-plane-only, never in a Job); bearer for the internal API — a leaked value authenticates only this task ([ADR-0092](docs/adr/0092-per-task-runner-tokens.md)) |
| `EMBEDDINGS_API_KEY` | `secretKeyRef` → `lightbridge-agent-secrets` | long-lived, shared | the embeddings gateway key ([ADR-0018](docs/adr/0018-openai-compatible-embeddings.md)) |
| internal-CA cert | mounted from a Secret | n/a | trusts the internal HTTPS embeddings gateway |

So "no long-lived secrets in the Job" is **not** accurate — only the GitHub App key is withheld, and
`EMBEDDINGS_API_KEY` is still a standing shared secret. `AGENT_RUNNER_TOKEN` is no longer one of the
long-lived ones, though: [ADR-0092](docs/adr/0092-per-task-runner-tokens.md) narrowed it to a per-task,
short-lived signed token.

---

## Monorepo layout

A pnpm + Turborepo monorepo with a Cargo workspace and an `xtask` for Rust automation
([ADR-0009](docs/adr/0009-pnpm-turborepo-monorepo.md)). Three language stacks live side by side:
**TypeScript** (`apps/`, `packages/`), **Rust** (`services/`, `xtask/`), and **Python** (`tools/`).

| Path | Stack | What it is |
|---|---|---|
| `apps/web` | TS | Next.js (App Router) web console; OIDC Auth Code + PKCE login against Keycloak ([ADR-0014](docs/adr/0014-keycloak-oidc-resource-server.md)). [README](apps/web/README.md) |
| `packages/auth` | TS | Shared OIDC/JWT helpers (token verification, claims, session cookie) |
| `packages/tsconfig` | TS | Shared TypeScript configs |
| `services/control-plane` | Rust | Axum control plane; Postgres via hand-written SQLx (cratestack deferred — [ADR-0005](docs/adr/0005-cratestack-schema-first-control-plane.md)). Runs as `serve`, `dispatcher`, or `reconciler` ([ADR-0058](docs/adr/0058-rename-poller-role-to-reconciler.md)). [README](services/control-plane/README.md) |
| `services/agent-runner` | Rust | Per-task Job: bootstraps from the control plane, clones, indexes (pgvector + Neo4j), and runs the native review agent + SAST. [README](services/agent-runner/README.md) |
| `services/config` | Rust | Shared config loader: one JSON file + `{env:VAR:-default}` substitution, used by both Rust services. [README](services/config/README.md) |
| `xtask` | Rust | Cargo `xtask` workspace automation — the Rust side of `just` (`cargo xtask ci\|fmt\|lint\|test\|build`) |
| `tools/dashboard-gen` | Python | Generates the Grafana dashboards-as-code into the Helm chart. [README](tools/dashboard-gen/README.md) |
| `docs/` | — | Documentation set, ADRs, RFCs, ways of working |
| `deploy/` | — | Observability dashboards/chart + Keycloak realm export for the `ai-helm` deployment. Production image tags live in [`adorsys-gis/ai-helm-values`](https://github.com/adorsys-gis/ai-helm-values) (promoted by argocd-image-updater, GitOps). See [docs/kubernetes-deployment.md](docs/kubernetes-deployment.md). |

### Kubernetes layout

The control plane (`serve` + `dispatcher` + `reconciler`) and the web console run in the platform
namespace; each task runs as a Job in a dedicated **agents** namespace (`AGENT_NAMESPACE`, default
`lightbridge-agents`); the data stores (Postgres/pgvector, Neo4j) are managed services. Delivery is
GitOps (the `ai-helm` chart + per-env `ai-helm-values`, promoted by argocd-image-updater). See
[docs/kubernetes-deployment.md](docs/kubernetes-deployment.md).

---

## Prerequisites

- **Node ≥ 22** (pinned in `.nvmrc`)
- **pnpm**
- **Rust** (stable toolchain + `cargo`)
- **just** (task runner)
- **docker** (for the local data plane via docker compose)

Optional: `cargo-nextest` (test runner), `multipass` (tentative local k3s cluster).

## Quick start

Using `just` (the single human-facing entrypoint):

```bash
just setup   # pnpm install + cargo fetch
just up      # docker compose up -d  (Postgres+pgvector, Neo4j)
just dev     # run web + control plane via Turborepo
```

Raw equivalents (if you prefer not to use `just`):

```bash
pnpm install && cargo fetch          # just setup
docker compose up -d                 # just up
pnpm dev                             # just dev

# Run only one side / role:
cargo run -p control-plane           # serve role (default)
cargo run -p control-plane dispatcher # dispatcher role
pnpm --filter @lightbridge/web dev   # just dev-web
```

## Quality gates

Run these locally **before pushing** (shift-left):

```bash
just lint   # pnpm lint + cargo clippy --all-targets -- -D warnings
just test   # pnpm test + cargo nextest run
just fmt    # biome + rustfmt
```

`just ci` runs the full local gate (lint, build, `cargo xtask ci`).

## How we work

We follow **XP + Lean + DevOps with shift-left** delivery, under the ADORSYS-GIS **AI Governance**
framework (Definition of Ready/Done, AI usage declarations). See
[ways of working](docs/ways-of-working/engineering-practices.md) and
[OKRs](docs/ways-of-working/okrs.md).

## Documentation

- [Documentation index](docs/INDEX.md)
- [Architecture Decision Records](docs/adr/README.md)
- [RFCs](docs/rfc/README.md)
- [Contributing](CONTRIBUTING.md)

## Development status

**Core shipped and running in production.** The end-to-end path is live: webhooks → Postgres work
queue + dispatcher → per-task Job → dual index (pgvector + Neo4j) → **native review agent** → validated
write-back through the `reconciler` outbox, plus the admin/governance web console (repo approval gate,
permission-based authz, runs + insights). Deployment is GitOps (per-env Helm values → the `ai-helm`
chart → ArgoCD).

Shipped since the initial cut: the **native Rust review agent** (OpenCode retired,
[ADR-0026](docs/adr/0026-native-review-agent.md)/[ADR-0037](docs/adr/0037-agent-acts-via-mediated-tools.md)),
**two-tier review** ([ADR-0062](docs/adr/0062-two-tier-review-fast-auto-deep-on-demand.md)), a
deterministic **SAST** pass ([ADR-0061](docs/adr/0061-sast-deterministic-finding-source.md)), the
single-egress **reconciler** ([ADR-0058](docs/adr/0058-rename-poller-role-to-reconciler.md)/[ADR-0059](docs/adr/0059-reconciler-owns-all-github-egress.md)),
and finding verification/refute + feedback memory. See the
[Issues](https://github.com/vymalo/lightbridge-code-intelligence/issues) for the roadmap.

## License

MIT — see [LICENSE](LICENSE).

## Acknowledgments

- [Graphify](https://github.com/safishamsi/graphify) — multi-modal graph extraction (used in the original structural indexer; since retired for the in-house `lci-codegraph` crate, [ADR-0086](docs/adr/0086-in-house-code-graph-crate.md))
- [tree-sitter](https://tree-sitter.github.io/) — syntax-aware parsing / chunking
- [OpenCode](https://opencode.ai) — agent reasoning framework (used in the original review agent; since retired for the native loop, [ADR-0026](docs/adr/0026-native-review-agent.md))
- [Neo4j](https://neo4j.com/) — graph database
- [pgvector](https://github.com/pgvector/pgvector) — PostgreSQL vector extension
- [Keycloak](https://www.keycloak.org/) — OIDC identity provider
