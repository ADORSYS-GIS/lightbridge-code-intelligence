# Local setup guide

Run Lightbridge Code Intelligence on your machine — from a fast "UI + control plane" loop to a full
closer-to-prod cluster (multipass + k3s) that actually executes agent Jobs, with GitHub events
arriving over a webhook proxy or triggered manually.

> Recipes live in the `Justfile`; local dependencies in `compose.yaml`. See also
> [jobs-and-lifecycle.md](jobs-and-lifecycle.md), [kubernetes-deployment.md](kubernetes-deployment.md),
> and ADR-0013 (local dev tooling).

## Topology

```mermaid
flowchart LR
    GH[GitHub App] -- webhook --> PROXY[smee / cloudflared proxy]
    PROXY --> CP["control plane :8080<br/>/github/webhook, /tasks, /internal/*"]
    WEB["web :3000 (Next.js)"] --> CP
    WEB -- OIDC --> KC["Keycloak :8081"]
    CP -- OIDC verify --> KC
    CP --> PG[("Postgres+pgvector :5432")]
    CP --> NEO[("Neo4j :7687")]
    CP -- launches --> JOB["agent-runner Job<br/>(k3s, or run manually)"]
    JOB --> CP
```

There are two ways to run the part that executes work (the **agent-runner Job**):
- **Manual runner** — run one task's runner process directly on your host (no cluster). Fastest for
  iterating on the runner / indexing / review.
- **k3s via multipass** — a real cluster so the dispatcher launches Jobs exactly like prod.

## Prerequisites

- **Node 20+ and pnpm** (`corepack enable`), **Rust** (stable, via rustup), **Docker** (for compose).
- Optional: **multipass** (local k3s), **`cargo-nextest`** (`cargo install cargo-nextest`), a tunnel
  for webhooks (**`smee-client`**: `pnpm dlx smee-client`, or cloudflared/ngrok).

## 1. Dependencies + app (fast loop)

```bash
just setup           # pnpm install + cargo fetch
just up              # docker compose: Postgres+pgvector, Neo4j, Keycloak (realm imported)
cp apps/web/.env.example apps/web/.env.local
just dev             # Next.js (:3000) + control plane (:8080) via Turborepo
```

What `just up` gives you (all local, **dev-only credentials**):

| Service | URL | Credentials |
|---|---|---|
| Postgres + pgvector | `localhost:5432` | `lightbridge` / `lightbridge` (db `lightbridge`) |
| Neo4j | browser `localhost:7474`, bolt `localhost:7687` | `neo4j` / `lightbridge` |
| Keycloak | `localhost:8081` | admin `admin` / `admin` |
| OIDC realm | `lightbridge` | client `lightbridge-web` (public/PKCE), dev user `dev` / `password` |

Open `http://localhost:3000`, sign in as `dev` / `password`. The control plane validates that token
(issuer `http://localhost:8081/realms/lightbridge`, audience `lightbridge-api`).

### Control-plane env

`just dev` runs the backend degraded (`ALLOW_NO_DB=1`, in-memory dedup). To use the real datastores
and auth, run it with:

```bash
DATABASE_URL=postgres://lightbridge:lightbridge@localhost:5432/lightbridge \
NEO4J_URI=bolt://localhost:7687 NEO4J_USER=neo4j NEO4J_PASSWORD=lightbridge \
OIDC_ISSUER=http://localhost:8081/realms/lightbridge OIDC_AUDIENCE=lightbridge-api \
RUNNER_TOKEN_SIGNING_KEY=dev-signing-key \
cargo run -p control-plane          # migrations run automatically on connect
```

`RUNNER_TOKEN_SIGNING_KEY` replaces the old shared `AGENT_RUNNER_TOKEN` dev bearer
([ADR-0092](adr/0092-per-task-runner-tokens.md)): the control plane now mints a fresh, per-task token
for each Job rather than accepting one fixed string, so there's no static value to hardcode here — see
§3 for how the manual runner gets a token to present.

Permission-based authz (ADR-0023): the dev realm should emit a `permissions` claim. To exercise gated
endpoints (`task:read`, `repo:approve`, `task:cancel`, …) add those to the dev user's token via a
client scope in Keycloak (see `deploy/keycloak/realm-lightbridge.json`); without them the UI hides
gated affordances and the API returns 403 (fail-closed).

## 2. Trigger work

A task is created by a GitHub event (PR opened / `@mention`) or a repo approval. Two routes:

### A. Real GitHub events via a webhook proxy

1. **Create a GitHub App** (Settings → Developer settings → GitHub Apps): permissions
   *Contents: read*, *Pull requests: read & write*, *Issues: read & write*, *Metadata: read*;
   subscribe to *Pull request*, *Issue comment*, and *Push* events (Push is what re-indexes the
   default branch on a merge — omit it and the index only ever runs once, on approval). Generate a
   **private key** and note the **App ID** + **webhook secret**.
2. **Proxy webhooks to localhost**:
   ```bash
   pnpm dlx smee-client --url https://smee.io/<your-channel> \
     --target http://localhost:8080/github/webhook
   ```
   Set the App's **Webhook URL** to the smee channel.
3. **Point the control plane at the App**:
   ```bash
   GITHUB_APP_ID=<id> GITHUB_APP_PRIVATE_KEY="$(cat app.pem)" \
   GITHUB_WEBHOOK_SECRET=<secret> GITHUB_APP_HANDLE=<app-handle> \
   # …plus the DATABASE_URL/OIDC/NEO4J vars from §1
   cargo run -p control-plane
   ```
4. **Install the App** on a test repo, approve it in the console (→ enqueues an **index** job), then
   open a PR (→ a **review** job). See the run in the console at `/dashboard/runs`.

### B. Manual trigger (no GitHub)

- **Approve a repo** in the admin console (with `repo:approve`) to enqueue its index task, or
- **Replay a webhook**: POST a sample `pull_request.opened` / `issue_comment` body to
  `POST /github/webhook` with a valid `X-Hub-Signature-256` (HMAC-SHA256 of the body using
  `GITHUB_WEBHOOK_SECRET`) and `X-GitHub-Delivery`. (HMAC verify + delivery dedup are enforced.)

## 3. Execute the task (the runner)

Creating a task leaves it **`queued`**; something must run the Job.

### Manual runner (no cluster)

Grab the queued task's id (from `/dashboard/runs` or the DB), mint it a token, and run the runner
against your local control plane:

```bash
TASK_ID=<uuid>
TOKEN=$(RUNNER_TOKEN_SIGNING_KEY=dev-signing-key cargo run -p control-plane -- mint-runner-token "$TASK_ID")

TASK_ID="$TASK_ID" CONTROL_PLANE_URL=http://localhost:8080 AGENT_RUNNER_TOKEN="$TOKEN" \
EMBEDDINGS_BASE_URL=<openai-compatible-url> EMBEDDINGS_API_KEY=<key> EMBEDDINGS_MODEL=<model> \
cargo run -p agent-runner
```

`mint-runner-token` (ADR-0092) is the same signer the dispatcher itself mints Job tokens with — it
needs only `RUNNER_TOKEN_SIGNING_KEY` (matching what you started the control plane with) and the task
id, and prints one token to stdout. The token is scoped to that task and expires after
`AGENT_JOB_DEADLINE_SECONDS` (default 1h) — mint a fresh one if the run takes long enough to expire.

It fetches context, clones, indexes (pgvector + Neo4j), and — for a review task — runs OpenCode
(requires `opencode` on PATH + an LLM provider; indexing-only works without it).

### Full cluster (multipass + k3s)

For the dispatcher to launch Jobs like prod:

```bash
just k3s-up          # multipass VM + k3s (TENTATIVE path; see kubernetes-deployment.md)
# deploy Postgres/Neo4j/Keycloak + the control plane, dispatcher, and the agent-runner image
# into the cluster (the ai-helm chart `charts/lightbridge-code-intelligence` is the prod shape).
just k3s-down        # tear down
```

The dispatcher (`CONTROL_PLANE_ROLE=dispatcher`) then claims queued tasks and creates one Job per
task in the agents namespace. See [kubernetes-deployment.md](kubernetes-deployment.md) for manifests
and the prod chart.

## 4. Quality gate (run before pushing — shift-left)

```bash
just fmt     # Biome + rustfmt
just lint    # biome check + clippy -D warnings
just test    # pnpm test + cargo nextest
just ci      # the full local gate (schema lint + build + xtask ci)
```

## Env reference

| Var | Used by | Local value |
|---|---|---|
| `DATABASE_URL` | control plane | `postgres://lightbridge:lightbridge@localhost:5432/lightbridge` |
| `NEO4J_URI` / `NEO4J_USER` / `NEO4J_PASSWORD` | control plane | `bolt://localhost:7687` / `neo4j` / `lightbridge` |
| `OIDC_ISSUER` / `OIDC_AUDIENCE` | control plane + web | `http://localhost:8081/realms/lightbridge` / `lightbridge-api` |
| `RUNNER_TOKEN_SIGNING_KEY` | control plane | any dev string (never given to the runner) |
| `AGENT_RUNNER_TOKEN` | runner | minted per task via `mint-runner-token` (§3) |
| `GITHUB_APP_ID` / `GITHUB_APP_PRIVATE_KEY` / `GITHUB_WEBHOOK_SECRET` / `GITHUB_APP_HANDLE` | control plane | from your test GitHub App |
| `EMBEDDINGS_BASE_URL` / `EMBEDDINGS_API_KEY` / `EMBEDDINGS_MODEL` | runner | any OpenAI-compatible endpoint |
| `GITHUB_APP_INSTALL_URL` / `PERMISSIONS_CLAIM` | web | App page URL / `permissions` |

## Troubleshooting

- **Control plane fails readiness** — it fails closed without `OIDC_ISSUER` (and without `DATABASE_URL`
  unless `ALLOW_NO_DB=1`). Check the env block above.
- **`/internal/tasks/{id}/graph` returns 503** — `NEO4J_URI` unset; structural indexing is skipped
  (non-fatal, the semantic index still lands).
- **Task stuck `queued`** — nothing is executing it: run the manual runner (§3) or bring up k3s.
- **Webhook 401/400** — signature mismatch (wrong `GITHUB_WEBHOOK_SECRET`) or a replayed delivery id
  (dedup). Recompute the HMAC over the exact body.
- **API 403 after login** — the token lacks the required `permissions` claim (ADR-0023); add it via a
  Keycloak client scope.
