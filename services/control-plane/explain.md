# Control Plane — `services/control-plane/`

## What is it?

The **trust boundary** of the entire Lightbridge system. It is a standalone Axum HTTP server
(one binary, three roles) that:

1. Receives and verifies GitHub webhooks (`serve` role)
2. Dispatches one Kubernetes Job per task (`dispatcher` role)
3. Owns all GitHub egress via a single-replica outbox reconciler (`reconciler` role)

It is the **only component that holds the GitHub App private key** and the **only component that
writes to GitHub**. The agent-runner Jobs are untrusted and talk back to the control plane via
a shared bearer token.

---

## Binary roles

The same binary runs as three separate Deployments, selected by `CONTROL_PLANE_ROLE`:

| Role | What it does | State | Replicas |
|------|-------------|-------|----------|
| `serve` | HTTP surface: webhooks, task CRUD, OIDC-protected admin, metrics | Reads/writes DB | N |
| `dispatcher` | Claims tasks from the queue, creates K8s Jobs | Reads/writes DB | N |
| `reconciler` | Drains `github_outbox`, posts to GitHub, polls reactions | Reads/writes DB | **1** |

---

## Module map

```
src/
├── main.rs           # Entry point: role dispatch, AppState, routing, readiness
├── config.rs         # File config loader (review feedback, embeddings dimension)
├── db.rs             # Postgres pool + migrations + pgvector dimension safety
├── jwt.rs            # OIDC JWT validation (Keycloak)
├── outbox.rs         # GitHub-egress outbox: enqueue_review/reply/reaction/failure_notice
├── review.rs         # Review validation + diff scoping + body rendering (badges, disclosure)
├── types.rs          # Shared domain types (Finding, Task, etc.)
│
├── http/             # HTTP handlers (serve role)
│   ├── mod.rs        #  └─ submodule re-exports
│   ├── webhook.rs    #     POST /github/webhook — HMAC-verified + delivery-id dedup
│   ├── internal.rs   #     /internal/tasks/{id}/* — runner bootstrap + results API
│   ├── admin.rs      #     /admin/* — approval gate (admin realm role)
│   └── metrics.rs    #     /metrics — Prometheus exposition
│
├── queue/            # Queue & background loops
│   ├── mod.rs        #  └─ submodule re-exports
│   ├── dispatcher.rs #     Claim + launch one K8s Job per task
│   ├── tasks.rs      #     Task queue persistence + CRUD
│   ├── lifecycle.rs  #     Task state machine (pending→running→succeeded/failed/cancelled)
│   ├── reaper.rs     #     Job GC + data-purge reconciler
│   ├── reconciler.rs #     Outbox consumer + reaction poller
│   ├── outbox_sweeper.rs # Stale-outbox cleanup
│   └── index_sweeper.rs # Stale-index cleanup
│
└── integrations/     # External system clients (control plane owns credentials)
    ├── mod.rs        #  └─ submodule re-exports
    ├── github.rs     #     GitHub App auth + token mint + API client
    ├── k8s.rs        #     K8s Job manifest generation (for agent-runner pods)
    └── neo4j.rs      #     Neo4j Bolt client for structural graph writes
```

---

## AI/ML terms explained

### Embedding dimension safety (`db.rs`, `config.rs`)

The control plane enforces that the pgvector `embedding` column's dimension matches the
configured embeddings model at startup. If the dimension changes (e.g. 1536→4096), it either
fails loudly or triggers a reindex-from-scratch — **never silently mismatching**. This is
critical because queries embed with one dimension and search in another would be mathematically
nonsensical.

### Review validation and diff scoping (`review.rs`)

The agent runner submits findings as `(file, line, title, priority, category, body)`. The
control plane validates them against the PR's unified diff:

- **Inline**: finding on a changed, commentable line → posted as a GitHub PR review comment
- **Deferred**: finding on a changed file but an unpinnable line → folded into the review body
- **Out of scope**: finding on a file the PR doesn't touch → counted in a collapsible section,
  **never silently dropped** (ADR-0033 "no silent drops")

The review body is rendered with:
- **Priority badges**: `![P0](https://img.shields.io/badge/P0-red)` — colour-coded shields.io
  images (P0=red, P1=orange, P2=lightgrey; security always red)
- **Evidence** field (Phase 2): cited lines buried in the finding so the claim is verifiable
- **Strip model artifacts**: removes `<think>...</think>`, deepseek `<|...|>` tokens before
  text reaches GitHub
- **AI Governance disclosure**: "_AI-generated review — treat it as untrusted, verify before
  acting_" appended to every review

### Two-tier review (ADR-0062)

Configured through `review.fast` and `review.deep` blocks in the agent config file. Each tier
has its own:
- Model / API endpoint / API key
- System prompt (persona + focus)
- Max turns, timeout, retry policy
- Tool allowlist (which tools the model may call)

The FAST tier produces a **"🅵 Fast automated pass"** banner with a pointer to mention the
GitHub App handle for a deeper review. The body is composed control-plane-side (the only place
that knows the real App handle).

### Outbox pattern (ADR-0059)

**What it is:** Every outbound GitHub *content* write (review, reply, reaction, failure notice)
is shaped at produce time into a JSON payload and enqueued into the `github_outbox` table.
The reconciler is the sole consumer — it drains the outbox and posts each intent to the GitHub
API. This means:

- The reconciler never parses diffs or renders bodies (baked at produce time)
- Dedup is by `dedup_key` — re-finalizing is idempotent
- Single-replica: no double-posting even on retry
- All egress is observable from the outbox table

**Key functions:** `enqueue_review`, `enqueue_reply`, `enqueue_reaction`, `enqueue_failure_notice`
in `outbox.rs`.

### Mediated write tools (ADR-0037)

The review agent runs inside the untrusted Job. It **buffers** findings/replies by calling
the control plane's internal API:
- `POST /internal/tasks/{id}/review/inline` — buffer one finding
- `POST /internal/tasks/{id}/review/comment` — buffer a plain reply
- `POST /internal/tasks/{id}/review/summary` — set the verdict
- `POST /internal/tasks/{id}/review/finalize` — flush the buffer as one grouped review

Nothing reaches GitHub until `finalize`. This lets the control plane validate, scope to the
diff, deduplicate, and shape the final review before anything is posted.

---

## Internal runner API (`/internal/tasks/{id}/*`)

All routes authenticated with the shared `AGENT_RUNNER_TOKEN` bearer (injected into each Job):

| Route | Method | Purpose |
|-------|--------|---------|
| `/internal/tasks/{id}` | GET | Fetch task context + short-lived installation token |
| `/internal/tasks/{id}/status` | GET/POST | Read task status / report status transition |
| `/internal/tasks/{id}/chunks` | POST | Submit a batch of indexed code chunks (16 MB limit) |
| `/internal/tasks/{id}/graph` | POST | Submit structural graph nodes+edges (32 MB limit) |
| `/internal/tasks/{id}/search` | POST | Semantic search over the repo's pgvector index |
| `/internal/tasks/{id}/graph/query` | POST | Structural queries (find_symbol, get_callers) |
| `/internal/tasks/{id}/transcript` | POST | Submit the agent run transcript |
| `/internal/tasks/{id}/review/*` | POST | Mediated write actions (inline, comment, summary, finalize) |

---

## Key dependencies

| Crate | Purpose |
|-------|---------|
| `axum 0.8` | HTTP framework |
| `sqlx 0.8` | Postgres with pgvector (`tls-rustls`, `runtime-tokio`) |
| `kube 0.99` | K8s API (Job lifecycle) |
| `neo4rs 0.8` | Neo4j Bolt driver (structural graph) |
| `jsonwebtoken` | OIDC access token validation |
| `metrics` + `metrics-exporter-prometheus` | Prometheus `/metrics` |
| `lightbridge-config` | Shared JSON config loader |
