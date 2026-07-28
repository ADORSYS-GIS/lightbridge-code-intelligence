# Components and data models

This document describes the **control-plane modules** and the **data models** they own: the
Postgres schema (the system of record), the pgvector chunk table, and the Neo4j structural graph.
It is grounded in `services/control-plane/src/` and `services/control-plane/migrations/` as they
exist today.

The control plane is the trust boundary ([ADR-0002](adr/0002-rust-control-plane-trust-boundary.md)):
it verifies webhooks, owns task state and idempotency, mediates every agent write, and is the **sole
GitHub egress**. The agent runner (indexer + native review agent) does deep repo reasoning but holds
no durable write authority.

## Component responsibilities

| Component | Responsibilities | Does not own |
|---|---|---|
| GitHub App | Identity, permissions, webhooks | Business logic |
| Rust control plane | Webhook verification, task creation, idempotency, policy/approval gate, mediated writes, GitHub egress | Deep repo reasoning |
| Agent runner (indexer) | Clone repo, parse code, build Neo4j graph + pgvector chunks | GitHub write actions |
| Agent runner (native review agent) | Investigation + review reasoning, SAST, finding emission via mediated tools | Trust decisions, durable writes |
| Postgres | Source-of-truth metadata, task queue, persisted reviews/feedback, GitHub outbox | Semantic retrieval |
| Neo4j | Structural code graph | System-of-record task state |
| pgvector (`code_chunks`) | Chunk embeddings + exact cosine search | Code topology |

The native review agent is an **in-process Rust loop**
([ADR-0026](adr/0026-native-review-agent.md)) that acts only through **mediated write tools**
routed back to the control plane ([ADR-0037](adr/0037-agent-acts-via-mediated-tools.md)). There is
no OpenCode/ACP/MCP-subprocess agent and no fallback model: a single model with
retry/backoff/circuit-breaker ([ADR-0039](adr/0039-agent-llm-resilience-and-observability.md),
[ADR-0053](adr/0053-remove-review-fallback-model.md)).

## Control-plane module layout

One binary runs in several **roles** (RFC-0001), selected by the first CLI arg or
`CONTROL_PLANE_ROLE` (`src/main.rs`): `serve`, `dispatcher`, and `reconciler` (with `poller` kept as
a legacy alias during the [ADR-0058](adr/0058-rename-poller-role-to-reconciler.md) rename).

| Module | Role | Responsibility |
|---|---|---|
| `src/main.rs` | all | Role selection, `AppState`, axum wiring, OIDC resource-server middleware |
| `src/http/webhook.rs` | serve | GitHub webhook receiver: verify `X-Hub-Signature-256`, dedupe on delivery id, create tasks |
| `src/http/internal.rs` | serve | Runner↔control-plane contract ([ADR-0017](adr/0017-agent-runner-control-plane-bootstrap.md)): status reports, mediated review tools, retrieval |
| `src/http/admin.rs` | serve | Approval gate + admin console API (Epic #75) |
| `src/http/metrics.rs` | all | Prometheus `/metrics` renderer |
| `src/queue/dispatcher.rs` | dispatcher | Claim queued tasks (`FOR UPDATE SKIP LOCKED`), launch one k8s Job per task ([ADR-0004](adr/0004-one-k8s-job-per-task.md)) |
| `src/queue/reaper.rs` | dispatcher | Reap stuck `running`/`posting_result` tasks whose lease expired |
| `src/queue/index_sweeper.rs` | dispatcher | Snapshot pruning ([ADR-0052](adr/0052-index-snapshot-pruning.md)) |
| `src/queue/lifecycle.rs` | dispatcher | Purge index data when a repo is removed/denied (Epic #75 Milestone B) |
| `src/queue/reconciler.rs` | reconciler | Drain `github_outbox` (the sole egress) + poll reactions into `review_feedback` |
| `src/queue/outbox_sweeper.rs` | reconciler | Prune drained `github_outbox` rows |
| `src/outbox.rs` | producers | Producer side of [ADR-0059](adr/0059-reconciler-owns-all-github-egress.md): shape + `enqueue_*` outbox intents |
| `src/review.rs` | serve | Review validation + write-back shaping (verification/refute, coverage gate) |
| `src/integrations/github.rs` | serve / reconciler | GitHub App auth (App key for **reads** on serve; writes only via reconciler) |
| `src/integrations/neo4j.rs` | — | Bolt persistence for the structural graph ([ADR-0019](adr/0019-graphify-cli-structural-graph.md)) |
| `src/integrations/k8s.rs` | dispatcher | Job creation/deletion |
| `src/db.rs` | all | All SQL: queue, tasks, repositories, reviews, feedback, outbox, code chunks |
| `src/config.rs` | all | File config (`control-plane.json`), `deny_unknown_fields` |
| `src/types.rs` | all | Core domain enums/structs mirroring the schema |

## Schema-first / SQLx approach

Persistence is **schema-first** ([ADR-0005](adr/0005-cratestack-schema-first-control-plane.md)): the
canonical schema lives in `services/control-plane/schema/control-plane.cstack`, but cratestack
codegen is deferred, so the live schema is the hand-written, append-only SQL migrations under
`services/control-plane/migrations/`, applied on startup via `sqlx::migrate!`. **Migrations are
checksum-verified — never edit an applied migration; add a new numbered one.** Hand-written types in
`src/types.rs` and the `sqlx::FromRow` structs in `src/db.rs` mirror the schema in the meantime.

## State enums

From `src/types.rs`:

```rust
pub enum RepoIndexStatus { Pending, Running, Ready, Failed, Stale, Disabled }

pub enum TaskStatus {
    Received, WaitingForIndex, Queued, Running, PostingResult,
    Succeeded, Failed, TimedOut, Cancelled,
}
```

`status` is stored as TEXT (snake_case); the dispatcher dequeues `queued`, the reaper scans
`running`/`posting_result`.

## Postgres schema

### `repositories` — known repos + approval gate

```sql
CREATE TABLE repositories (
    id              BIGSERIAL PRIMARY KEY,
    github_repo_id  BIGINT  NOT NULL UNIQUE,
    owner           TEXT    NOT NULL,
    name            TEXT    NOT NULL,
    default_branch  TEXT    NOT NULL,
    active          BOOLEAN NOT NULL DEFAULT TRUE,
    -- migration 0007 (Epic #75): approval gate
    status          TEXT NOT NULL DEFAULT 'pending'
                        CHECK (status IN ('pending','approved','disabled')),
    approved_at     timestamptz,
    approved_by     TEXT,
    -- migration 0008: needed to mint an installation token for index-on-approve
    installation_id BIGINT
);
```

A repo must be `status = 'approved'` before any task runs (`0007_repo_approval.sql`); rows existing
before the gate were grandfathered to `approved`. Approving a repo enqueues a standalone `index`
task on the default branch.

### `github_deliveries` — webhook idempotency

```sql
CREATE TABLE github_deliveries (
    delivery_id     TEXT PRIMARY KEY,   -- X-GitHub-Delivery: natural idempotency key
    event_name      TEXT        NOT NULL,
    installation_id BIGINT,
    repository_id   BIGINT,
    received_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    payload_json    JSONB       NOT NULL
);
```

The PK + `ON CONFLICT` gives exactly-once delivery handling with no in-process dedup set.

### `repo_index` — index snapshot bookkeeping (reserved, no writer yet)

```sql
CREATE TABLE repo_index (
    id, repository_id, branch, commit_sha,
    graph_version, vector_version, status,
    started_at, completed_at,
    UNIQUE (repository_id, branch, commit_sha, graph_version, vector_version)
);
```

Confirmed by code search (#245): no `INSERT`/`UPDATE` exists anywhere in `services/control-plane/src`.
Snapshot readiness today is tracked purely via `code_chunks` / `latest_indexed_commit`
([ADR-0050](adr/0050-retrieval-pins-to-latest-indexed-snapshot.md)). The only live references are
defensive purge cleanup — `delete_repo_index_rows` and the `EXISTS` leftover-data check in
[`db/code_chunks.rs`](../services/control-plane/src/db/code_chunks.rs). The table is intentionally
kept, not dead: [ADR-0055](adr/0055-review-waits-for-index-readiness.md)'s "Failed/partial index"
consequence names `status`/`completed_at` here as the fix for gating `latest_indexed_commit` on a
`ready` row per commit (closing the reuse-a-failed-index and mid-review-reindex-race gaps), tied to
[RFC-0002](rfc/0002-incremental-layered-indexing.md)'s layered-indexing model.

### `tasks` — system of record + work queue

The central table. Created by `0001_init.sql`, then extended by several migrations into a
Postgres-backed work queue (RFC-0001) and the carrier of run kind/preset.

| Column | Source | Notes |
|---|---|---|
| `id` UUID PK | 0001 | |
| `repository_id`, `installation_id` | 0001 | FK to `repositories` |
| `github_delivery_id` | 0001 / 0008 | FK; **nullable since 0008** (admin-initiated index tasks have no webhook) |
| `target_type`, `target_id` | 0001 | e.g. `pull_request` + PR number, or `repository` |
| `command_text` | 0001 | normalized inbound command (`review this`, `index`, …) |
| `base_sha`, `head_sha` | 0001 | PR diff endpoints; `head_sha` participates in idempotency |
| `status` | 0001 | TEXT, mirrors `TaskStatus` |
| `priority` | 0001 | default 100; dispatch order |
| `created_at`, `started_at`, `completed_at` | 0001 | run timing for the dashboard |
| `attempts`, `run_after`, `run_epoch`, `lease_owner`, `lease_expires_at`, `job_name` | 0003 | queue mechanics + lease + the k8s Job name |
| `kind` | 0011 | [ADR-0033](adr/0033-inbound-command-parsing-and-run-kinds.md): `review` (diff-scoped) or `ask` (conversational); default `review` |
| `error_detail` | 0016 | runner's free-text failure/no-op reason; `NULL` = clean success ([ADR-0056](adr/0056-control-plane-owns-the-posted-output.md)) |
| `preset` | 0021 / 0033 (column added as `tier` by 0021, renamed to `preset` by 0033) | [ADR-0103](adr/0103-repo-configurable-opencode-review-presets.md): the resolved review preset name (`fast`/`deep` by platform default, or any operator-defined name); default `deep` |
| `entry_point` | 0033 | [ADR-0103](adr/0103-repo-configurable-opencode-review-presets.md): which trigger created the task — `pr_open` / `mention` / `a2a`; default `mention` |

**Preset resolution, per entry point** ([ADR-0103](adr/0103-repo-configurable-opencode-review-presets.md),
superseding the old fixed [ADR-0062](adr/0062-two-tier-review-fast-auto-deep-on-demand.md) split): the
webhook resolves `preset` per trigger from the repo's `.lightbridge-code-review.jsonc`
([ADR-0030](adr/0030-repo-review-config.md)), falling back to a platform-default mapping when the repo
declares nothing — `pull_request opened` (`entry_point = pr_open`) → `fast` by default (cheap model,
SAST + a lean diff-only LLM pass, no retrieval, a small tool allowlist
`review.presets.fast.tools`, a small `max_turns`); `@mention` (`entry_point = mention`) → `deep` by
default (strong model, full retrieval, multi-turn, long timeout). `index` tasks ignore `preset`. The
model is operator-tuned per preset in `ai-helm-values` and **churns — never assume a specific model
name**. `preset` and `entry_point` are kept as separate columns because a preset name is now
operator-defined and can't reliably signal which trigger created the task.

**Idempotency** (`0003_task_queue.sql`):

```sql
CREATE UNIQUE INDEX tasks_idempotency_idx
    ON tasks (repository_id, target_type, target_id, command_text, head_sha, run_epoch)
    NULLS NOT DISTINCT;   -- a NULL head_sha cannot bypass the guard
```

`run_epoch` lets an explicit re-run create a fresh version without colliding; `create_explicit_task`
computes `MAX(run_epoch)+1` (`src/db.rs`). Supporting indexes: `tasks_queue_idx` (partial, `WHERE
status='queued'`), `tasks_reapable_idx` (partial, `WHERE status IN ('running','posting_result')`),
`tasks_github_delivery_id_idx`.

### `reviews` — persisted review output (one row per task)

```sql
CREATE TABLE reviews (
    task_id            uuid PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
    summary            text NOT NULL,
    body               text NOT NULL,         -- the rendered body as posted
    inline_count       int  NOT NULL DEFAULT 0,
    deferred_count     int  NOT NULL DEFAULT 0,
    out_of_scope_count int  NOT NULL DEFAULT 0,
    findings           jsonb NOT NULL DEFAULT '[]',  -- verbatim findings array
    created_at         timestamptz NOT NULL DEFAULT now(),
    review_url         text,        -- 0010: permalink to the posted review
    github_review_id   bigint       -- 0013: correlate feedback back to the run (ADR-0035)
);
```

`findings` carries each finding's `file`/`line`/`severity`/`title`/`body`; priority/category follow
[ADR-0032](adr/0032-review-finding-priority-and-category.md). It is later joined against rejected
reactions for feedback memory (see below).

### `pending_review_actions` — mediated write buffer (ADR-0037)

The agent's mediated tools (`add_review_comment` / `add_comment` / `set_summary`) accumulate here
**during** a run; the control plane flushes once on clean completion, so a mid-run crash posts
nothing.

```sql
CREATE TABLE pending_review_actions (
    id BIGSERIAL PRIMARY KEY,
    task_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    action TEXT NOT NULL CHECK (action IN ('inline','comment','summary')),
    file, line, title, priority, category, suggestion,  -- inline only
    body TEXT NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    CHECK (action <> 'inline' OR (file IS NOT NULL AND line IS NOT NULL))
);
```

Dedup: inline findings are unique per `(task_id, file, line)` (last write wins, **not** a content
hash — an LLM re-run rewords the same finding); the summary is single-valued per task; plain comments
are append-only and consolidated into one reply at flush. The whole buffer is cleared when a runner
(re)starts, so a retry begins empty. On a PR posting a review, buffered `add_comment` replies are
dropped — single PR output channel ([ADR-0056](adr/0056-control-plane-owns-the-posted-output.md)).

### `agent_transcript` — **retired** (ADR-0100)

The per-run `agent_transcript` table (ADR-0034: tool calls, reasoning, per-turn token usage, submitted
once at end of run over `POST /internal/tasks/{id}/transcript` and served over
`GET /tasks/{id}/transcript`) was **torn out** in
[ADR-0100](adr/0100-retire-db-transcript-logs-as-observability.md) (epic #459, ticket #461). Migration
`0032_drop_agent_transcript.sql` `DROP`s it (forward-only; irreversible in prod). Run observability is
now **Loki-only** — the per-turn proof-of-work log lines are the single source of run legibility; there
is no DB transcript and no `/transcript` endpoint.

### Feedback signal + memory: `review_comments` / `review_feedback` (ADR-0035, ADR-0044)

GitHub does **not** webhook reactions, so the reconciler periodically reads the reactions REST API
for the comments we own and reconciles them.

```sql
CREATE TABLE review_comments (         -- the comment ids we created at write-back
    id UUID PRIMARY KEY, task_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    github_comment_id BIGINT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('inline','reply','failure_notice')),  -- 0019 added failure_notice
    file TEXT, line INT,               -- inline only, to correlate to a finding
    UNIQUE (kind, github_comment_id)
);

CREATE TABLE review_feedback (         -- one reaction on one of our comments
    id UUID PRIMARY KEY, task_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    github_comment_id BIGINT NOT NULL,
    comment_kind TEXT NOT NULL,        -- 'inline' | 'reply'
    reactor TEXT NOT NULL,             -- GitHub login
    reaction TEXT NOT NULL,            -- '+1' | '-1' | 'heart' | ...
    UNIQUE (github_comment_id, comment_kind, reactor, reaction)
);
```

A new reaction is inserted; one that disappeared (un-react) is deleted on the next reconcile — that
is how the "deleted" case is observed without a webhook. **Feedback memory M1**
([ADR-0044](adr/0044-feedback-memory-m1.md)): `rejected_findings_for_repo` (`src/db.rs`) joins
`review_feedback` (`reaction = '-1'`) → `review_comments (file, line)` → the matching entry in
`reviews.findings` to recover each rejected finding's title, fed back into future reviews as
"previously rejected here — don't repeat."

### `github_outbox` — the single GitHub egress (ADR-0059)

Every outbound GitHub **content** write becomes an intent row; the reconciler is the **sole**
consumer that posts. Producers (`serve`/finalize, reaper, webhook 👀) only INSERT.

```sql
CREATE TABLE github_outbox (
    id BIGSERIAL PRIMARY KEY,
    task_id UUID REFERENCES tasks(id) ON DELETE CASCADE,   -- nullable (a reaction may not tie to a task)
    installation_id BIGINT NOT NULL, owner TEXT NOT NULL, repo TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('review','reply','reaction','label','failure_notice')),
    payload JSONB NOT NULL,
    dedup_key TEXT NOT NULL UNIQUE,    -- e.g. '<task>:review', '<task>:reaction:eyes'
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','posted','failed')),
    attempts INT NOT NULL DEFAULT 0, last_error TEXT,
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    created_at timestamptz NOT NULL DEFAULT now(), posted_at timestamptz,
    github_id BIGINT
);
```

`owner`/`repo`/`installation_id` are carried as columns so the reconciler posts without a join.
Payloads are **fully shaped at produce time** (`src/outbox.rs`). The unique `dedup_key` makes
enqueue idempotent (`ON CONFLICT DO NOTHING`), so a re-finalize/retry never double-posts. Drain
index `github_outbox_drain_idx (next_attempt_at, created_at, id) WHERE status='pending'`.

## pgvector chunk schema — `code_chunks`

Produced by the indexer; one row per semantic unit (function/class/impl block) or windowed fallback.

```sql
CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE code_chunks (
    id BIGSERIAL PRIMARY KEY,
    repository_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    commit_sha TEXT NOT NULL,
    file_path TEXT NOT NULL,
    language TEXT NOT NULL,
    chunk_type TEXT NOT NULL,
    symbol_name TEXT,
    start_line INT NOT NULL, end_line INT NOT NULL,
    content TEXT NOT NULL,
    embedding vector(4096) NOT NULL,   -- 0005: switched from 1536
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (repository_id, commit_sha, file_path, start_line, end_line)
);
```

**4096 dimensions, no ANN index** ([ADR-0018](adr/0018-openai-compatible-embeddings.md)):
`0005_embedding_4096.sql` switched the column to 4096 dims for `qwen3-embedding-8b` (the model the
internal OpenAI-compatible **eaig** gateway actually serves — *not* `text-embedding-3-small`/1536).
4096 exceeds pgvector's HNSW limit, so there is no ANN index; the HNSW index from migration 0004 was
dropped. Search is an **exact cosine scan** scoped to one `(repository_id, commit_sha)` snapshot,
which keeps it fast (`search_code_chunks` in `src/db.rs`):

```sql
SELECT file_path, language, chunk_type, symbol_name, start_line, end_line, content,
       1.0 - (embedding <=> $1::vector) AS score
FROM code_chunks
WHERE repository_id = $2 AND commit_sha = $3
ORDER BY embedding <=> $1::vector
LIMIT $4;
```

Embeddings are written via a server-side `$N::vector` cast of a text literal (`vector_literal`), so
no extra Rust crate is needed.

**Snapshot pinning** ([ADR-0050](adr/0050-retrieval-pins-to-latest-indexed-snapshot.md)): reviews
reuse the base index ([ADR-0025](adr/0025-review-reuses-base-index.md)) pinned to the latest indexed
commit. `latest_indexed_commit` is `SELECT commit_sha FROM code_chunks WHERE repository_id=$1 ORDER
BY created_at DESC, id DESC LIMIT 1`, backed by `code_chunks_repo_recent_idx (repository_id,
created_at DESC, id DESC)` (`0018`). Old snapshots are pruned by the index sweeper
([ADR-0052](adr/0052-index-snapshot-pruning.md)). Dual retrieval pairs this with the Neo4j graph
([ADR-0003](adr/0003-dual-retrieval-neo4j-pgvector.md)). Incremental/layered indexing is future work
([RFC-0002](rfc/0002-incremental-layered-indexing.md)).

## Neo4j structural graph

The structural graph is produced in-process by the in-house `lci-codegraph` crate (tree-sitter)
([ADR-0086](adr/0086-in-house-code-graph-crate.md), which retired the Graphify path of
[ADR-0010](adr/0010-graphify-treesitter-indexing-baseline.md) /
[ADR-0019](adr/0019-graphify-cli-structural-graph.md)): the runner walks the checkout and submits
symbols + `contains`/`method`/`calls` edges, persisted over Bolt by `src/integrations/neo4j.rs`.
Retrieval is server-side scoped to the task's repo snapshot. The graph holds code topology; Postgres
remains the system of record.

## Authorization context

All API endpoints authorize on **permission claims**, not roles
([ADR-0023](adr/0023-db-backed-rbac.md)): per-capability, fail-closed, derived from a Keycloak OIDC
access-token claim ([ADR-0014](adr/0014-keycloak-oidc-resource-server.md)). The control plane is the
OAuth2 resource server and trust boundary
([ADR-0002](adr/0002-rust-control-plane-trust-boundary.md)).
