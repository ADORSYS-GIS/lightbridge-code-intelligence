# Lightbridge Code Intelligence — Project Overview

## What is this?

Lightbridge is a **GitHub App for intelligent, automated code review**. It listens for PR events,
clones the changed repository, builds a rich searchable index (semantic + structural), runs SAST,
and performs an agentic review — posting findings as inline PR comments.

The project is a **pnpm + Turborepo monorepo** with a **Cargo Rust workspace** for all backend
services. There is also a Next.js web console (`apps/web`) for user-facing task management via
Keycloak OIDC.

---

## Duality

The system is split into two trust strata:

| Layer | Component | Trusted? | What it does |
|-------|-----------|----------|--------------|
| **Control plane** | `services/control-plane` | ✅ **Trust boundary** | Receives GitHub webhooks, mints short-lived tokens, dispatches tasks, owns the database, validates all outputs, and posts *everything* to GitHub via a single reconciler outbox. |
| **Agent runner** | `services/agent-runner` | ❌ Untrusted per-task K8s Job | Clones the repo, builds an index (tree-sitter → embeddings → pgvector, Graphify → Neo4j), runs SAST, and runs a native Rust review agent — but **never holds the GitHub App key** and never writes to GitHub directly. |

---

## Architecture in one picture

```
GitHub Webhook ──▶  Control Plane (serve role)
                         │
                    ┌────┴────┐
               Queue/DB │    │ K8s API
                    ┌────┘    └────┐
               Dispatcher      Reconciler
                    │              │
                    ▼              ▼
          Kubernetes Job       GitHub API
         (agent-runner)     (single outbox)
                    │
          ┌─────────┼─────────┐
          ▼         ▼         ▼
   Clone → Index → SAST → Review
           (tree-    (open-   (native
            sitter +  grep)    agent
            Graphify)          loop)
```

---

## Key AI/ML terms explained relative to the implementation

### Vector / Embeddings

**What they are in general:**
An *embedding* is a dense vector (array of floating-point numbers) that represents the *meaning*
of a piece of text. Similar texts produce vectors that are close together in the vector space
(by cosine distance). This is computed by a machine-learning model (e.g. `text-embedding-3-small`).

**What Lightbridge does with them:**
Every indexed code file is parsed by tree-sitter into logical chunks (functions, structs, classes,
or line windows). Each chunk is sent to an **OpenAI-compatible embeddings API** (the eaig/core-gateway
— an Envoy AI Gateway) which returns a 1536- or 4096-dimensional `Vec<f32>`. These embeddings are
stored in **pgvector**, a Postgres extension. When the review agent searches semantically, it embeds
the search query the same way and runs a nearest-neighbour search in pgvector (`<=>` cosine distance).

**Key files:**
- `services/agent-runner/src/indexer/embeddings.rs` — the HTTP client that talks to `POST /v1/embeddings`
- `services/agent-runner/src/indexer/mod.rs` — batches chunks and calls the embedder

### Semantic Indexing

**What it is:**
The process of parsing source code into meaningful units and building a vector index so that code
can be searched by *intent* ("find where we validate sessions") rather than by keyword.

**How Lightbridge does it:**
1. Walk the cloned repository, skipping non-code directories (`node_modules`, `.git`, etc.).
2. For each source file, determine the language via file extension and content heuristics.
3. Run **tree-sitter** (a parsing library with grammars for Rust, TypeScript, JavaScript, Python)
   to extract named AST nodes: functions, structs, classes, methods, impl blocks.
4. For languages/structures without a grammar, fall back to fixed-size line windows (100 lines,
   50-line overlap).
5. Embed each chunk → submit to control plane → store in pgvector.

**Key files:**
- `services/agent-runner/src/indexer/chunker.rs` — tree-sitter AST extraction + windowed fallback
- `services/agent-runner/src/indexer/language.rs` — language detection by file extension

### Knowledge Graph / Structural Graph

**What it is:**
A graph database (Neo4j) that stores code *symbols* (functions, classes, methods) and their
*relationships* ("contains", "calls", "method of"). This lets the review agent ask questions
like "what calls this function?" or "find all symbols named `validate`".

**How Lightbridge builds it:**
The agent-runner spawns **Graphify CLI** as `graphify update <checkout> --no-cluster` (AST-only,
no LLM pass). Graphify produces a `graph.json` with code nodes + edges. The runner filters to
code-only nodes (dropping documents/headings), then submits the graph to the control plane via
the internal API. The control plane owns the Neo4j write credentials and persists the graph
scoped to the repository's snapshot.

**Key files:**
- `services/agent-runner/src/indexer/graph.rs` — spawns Graphify, parses output, submits
- `services/control-plane/src/integrations/neo4j.rs` — Neo4j write connection

### Dispatcher

**What it is:**
A role of the control-plane binary (`CONTROL_PLANE_ROLE=dispatcher`). It polls a Postgres-backed
task queue and launches one **Kubernetes Job** per task. Each Job runs the agent-runner image.

**Why it exists:**
ADR-0004: Each task (review, index, ask) gets its own ephemeral K8s Job so resource usage is
isolated, the cluster reaps completed pods automatically, and the control plane stays stateless.

**Key files:**
- `services/control-plane/src/queue/dispatcher.rs` — queue consumer + Job launcher
- `services/control-plane/src/integrations/k8s.rs` — K8s Job manifest generation

### Reconciler

**What it is:**
A role of the control-plane binary (`CONTROL_PLANE_ROLE=reconciler`). It **owns all GitHub egress**:
it drains the `github_outbox` table and posts each queued intent (review, reply, reaction,
failure notice) to the GitHub API. Only runs as one replica so nothing is double-posted.

**Key files:**
- `services/control-plane/src/queue/reconciler.rs` — outbox consumer
- `services/control-plane/src/outbox.rs` — outbox producer (enqueue helpers)

### Review Agent

**What it is:**
A multi-turn chat loop that runs inside the agent-runner Job. It talks to a configurable LLM
(OpenAI-compatible chat endpoint via the eaig gateway) and offers a set of **mediated tools**:
- `lightbridge_vector_semantic_search` — query the pgvector index by meaning
- `lightbridge_graph_find_symbol` / `lightbridge_graph_get_callers` — query the Neo4j graph
- `read_file` — read source files from the checkout
- `add_review_comment` — buffer an inline finding (nothing posts until `finish`)
- `add_comment` — buffer a plain reply
- `finish` — finalize the review, flushing all buffered findings
- `abort` — abort gracefully

The agent acts *as it goes*: every `add_review_comment` is buffered control-plane-side but
**never posted until `finish`**. The control plane validates, scopes to the PR diff, and
shapes the final review (inline comments on changed lines, body notes for deferred findings,
collapsible section for out-of-scope observations).

**Two-tier review (ADR-0062):**
- **Fast** (automatic on `pull_request opened`): a single diff-only pass with no retrieval tools.
  Uses a cheap model, short timeout, and posts a "🅵 Fast automated pass" banner.
- **Deep** (triggered by `@mention`): full retrieval multi-turn review with the strong model.

**Key files:**
- `services/agent-runner/src/review/native/agent.rs` — the agent loop
- `services/agent-runner/src/review/native/tools.rs` — tool definitions and dispatcher
- `services/agent-runner/src/review/native/chat.rs` — LLM chat client with streaming, retry, backoff
- `services/control-plane/src/review.rs` — review validation + diff scoping + body rendering

### SAST (Static Application Security Testing)

**What it is:**
A deterministic first-pass scan using **opengrep**. It runs over the PR's changed files and
produces SARIF findings that are buffered into the *same* review buffer as the agent's findings.
A digest is fed to the agent so it doesn't redundantly re-report the same lines.

**Key files:**
- `services/agent-runner/src/sast/mod.rs` — opengrep invocation + SARIF parsing

---

## Project structure

```
.
├── apps/web                          # Next.js web console (Keycloak OIDC)
├── packages/                         # Shared TypeScript packages (Turborepo)
├── services/
│   ├── config/                       # Shared config loader crate (lightbridge-config)
│   ├── control-plane/                # Rust trust boundary (Axum HTTP server)
│   └── agent-runner/                 # Per-task K8s Job (clone → index → review)
├── xtask/                            # Cargo-xtask automation (from justfile)
├── docs/                             # Architecture docs + ADRs
│   ├── adr/                          # 67 Architecture Decision Records
│   ├── INDEX.md
│   ├── architecture.md
│   └── indexing-and-storage.md
├── deploy/                           # Helm charts (observability, etc.)
├── tools/                            # Code generation (dashboard-gen, etc.)
├── .github/workflows/                # CI/CD pipelines
└── compose.yaml                      # Local dev with pgvector + Neo4j
```

---

## Package dependencies (Rust workspace)

Defined in root `Cargo.toml` with workspace-level version pinning:

| Crate | Role | Dependencies |
|-------|------|-------------|
| `lightbridge-config` | Shared JSON config loader with `{env:VAR:-default}` substitution | serde, serde_json |
| `control-plane` | HTTP server + queue consumer + reconciler | axum, sqlx (pgvector), kube, neo4rs, jwt, metrics |
| `agent-runner` | Clone → index → SAST → review | reqwest, tree-sitter, futures |
| `xtask` | CI automation (fmt, clippy, test, schema validation) | anyhow |

---

## CI/CD workflows (`.github/workflows/`)

| Workflow | What it does |
|----------|-------------|
| `build-images.yml` | Build + publish 4 container images to GHCR (PR: build only, main: push + cosign sign) |
| `control-plane-tests.yml` | Run `cargo test -p control-plane` against a live `pgvector/pgvector:pg17` |
| `rust-lint.yml` | `cargo fmt --check` + `cargo clippy -D warnings` for the whole workspace |
| `dashboards.yml` | Verify Grafana dashboards are in sync with the generator |
| `governance.yml` | AI Governance PR compliance check (ADORSYS-GIS reusable workflow) |
| `opencode.yml` | opencode review for this repo (auto on PR, `/oc` on comment) |

---

## Key ADRs

| ADR | Topic |
|-----|-------|
| 0002 | Rust control plane is the trust boundary |
| 0003 | Dual retrieval: pgvector (semantic) + Neo4j (structural) |
| 0004 | One Kubernetes Job per task |
| 0010 | Tree-sitter syntax-aware chunking |
| 0014 | OIDC/JWT auth (Keycloak) |
| 0017 | Internal runner API (shared bearer) |
| 0018 | Embedding dimension safety (pgvector) |
| 0019 | Graphify for structural code graphs |
| 0026 | Native agent loop |
| 0032 | Finding priority model (P0/P1/P2) + shield.io badges |
| 0034 | Agent run transcript for observability |
| 0035 | Post-hoc feedback (👍/👎 reaction polling) |
| 0037 | Mediated write tools (buffer → finalize) |
| 0039 | Resilience: timeout, retry/backoff, circuit breaker, streaming |
| 0042 | Risk-first read batching (concurrent read-only tools) |
| 0043 | Refute pass (verify P0/P1 before posting) |
| 0045 | Context-window budgeting (token estimation) |
| 0058 | Reconciler owns all GitHub egress (single replica) |
| 0059 | Outbox pattern (producer/reconciler) |
| 0061 | SAST (opengrep) integration |
| 0062 | Two-tier review (fast auto + deep @mention) |

---

## Setup guide — connecting Lightbridge to a code-hosting platform

This section explains how to configure Lightbridge to work with a code-hosting platform, assuming
the **external services are already running somewhere**: a Postgres+pgvector instance, a Neo4j
instance, a Keycloak (OIDC) instance, a Kubernetes cluster, and an OpenAI-compatible LLM gateway
(for embeddings + chat completions). It covers **GitHub** and **GitLab** — both implemented behind
a `CodePlatform` trait (ADR-0071) with a unified `/webhook` route and a unified `outbox` table.

### External services you need running

Before configuring the platform integration, these services must be reachable:

| Service | Why | Connection info needed |
|---------|-----|------------------------|
| **PostgreSQL + pgvector** | Task queue, `outbox`, `code_chunks` (semantic store), `review_feedback`, delivery dedup | `DATABASE_URL` — e.g. `postgres://user:pass@host:5432/dbname` |
| **Neo4j 5+** | Structural code graph (symbols, call relationships) | `NEO4J_URI`, `NEO4J_USER`, `NEO4J_PASSWORD` — e.g. `bolt://host:7687`, `neo4j`, `pass` |
| **Keycloak** (or any OIDC provider) | Issues RS256 JWTs for the web console; control plane validates them | `OIDC_ISSUER`, `OIDC_AUDIENCE`, `OIDC_JWKS_URI` |
| **Kubernetes cluster** | Runs agent-runner Jobs (one per task) | Kubeconfig context + `AGENT_NAMESPACE` + `AGENT_SERVICE_ACCOUNT` |
| **LLM gateway** (OpenAI-compatible) | Embeddings for indexing + chat completions for review | `EMBEDDINGS_BASE_URL`, `EMBEDDINGS_API_KEY`, `EMBEDDINGS_MODEL` (required); `LLM_BASE_URL`, `LLM_API_KEY`, `LLM_MODEL` (optional — absent = indexing only) |
| **Container registry** (e.g. GHCR) | Stores the `control-plane` and `agent-runner` images | `AGENT_RUNNER_IMAGE` (or per-kind overrides) |

### Control-plane roles

The control plane is **one binary** that runs in three roles (selected by `CONTROL_PLANE_ROLE`):

| Role | Env var | Replicas | What it does |
|------|---------|----------|--------------|
| `serve` | `CONTROL_PLANE_ROLE=serve` (default) | 1 (see RFC-0001) | HTTP server: receives webhooks, serves the internal runner API, serves the web console API |
| `dispatcher` | `CONTROL_PLANE_ROLE=dispatcher` | 1 | Polls the task queue, launches one K8s Job per task, runs the reaper (Job GC + data purge) |
| `reconciler` | `CONTROL_PLANE_ROLE=reconciler` | 1 | Drains the `github_outbox` and posts all outbound content to the platform API |

All three roles need `DATABASE_URL`. Only `serve` needs the GitHub/GitLab credentials and the OIDC
config. Only `dispatcher` needs K8s cluster access. Only `reconciler` needs the platform API
credentials for posting (though in the current implementation, `serve` also reads via the App key).

---

### A. GitHub setup (fully implemented)

GitHub is the only code-hosting platform the codebase currently supports. Work enters
Lightbridge exclusively through GitHub App webhooks.

#### Step 1 — Create a GitHub App

Go to **GitHub → Settings → Developer settings → GitHub Apps → New GitHub App**.

**App configuration:**

| Field | Value | Notes |
|-------|-------|-------|
| GitHub App name | e.g. `lightbridge-assistant` | This becomes the `@mention` handle |
| Webhook URL | `https://<your-control-plane>/github/webhook` | Must be publicly reachable |
| Webhook secret | a random string | Goes into `GITHUB_WEBHOOK_SECRET` |

**Repository permissions:**

| Permission | Level | Why |
|------------|-------|-----|
| Metadata | Read | Basic repository context (required by GitHub) |
| Contents | Read | Clone/fetch for indexing and diff scoping |
| Pull requests | Read & Write | Read PRs; post reviews and inline comments |
| Issues | Read & Write | Read/write issue comments (the `@mention` answer path) |
| Checks | Read & Write | Optional check-run reporting |
| Commit statuses | Read & Write | Optional lightweight status reporting |

**Webhook event subscriptions:**

| Event | Why |
|-------|-----|
| `Pull request` | `opened` → automatic fast review; `closed` → cancel active tasks |
| `Push` | Push to default branch → re-index (keeps the base index fresh) |
| `Issue comment` | `@mention` → deep re-review or issue answer |
| `Installation` | `created` → register repos as pending; `deleted` → disable + purge |
| `Installation repositories` | repos added → pending; removed → disabled + purged |

After creating the App:
1. **Generate a private key** (PEM file) — goes into `GITHUB_APP_PRIVATE_KEY`
2. **Note the App ID** — goes into `GITHUB_APP_ID`
3. **Install the App** on the target repositories

#### Step 2 — Configure the control plane (serve role)

All env vars can be set directly or via a JSON config file at `/etc/lightbridge/control-plane.json`
(override path with `CONTROL_PLANE_CONFIG`). The config file supports `{env:VAR:-default}` substitution.

**Required env vars for `serve`:**

```bash
# --- Role ---
CONTROL_PLANE_ROLE=serve
BIND_ADDR=0.0.0.0:8080
METRICS_ADDR=0.0.0.0:9090

# --- Database (required for production) ---
DATABASE_URL=postgres://lightbridge:lightbridge@<pg-host>:5432/lightbridge

# --- Neo4j (structural graph) ---
NEO4J_URI=bolt://<neo4j-host>:7687
NEO4J_USER=neo4j
NEO4J_PASSWORD=<password>

# --- OIDC (Keycloak) ---
OIDC_ISSUER=https://<keycloak-host>/realms/lightbridge
OIDC_AUDIENCE=lightbridge-api
# OIDC_JWKS_URI is derived automatically: ${OIDC_ISSUER}/protocol/openid-connect/certs

# --- GitHub App ---
GITHUB_APP_ID=<app-id>
GITHUB_APP_PRIVATE_KEY="$(cat /path/to/app.pem)"
GITHUB_WEBHOOK_SECRET=<webhook-secret>
GITHUB_APP_HANDLE=<app-handle>           # default: lightbridge-assistant

# --- Internal runner API ---
AGENT_RUNNER_TOKEN=<shared-secret>       # shared bearer between control plane and agent-runner

# --- K8s launcher (used by dispatcher, but configured here) ---
AGENT_NAMESPACE=lightbridge-agents
AGENT_RUNNER_IMAGE=ghcr.io/vymalo/lightbridge-agent-runner:latest
AGENT_SERVICE_ACCOUNT=lightbridge-agent
CONTROL_PLANE_INTERNAL_URL=http://lightbridge-ci-control-plane:8080
AGENT_JOB_DEADLINE_SECONDS=3600
AGENT_CONFIG_CONFIGMAP=lightbridge-agent-config
```

**Optional env vars:**

```bash
PERMISSIONS_CLAIM=permissions            # JWT claim path for RBAC (default: permissions)
RECONCILER_INTERVAL_SECS=300             # reconciler poll interval
POLLER_INTERVAL_SECS=300                 # feedback poller interval
RECONCILER_WINDOW_DAYS=14               # feedback poll window
POLLER_WINDOW_DAYS=14
HOSTNAME=<pod-name>                      # used as dispatcher lease owner
```

#### Step 3 — Configure the dispatcher role

```bash
CONTROL_PLANE_ROLE=dispatcher
DATABASE_URL=postgres://lightbridge:lightbridge@<pg-host>:5432/lightbridge

# K8s launcher config (can also be in control-plane.json)
AGENT_NAMESPACE=lightbridge-agents
AGENT_RUNNER_IMAGE=ghcr.io/vymalo/lightbridge-agent-runner:latest
# Optional per-kind image overrides:
AGENT_INDEXER_RUNNER_IMAGE=ghcr.io/vymalo/lightbridge-agent-runner:latest-indexer
AGENT_REVIEW_RUNNER_IMAGE=ghcr.io/vymalo/lightbridge-agent-runner:latest-review
AGENT_SERVICE_ACCOUNT=lightbridge-agent
CONTROL_PLANE_INTERNAL_URL=http://lightbridge-ci-control-plane:8080
AGENT_JOB_DEADLINE_SECONDS=3600
AGENT_CONFIG_CONFIGMAP=lightbridge-agent-config
# Optional: internal CA for HTTPS to the LLM gateway
AGENT_CA_SECRET=lightbridge-internal-ca
```

The dispatcher does **not** need GitHub App credentials or OIDC — it only needs the database and K8s.

#### Step 4 — Configure the reconciler role

```bash
CONTROL_PLANE_ROLE=reconciler
DATABASE_URL=postgres://lightbridge:lightbridge@<pg-host>:5432/lightbridge

# GitHub App credentials (for posting reviews, replies, reactions)
GITHUB_APP_ID=<app-id>
GITHUB_APP_PRIVATE_KEY="$(cat /path/to/app.pem)"
GITHUB_APP_HANDLE=<app-handle>

# Feedback polling
RECONCILER_INTERVAL_SECS=300
RECONCILER_WINDOW_DAYS=14
```

The reconciler does **not** need OIDC or K8s access — it only needs the database and GitHub App
credentials.

#### Step 5 — Configure the agent-runner (per-Job env)

The dispatcher stamps these env vars into each K8s Job it launches. They come from the
`lightbridge-agent-secrets` Secret and the `lightbridge-agent-config` ConfigMap in the
`lightbridge-agents` namespace.

**Required for all tasks:**

```bash
TASK_ID=<uuid>                           # stamped by dispatcher
CONTROL_PLANE_URL=http://lightbridge-ci-control-plane:8080
AGENT_RUNNER_TOKEN=<shared-secret>       # same as control plane's AGENT_RUNNER_TOKEN
WORKDIR=/workspace                       # default clone directory

# Embeddings (REQUIRED — no defaults, fails loud if missing)
EMBEDDINGS_BASE_URL=https://<llm-gateway>/v1
EMBEDDINGS_API_KEY=<key>
EMBEDDINGS_MODEL=text-embedding-3-small
# Optional: EMBEDDINGS_CA_CERT=/etc/internal-ca/ca.crt
# Optional: EMBEDDINGS_REQUEST_TIMEOUT_SECS=30
```

**Required for review tasks (absent = indexing only):**

```bash
LLM_BASE_URL=https://<llm-gateway>/v1
LLM_API_KEY=<key>
LLM_MODEL=gpt-4o
LLM_STREAM=1                             # enable SSE streaming
LLM_REQUEST_TIMEOUT_SECS=180
LLM_MAX_RETRIES=3
LLM_CIRCUIT_BREAKER_THRESHOLD=5
LLM_MAX_TURNS=20
LLM_MAX_BATCH_SIZE=10
LLM_MAX_FILES_READ=50
LLM_MAX_SEARCHES=20
LLM_MAX_BATCHES=5
LLM_CONTEXT_WINDOW=128000
```

**Optional SAST:**

```bash
SAST_ENABLED=1
SAST_BIN=opengrep
SAST_RULES=/opt/opengrep-rules
SAST_MIN_SEVERITY=error
SAST_MAX_FINDINGS=25
SAST_TIMEOUT_SECS=300
```

**Config file (agent.json):**

Mounted from the ConfigMap named by `AGENT_CONFIG_CONFIGMAP` at `/etc/lightbridge/agent.json`
(override via `AGENT_CONFIG`). This file holds the structured config for embeddings, review
(two-tier: `fast` and `deep` sub-blocks), and SAST. It supports `{env:VAR:-default}` substitution.

#### Step 6 — JSON config files (optional but recommended)

**Control-plane config** (`/etc/lightbridge/control-plane.json`):

```json
{
  "agent": {
    "namespace": "lightbridge-agents",
    "runner_image": "ghcr.io/vymalo/lightbridge-agent-runner:latest",
    "indexer_runner_image": "{env:AGENT_INDEXER_RUNNER_IMAGE:-}",
    "review_runner_image": "{env:AGENT_REVIEW_RUNNER_IMAGE:-}",
    "service_account": "lightbridge-agent",
    "control_plane_url": "http://lightbridge-ci-control-plane:8080",
    "config_configmap": "lightbridge-agent-config",
    "job_deadline_seconds": 3600,
    "resources": { "requests": { "cpu": "500m", "memory": "1Gi" } },
    "indexer_resources": { "requests": { "cpu": "2000m", "memory": "4Gi" } },
    "review_resources": { "requests": { "cpu": "1000m", "memory": "2Gi" } }
  },
  "dispatcher": {
    "claim_lease_seconds": 60,
    "poll_fallback_seconds": 30,
    "launch_backoff_seconds": 5,
    "reap_interval_seconds": 60,
    "prune_interval_seconds": 300,
    "outbox_posted_retention_days": 7,
    "outbox_failed_retention_days": 30
  },
  "review": {
    "reactions": { "seen": "eyes", "upvote": "+1", "downvote": "-1" },
    "label_reviewed": "lightbridge-reviewed",
    "label_findings": "lightbridge-has-findings",
    "label_error": "lightbridge-error"
  },
  "embeddings": {
    "dimension": 1536,
    "allow_reindex_on_dim_change": false
  }
}
```

**Agent-runner config** (`/etc/lightbridge/agent.json`):

```json
{
  "embeddings": {
    "base_url": "{env:EMBEDDINGS_BASE_URL}",
    "api_key": "{env:EMBEDDINGS_API_KEY}",
    "model": "{env:EMBEDDINGS_MODEL}",
    "request_timeout_secs": 30
  },
  "review": {
    "fast": {
      "base_url": "{env:LLM_BASE_URL}",
      "api_key": "{env:LLM_API_KEY}",
      "model": "{env:LLM_FAST_MODEL:-gpt-4o-mini}",
      "system_prompt_file": "/etc/lightbridge/review-system-fast.md",
      "max_turns": 5,
      "max_diff_chars": 50000,
      "stream": true,
      "tools": ["add_review_comment", "finish", "abort"]
    },
    "deep": {
      "base_url": "{env:LLM_BASE_URL}",
      "api_key": "{env:LLM_API_KEY}",
      "model": "{env:LLM_MODEL:-gpt-4o}",
      "system_prompt_file": "/etc/lightbridge/review-system.md",
      "max_turns": 20,
      "max_diff_chars": 200000,
      "stream": true,
      "tools": [
        "vector_semantic_search", "graph_find_symbol", "graph_get_callers",
        "read_file", "add_review_comment", "retract_finding",
        "add_comment", "finish", "abort"
      ]
    }
  },
  "sast": {
    "enabled": true,
    "bin": "opengrep",
    "rules": "/opt/opengrep-rules",
    "min_severity": "error",
    "max_findings": 25,
    "timeout_secs": 300
  }
}
```

#### Step 7 — Keycloak realm

The control plane is a pure OAuth2 **resource server**: it validates RS256 JWTs against the
Keycloak JWKS endpoint. The web app (`apps/web`) is an OIDC client (public/PKCE).

Keycloak realm configuration:
- **Realm name**: `lightbridge`
- **Client**: `lightbridge-web` (public, PKCE, redirect URIs for the web app)
- **API audience**: `lightbridge-api` (configured as an audience in the realm)
- **Dev user** (local only): `dev` / `password`
- **Permissions claim**: the JWT should include a `permissions` claim for RBAC
  (`task:read`, `repo:approve`, `task:cancel`, etc. — ADR-0023)

The control plane reads:
- `OIDC_ISSUER` — e.g. `https://<keycloak>/realms/lightbridge`
- `OIDC_AUDIENCE` — `lightbridge-api` (default: `account`)
- `OIDC_JWKS_URI` — defaults to `${OIDC_ISSUER}/protocol/openid-connect/certs`

#### Step 8 — Deploy order (the 3-repo dance)

Because config files use `deny_unknown_fields`, a new config field must land in this order:

1. **Image** (this repo) — ship the binary that understands the new field
2. **Chart** (ai-helm) — render the field into manifests
3. **Values** (ai-helm-values) — actually set the field

Reversing this causes a crash-loop (the binary rejects unknown fields).

#### Step 9 — Install and approve

1. **Install the GitHub App** on target repositories
2. Repos appear as **pending** in the web console (approval gate, Epic #75)
3. An admin **approves** each repo (requires `repo:approve` permission)
4. Approval enqueues an **index** task (builds semantic + structural index)
5. Open a PR → automatic **fast** review
6. `@mention` in a PR or issue → **deep** review or answer

---

### B. GitLab setup (implemented — ADR-0071)

GitLab support is built alongside GitHub behind a **`CodePlatform` trait**
(`services/control-plane/src/integrations/platform.rs`). Both platforms share a unified
`/webhook` route (with platform auto-detected from headers) and a unified `outbox` table (with a
`platform` column). The agent-runner is entirely untouched — the control plane hands it the right
`clone_url` via the trait.

GitLab and GitHub have analogous concepts, but the APIs, auth models, and webhook payloads differ:

#### GitLab ↔ GitHub concept mapping

| GitHub | GitLab | Notes |
|--------|-------|-------|
| Pull request | Merge request | MR webhooks: `Merge Request Hook` |
| `issue_comment` | Note on MR / Issue | GitLab `Note Hook` — `noteable_type` distinguishes MR vs Issue |
| `push` (default branch) | `push` to default branch | GitLab `Push Hook` |
| `installation` / `installation_repositories` | (no equivalent) | GitLab repos are registered manually via the admin console (same approval gate as GitHub, ADR-0063) |
| GitHub App (JWT → installation token) | Bot account + access token | GitLab has no App equivalent; use a project/group access token |
| Installation token (short-lived) | Project/group access token (rotatable) | GitLab tokens have expiry dates; no per-request minting |
| `X-Hub-Signature-256` (HMAC-SHA256) | `X-Gitlab-Token` (shared secret comparison) | GitLab sends the plain webhook secret in a header — no HMAC |
| `X-GitHub-Delivery` (delivery ID) | `X-Gitlab-Event-UUID` | Used for dedup |
| `X-GitHub-Event` (event type) | `X-Gitlab-Event` | `Merge Request Hook` / `Push Hook` / `Note Hook` |
| `github_outbox` | `outbox` (with `platform` column) | Unified table — one reconciler drains both platforms |
| `github_repo_id` | `platform_repo_id` | Composite unique on `(platform, platform_repo_id)` |

#### What you need to configure for GitLab

**Step 1 — Create a GitLab bot user + access token:**
- Create a dedicated bot account in GitLab (or use a project/group access token)
- Grant it **Developer** or **Maintainer** role on target projects (needed to post notes on MRs)
- Create a **project access token** or **group access token** with scopes: `api` (read/write MRs,
  notes, discussions), `read_repository` (clone for indexing)
- Note the token — this goes into the `GITLAB_API_TOKEN` env var

**Step 2 — Configure webhooks in GitLab (per project):**
- **Webhook URL**: `https://<your-control-plane>/webhook` (the unified route — platform is
  auto-detected from the `X-Gitlab-Event` header)
- **Secret token**: a shared secret for `X-Gitlab-Token` verification
- **Trigger events**: Merge request events, Push events, Note events
- (Group-level webhooks are a Premium+ feature; per-project webhooks are sufficient initially)

**Step 3 — Control-plane env vars:**

```bash
# --- GitLab ---
GITLAB_API_URL=https://gitlab.example.com/api/v4   # or https://gitlab.com/api/v4
GITLAB_API_TOKEN=<bot-access-token>
GITLAB_WEBHOOK_SECRET=<shared-secret>
GITLAB_BOT_HANDLE=<bot-username>                   # default: lightbridge-bot
```

**Step 4 — How the webhook is handled (implemented):**

The unified `webhook_router` (`http/webhook.rs`) detects the platform from headers
(`x-gitlab-event` → GitLab), verifies the `X-Gitlab-Token` against `GITLAB_WEBHOOK_SECRET`
(constant-time plain comparison), dedups on `X-Gitlab-Event-UUID`, and routes:
- `Merge Request Hook` → `handle_gitlab_merge_request` — `open` → approval gate → draft skip →
  bot detection → fast-tier review task (SHAs from `diff_refs` in the payload); `close` → cancel
- `Push Hook` → `handle_gitlab_push` — default branch only → approval gate → index task
- `Note Hook` → `handle_gitlab_note` — `@mention` detection (using `GITLAB_BOT_HANDLE`) →
  approval gate → fetches MR SHAs via the trait → deep-tier task

**Step 5 — Integration module (implemented):**

`services/control-plane/src/integrations/gitlab.rs` (`GitlabClient`) implements `CodePlatform`:
- `list_changed_files` — `GET /projects/:id/merge_requests/:iid/changes`
- `default_branch` — `GET /projects/:id`
- `pr_shas` — `GET /projects/:id/merge_requests/:iid` → `diff_refs.base_sha` / `diff_refs.head_sha`
- `post_review` — fetches MR `diff_refs`, posts each inline as a discussion thread with a
  `position` object (`base_sha`/`head_sha`/`start_sha`/`position_type`/`new_path`/`old_path`/
  `new_line`), posts the body as an MR note, returns the note ID
- `post_comment` — tries MR notes first, falls back to issue notes (GitLab uses different
  endpoints per noteable type)
- `add_reaction` — award emoji on MR/Issue (via `ReactionTarget::Issue`); comment reactions are a
  known limitation (requires MR/issue `iid` lookup from note ID)
- `add_labels` — `PUT /projects/:id/merge_requests/:iid` with `labels` param
- `list_review_comments` — `GET /projects/:id/merge_requests/:iid/discussions`, filters to
  `DiffNote` type
- `list_comment_reactions` — returns empty Vec (known limitation: 👍/👎 feedback polling requires
  MR/issue `iid` from note ID; reviews/comments/labels all work correctly)
- `clone_url` — `https://oauth2:{token}@{host}/{full_name}.git` (embeds the token so the
  agent-runner can clone without any platform credentials)

**Step 6 — Reconciler outbox (implemented):**

The unified `outbox` table (renamed from `github_outbox` in migration `0024`) carries a
`platform` column. The reconciler (`queue/reconciler.rs`) takes a
`HashMap<Platform, Arc<dyn CodePlatform>>` and dispatches per row — GitHub rows go to
`GithubApp`, GitLab rows go to `GitlabClient`. One writer, one loop, both platforms
(ADR-0059 preserved).

#### Known limitations (GitLab)

- **Comment-reaction feedback polling** (`list_comment_reactions`) returns an empty Vec — the
  GitLab award-emoji endpoint needs the MR/issue `iid` to reach a note, and the outbox only
  stores the note `id`. Reviews, comments, labels, and issue/MR-body reactions all post correctly.
  This is a Phase-7 follow-up, not a correctness risk.
- **`post_comment` probes MR then issue** — GitLab uses different endpoints per noteable type and
  the outbox only carries the `issue_number`. The probe costs one 404 on the wrong type; acceptable
  for a reply/failure-notice path that fires once per task.
- **Bot detection** is less precise than GitHub (GitLab has no `type: "Bot"` field) — checks
  author name suffix patterns (`_bot`/`-bot`/`GitLab`), fails open.
- **No installation/system hooks** — GitLab repos are registered manually via the admin console
  (same approval gate as GitHub, ADR-0063).

#### What does NOT change (platform-agnostic)

- The **agent-runner** is platform-agnostic — it clones a repo, builds an index, runs SAST, and
  runs the review agent. It talks only to the control plane's internal API. It does not know or
  care whether the repo is hosted on GitHub or GitLab.
- The **indexer** (tree-sitter, embeddings, Graphify) is platform-agnostic.
- The **review agent** is platform-agnostic — it uses mediated tools (`vector_semantic_search`,
  `graph_find_symbol`, `read_file`, `add_review_comment`) that are abstracted from the platform.
- The **config crate**, **xtask**, and **CI/CD workflows** are platform-agnostic.
- The **Keycloak OIDC** setup is identical.
- The **K8s Job-per-task** model is identical.

#### Summary: what exists (both platforms implemented)

| Component | GitHub | GitLab |
|-----------|--------|--------|
| Webhook receiver | `POST /webhook` (unified) + `/github/webhook` (legacy alias) | `POST /webhook` (unified) |
| API integration | `integrations/github.rs` — `impl CodePlatform for GithubApp` | `integrations/gitlab.rs` — `impl CodePlatform for GitlabClient` |
| Auth | GitHub App (JWT → installation token) | Bot account + access token (`PRIVATE-TOKEN` header) |
| Webhook signature | HMAC-SHA256 (`X-Hub-Signature-256`) | Shared secret (`X-Gitlab-Token`, constant-time compare) |
| Dedup key | `X-GitHub-Delivery` | `X-Gitlab-Event-UUID` |
| Outbox | `outbox` table, `platform = 'github'` | `outbox` table, `platform = 'gitlab'` |
| Event → task mapping | `handle_pull_request`, `handle_push`, `handle_issue_comment` | `handle_gitlab_merge_request`, `handle_gitlab_push`, `handle_gitlab_note` |
| Bot handle | `GITHUB_APP_HANDLE` (default `lightbridge-assistant`) | `GITLAB_BOT_HANDLE` (default `lightbridge-bot`) |
| Agent runner | Platform-agnostic — no changes | No changes needed |
| Indexer | Platform-agnostic — no changes | No changes needed |
| Review agent | Platform-agnostic — no changes | No changes needed |
