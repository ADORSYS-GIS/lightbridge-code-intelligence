# control-plane

The Rust (Axum) **control plane** — the system's trust boundary
([ADR-0002](../../docs/adr/0002-rust-control-plane-trust-boundary.md)). It owns the GitHub App private
key, mints short-lived per-task installation tokens, persists task state in Postgres, and dispatches one
Kubernetes Job per task.

## Roles

One binary, three roles, selected by the first arg or `CONTROL_PLANE_ROLE` (default `serve`):

- **`serve`** — the HTTP API: GitHub webhooks (HMAC-verified, delivery-id deduped; sets the review
  **tier** — `fast` on PR-opened, `deep` on `@mention`, [ADR-0062](../../docs/adr/0062-two-tier-review-fast-auto-deep-on-demand.md)),
  the internal task/runner API (`/internal/tasks/{id}` — context + minted token + status/chunks/review/
  graph/search, [ADR-0017](../../docs/adr/0017-agent-runner-control-plane-bootstrap.md)), the dashboard
  API (`/tasks`, `/repositories`, `/admin/*`, `/me`, `/tasks/{id}/review|cancel`), and `/metrics`.
  Verifies OIDC JWTs and enforces **permission-based authz** ([ADR-0023](../../docs/adr/0023-db-backed-rbac.md)).
  At finalize it *shapes* the review (validates findings against the PR diff) and **enqueues** egress —
  it never posts to GitHub directly.
- **`dispatcher`** — the queue consumer: claims `queued` tasks (`SELECT … FOR UPDATE SKIP LOCKED`),
  launches the agent Job (`integrations/k8s.rs`), and runs the **reaper** (deletes cancelled/finished
  Jobs) + the **purge reconciler** (removes data for denied/removed repos). See RFC-0001.
- **`reconciler`** — a **single replica that owns ALL GitHub egress** ([ADR-0058](../../docs/adr/0058-rename-poller-role-to-reconciler.md)/[ADR-0059](../../docs/adr/0059-reconciler-owns-all-github-egress.md)):
  it drains the transactional `github_outbox` and delivers reviews / replies / reactions / failure
  notices, and runs the **feedback poll** (👍/👎 reactions → `review_feedback`,
  [ADR-0035](../../docs/adr/0035-review-feedback-signal.md)). One writer ⇒ no double-post; it holds the
  GitHub App key (to mint tokens). (`poller` is the legacy alias the binary still accepts.)

```bash
cargo run -p control-plane              # serve (default)
cargo run -p control-plane dispatcher   # dispatcher
cargo run -p control-plane reconciler   # reconciler (GitHub egress + feedback poll)
```

## What it talks to

- **Postgres (+pgvector)** — the work queue, repositories, reviews, and the `code_chunks` semantic index.
  Schema via hand-written SQLx migrations (cratestack deferred,
  [ADR-0005](../../docs/adr/0005-cratestack-schema-first-control-plane.md)).
- **Neo4j** — the structural graph (the control plane writes; built in-process by the runner's
  **lci-codegraph** crate, [ADR-0086](../../docs/adr/0086-in-house-code-graph-crate.md)).
- **GitHub** — webhooks in; minted installation tokens used by runners to clone; all write-back
  (validated reviews, replies, reactions, notices) flows **out through the `reconciler`'s
  `github_outbox`** — one writer ([ADR-0022](../../docs/adr/0022-review-writeback-control-plane.md) →
  [ADR-0059](../../docs/adr/0059-reconciler-owns-all-github-egress.md)).
- **Kubernetes** — builds + launches the per-task Job (`integrations/k8s.rs::job_manifest`); one Job per task
  ([ADR-0004](../../docs/adr/0004-one-k8s-job-per-task.md)).

## Trust boundary

The GitHub App key never leaves this service. A Job receives only a short-lived (~1h) installation token
plus a per-task `AGENT_RUNNER_TOKEN` and the embeddings key — never datastore credentials
([ADR-0020](../../docs/adr/0020-mcp-servers-via-control-plane.md)). `AGENT_RUNNER_TOKEN` is minted fresh
per Job from `RUNNER_TOKEN_SIGNING_KEY` — a signing key that, unlike the token it produces, never itself
leaves this service ([ADR-0092](../../docs/adr/0092-per-task-runner-tokens.md)). The runner *proposes*;
the control plane *validates and writes back*.

## Configuration

Read from `/etc/lightbridge/control-plane.json` (mounted ConfigMap) or env. Key knobs: `agent.*`
(runner image, namespace, service account, resources, deadline, CA secret), `dispatcher.*` (lease /
poll / reap cadences), the OIDC issuer/audience + `PERMISSIONS_CLAIM`, and `RUNNER_TOKEN_SIGNING_KEY`
(the per-task runner-token signing key, ADR-0092). See
[`src/config.rs`](src/config.rs) and [docs/kubernetes-deployment.md](../../docs/kubernetes-deployment.md).

## Tests

`cargo nextest run -p control-plane` (`#[sqlx::test]` against a throwaway Postgres). The runner↔control
contract is tested on both sides (wiremock in the runner, `#[sqlx::test]` here).
