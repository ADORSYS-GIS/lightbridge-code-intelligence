# Local development guide — running all services locally

This guide covers running the **entire Lightbridge Code Intelligence stack** on your local machine:
the control plane, the web console, the agent-runner, and all external dependencies (Postgres+pgvector,
Neo4j, Keycloak). It also covers running a **local GitLab instance** so you can test the webhook
flow without a public GitHub App — and a **local GitHub App** path via a webhook proxy for real
GitHub events.

> **Existing docs:** This guide complements [`docs/local-setup.md`](docs/local-setup.md) (which
> covers the fast dev loop) and [`docs/kubernetes-deployment.md`](docs/kubernetes-deployment.md)
> (prod deployment). See also the root [`explain.md`](explain.md) for the setup guide covering
> GitHub and GitLab when external services are already running somewhere.

---

## Table of contents

1. [Prerequisites](#1-prerequisites)
2. [Start external dependencies (docker compose)](#2-start-external-dependencies-docker-compose)
3. [Run the control plane](#3-run-the-control-plane)
4. [Run the web console](#4-run-the-web-console)
5. [Run the agent-runner manually](#5-run-the-agent-runner-manually)
6. [Connect a real GitHub App (via webhook proxy)](#6-connect-a-real-github-app-via-webhook-proxy)
7. [Run GitLab locally](#7-run-gitlab-locally)
8. [Full local cluster (multipass + k3s)](#8-full-local-cluster-multipass--k3s)
9. [Quality gate (run before pushing)](#9-quality-gate-run-before-pushing)
10. [Env reference](#10-env-reference)
11. [Troubleshooting](#11-troubleshooting)

---

## 1. Prerequisites

### Required

| Tool | Version | Why |
|------|---------|-----|
| **Node.js** | 20+ | Next.js web app |
| **pnpm** | latest (`corepack enable`) | JS package manager (Turborepo) |
| **Rust** | stable (via rustup) | All backend services |
| **Docker** | latest | docker compose for Postgres, Neo4j, Keycloak, GitLab |

### Optional

| Tool | Why |
|------|-----|
| **cargo-nextest** | Faster Rust test runner (`cargo install cargo-nextest`) |
| **smee-client** | Webhook proxy for GitHub events (`pnpm dlx smee-client`) |
| **cloudflared** or **ngrok** | Alternative tunnel for webhooks |
| **multipass** | Local k3s cluster for closer-to-prod testing |
| **just** | Task runner — recipes in `justfile` |

### Install

```bash
# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# pnpm
corepack enable

# just (if not installed)
cargo install just

# cargo-nextest (optional, faster tests)
cargo install cargo-nextest
```

### First-time setup

```bash
git clone <repo-url> && cd lightbridge-code-intelligence
just setup           # pnpm install + cargo fetch
```

---

## 2. Start external dependencies (docker compose)

The `compose.yaml` file defines three local dev dependencies:

| Service | Image | Port | Credentials |
|---------|-------|------|-------------|
| Postgres + pgvector | `pgvector/pgvector:pg17` | `localhost:5432` | `lightbridge` / `lightbridge` (db: `lightbridge`) |
| Neo4j | `neo4j:5` | browser `localhost:7474`, bolt `localhost:7687` | `neo4j` / `lightbridge` |
| Keycloak | `quay.io/keycloak/keycloak:26.0` | `localhost:8081` | admin `admin` / `admin` |

```bash
just up               # docker compose up -d
```

Verify they are healthy:

```bash
docker compose ps     # all three should show "healthy"
```

### Keycloak realm

Keycloak imports the `lightbridge` realm on boot (from
`deploy/keycloak/realm-lightbridge.json`):

- **Realm**: `lightbridge`
- **Client**: `lightbridge-web` (public, PKCE)
- **API audience**: `lightbridge-api`
- **Dev user**: `dev` / `password`

Open `http://localhost:8081` and sign in as `admin` / `admin` to inspect or modify the realm.

### Postgres + pgvector

The control plane runs SQL migrations automatically on connect. The pgvector extension is
pre-installed in the `pgvector/pgvector:pg17` image. You can connect with:

```bash
psql postgres://lightbridge:lightbridge@localhost:5432/lightbridge
```

### Neo4j

The Neo4j browser is at `http://localhost:7474`. Sign in as `neo4j` / `lightbridge`.

### Tear down

```bash
just down             # docker compose down (keeps volumes)
docker compose down -v  # also destroy data volumes
```

---

## 3. Run the control plane

The control plane is the Rust HTTP server that receives webhooks, serves the internal runner API,
and (in other roles) dispatches tasks and reconciles GitHub egress.

### Fast loop (degraded, no database)

```bash
just dev-backend
```

This runs with `ALLOW_NO_DB=1` (in-memory dedup, single-replica) and Neo4j env vars pointing at
the compose Neo4j. Good for iterating on HTTP handlers without a database.

### Full loop (with database + OIDC)

```bash
DATABASE_URL=postgres://lightbridge:lightbridge@localhost:5432/lightbridge \
NEO4J_URI=bolt://localhost:7687 \
NEO4J_USER=neo4j \
NEO4J_PASSWORD=lightbridge \
OIDC_ISSUER=http://localhost:8081/realms/lightbridge \
OIDC_AUDIENCE=lightbridge-api \
AGENT_RUNNER_TOKEN=dev-runner-token \
cargo run -p control-plane
```

Migrations run automatically on connect. The server listens on `0.0.0.0:8080` and metrics on
`0.0.0.0:9090`.

### Running all three roles locally

Open three terminals:

```bash
# Terminal 1 — serve (HTTP + webhooks)
DATABASE_URL=postgres://lightbridge:lightbridge@localhost:5432/lightbridge \
NEO4J_URI=bolt://localhost:7687 NEO4J_USER=neo4j NEO4J_PASSWORD=lightbridge \
OIDC_ISSUER=http://localhost:8081/realms/lightbridge OIDC_AUDIENCE=lightbridge-api \
AGENT_RUNNER_TOKEN=dev-runner-token \
CONTROL_PLANE_ROLE=serve \
cargo run -p control-plane

# Terminal 2 — dispatcher (claims tasks → would launch K8s Jobs)
DATABASE_URL=postgres://lightbridge:lightbridge@localhost:5432/lightbridge \
CONTROL_PLANE_ROLE=dispatcher \
cargo run -p control-plane

# Terminal 3 — reconciler (drains outbox → posts to GitHub)
DATABASE_URL=postgres://lightbridge:lightbridge@localhost:5432/lightbridge \
GITHUB_APP_ID=<id> GITHUB_APP_PRIVATE_KEY="$(cat app.pem)" \
GITHUB_APP_HANDLE=<handle> \
CONTROL_PLANE_ROLE=reconciler \
cargo run -p control-plane
```

> **Note:** The dispatcher will try to talk to a Kubernetes cluster. Without one, it will fail to
> launch Jobs but will still claim tasks (you can then run the agent-runner manually — see §5).

### Health checks

```bash
curl http://localhost:8080/healthz   # liveness (always 200)
curl http://localhost:8080/readyz    # readiness (requires DB ping unless ALLOW_NO_DB=1)
curl http://localhost:8080/metrics   # Prometheus metrics
```

---

## 4. Run the web console

The web console is a Next.js app (`apps/web`) that talks to the control plane API and uses
Keycloak for OIDC authentication.

```bash
cp apps/web/.env.example apps/web/.env.local
just dev-web          # pnpm --filter @lightbridge/web dev
```

Or run both the web app and the control plane together via Turborepo:

```bash
just dev              # pnpm dev (Next.js :3000 + control plane :8080)
```

Open `http://localhost:3000` and sign in as `dev` / `password`.

### Web app env

Check `apps/web/.env.example` for the full list. Key vars:

| Var | Value |
|-----|-------|
| `NEXTAUTH_URL` | `http://localhost:3000` |
| `OIDC_ISSUER` | `http://localhost:8081/realms/lightbridge` |
| `OIDC_AUDIENCE` | `lightbridge-api` |
| `OIDC_CLIENT_ID` | `lightbridge-web` |
| `CONTROL_PLANE_URL` | `http://localhost:8080` |
| `GITHUB_APP_INSTALL_URL` | URL to your GitHub App's install page |
| `PERMISSIONS_CLAIM` | `permissions` |

---

## 5. Run the agent-runner manually

The agent-runner is what actually does the work: clone, index (tree-sitter → embeddings →
pgvector, Graphify → Neo4j), SAST, and review. In production, the dispatcher launches it as a K8s
Job. Locally, you can run it directly.

### Prerequisites

You need an **embeddings endpoint** (OpenAI-compatible). Options:
- Use the real OpenAI API: `EMBEDDINGS_BASE_URL=https://api.openai.com/v1`
- Use a local LLM gateway (e.g. [eaig/core-gateway](https://github.com/envoy-ai-gateway/core-gateway))
- Use any OpenAI-compatible API (vLLM, Ollama with embeddings, etc.)

For **review tasks**, you also need a **chat completions endpoint** (same OpenAI-compatible API).
Without it, the runner will index only (no review).

### Run the runner

First, create a task (via the web console by approving a repo, or by sending a webhook). Then
grab the task ID from `/dashboard/runs` or the database:

```bash
# Get the latest queued task ID
psql postgres://lightbridge:lightbridge@localhost:5432/lightbridge \
  -c "SELECT id, command_text, status FROM tasks ORDER BY created_at DESC LIMIT 5;"
```

Then run the runner:

```bash
TASK_ID=<uuid> \
CONTROL_PLANE_URL=http://localhost:8080 \
AGENT_RUNNER_TOKEN=dev-runner-token \
EMBEDDINGS_BASE_URL=https://api.openai.com/v1 \
EMBEDDINGS_API_KEY=<your-key> \
EMBEDDINGS_MODEL=text-embedding-3-small \
cargo run -p agent-runner
```

For a **review task**, add the LLM vars:

```bash
TASK_ID=<uuid> \
CONTROL_PLANE_URL=http://localhost:8080 \
AGENT_RUNNER_TOKEN=dev-runner-token \
EMBEDDINGS_BASE_URL=https://api.openai.com/v1 \
EMBEDDINGS_API_KEY=<key> \
EMBEDDINGS_MODEL=text-embedding-3-small \
LLM_BASE_URL=https://api.openai.com/v1 \
LLM_API_KEY=<key> \
LLM_MODEL=gpt-4o \
LLM_STREAM=1 \
cargo run -p agent-runner
```

### What the runner does

1. Fetches task context from the control plane (`GET /internal/tasks/{id}`)
2. Clones the repository into `WORKDIR` (default `/workspace`)
3. If `command_text == "index"`:
   - Walks the repo, detects languages, chunks with tree-sitter
   - Embeds each chunk → submits to control plane → stored in pgvector
   - Spawns Graphify (`graphify update <checkout> --no-cluster`) → submits graph to control plane
4. If `command_text == "review"`:
   - Runs SAST (opengrep) over changed files (if `SAST_ENABLED=1`)
   - Runs the review agent loop (multi-turn chat with mediated tools)
   - Buffers findings via `add_review_comment` → control plane validates + scopes to diff
   - On `finish`, control plane posts the review (via the reconciler outbox)

### Using a config file instead of env vars

Create `agent.json`:

```json
{
  "embeddings": {
    "base_url": "{env:EMBEDDINGS_BASE_URL}",
    "api_key": "{env:EMBEDDINGS_API_KEY}",
    "model": "{env:EMBEDDINGS_MODEL}",
    "request_timeout_secs": 30
  },
  "review": {
    "deep": {
      "base_url": "{env:LLM_BASE_URL}",
      "api_key": "{env:LLM_API_KEY}",
      "model": "{env:LLM_MODEL}",
      "system_prompt": "You are a code reviewer...",
      "max_turns": 20,
      "stream": true
    }
  }
}
```

Then:

```bash
AGENT_CONFIG=./agent.json \
TASK_ID=<uuid> \
CONTROL_PLANE_URL=http://localhost:8080 \
AGENT_RUNNER_TOKEN=dev-runner-token \
EMBEDDINGS_BASE_URL=... EMBEDDINGS_API_KEY=... EMBEDDINGS_MODEL=... \
LLM_BASE_URL=... LLM_API_KEY=... LLM_MODEL=... \
cargo run -p agent-runner
```

---

## 6. Connect a real GitHub App (via webhook proxy)

To receive real GitHub webhook events on your local machine, use a webhook proxy like
**smee.io** (or cloudflared/ngrok).

### Step 1 — Create a GitHub App

Go to **GitHub → Settings → Developer settings → GitHub Apps → New GitHub App**.

| Field | Value |
|-------|-------|
| App name | e.g. `lightbridge-dev` |
| Webhook URL | `https://smee.io/<your-channel>` |
| Webhook secret | a random string |

**Repository permissions:**

| Permission | Level |
|------------|-------|
| Metadata | Read |
| Contents | Read |
| Pull requests | Read & Write |
| Issues | Read & Write |

**Webhook event subscriptions:**
- Pull request
- Push
- Issue comment
- Installation
- Installation repositories

Generate a private key and note the **App ID**.

### Step 2 — Start the webhook proxy

```bash
pnpm dlx smee-client --url https://smee.io/<your-channel> \
  --target http://localhost:8080/github/webhook
```

### Step 3 — Start the control plane with GitHub App credentials

```bash
DATABASE_URL=postgres://lightbridge:lightbridge@localhost:5432/lightbridge \
NEO4J_URI=bolt://localhost:7687 NEO4J_USER=neo4j NEO4J_PASSWORD=lightbridge \
OIDC_ISSUER=http://localhost:8081/realms/lightbridge OIDC_AUDIENCE=lightbridge-api \
AGENT_RUNNER_TOKEN=dev-runner-token \
GITHUB_APP_ID=<app-id> \
GITHUB_APP_PRIVATE_KEY="$(cat /path/to/app.pem)" \
GITHUB_WEBHOOK_SECRET=<webhook-secret> \
GITHUB_APP_HANDLE=<app-handle> \
cargo run -p control-plane
```

### Step 4 — Install and test

1. Install the App on a test repository
2. The repo appears as **pending** in the web console
3. Approve it (requires `repo:approve` permission — add via Keycloak client scope)
4. Approval enqueues an **index** task
5. Run the agent-runner manually (§5) to execute the index task
6. Open a PR → a **fast review** task is enqueued
7. `@mention` the bot in a PR → a **deep review** task is enqueued

### Alternative: manual webhook replay (no GitHub)

You can POST a sample webhook payload directly:

```bash
# Compute HMAC signature
BODY='{"action":"opened","pull_request":{...}}'
SECRET="your-webhook-secret"
SIG="sha256=$(echo -n "$BODY" | openssl dgst -sha256 -hmac "$SECRET" | sed 's/^.* //')"

curl -X POST http://localhost:8080/github/webhook \
  -H "Content-Type: application/json" \
  -H "X-Hub-Signature-256: $SIG" \
  -H "X-GitHub-Delivery: $(uuidgen)" \
  -H "X-GitHub-Event: pull_request" \
  -d "$BODY"
```

---

## 7. Run GitLab locally

GitLab support is **implemented** (ADR-0071). The control plane has a unified `/webhook` route
that auto-detects the platform from headers, a `GitlabClient` implementing the `CodePlatform`
trait, and a unified `outbox` table with a `platform` column. Running GitLab locally lets you
test the full GitLab flow end-to-end.

### Step 1 — Start GitLab in Docker

Add a GitLab service to a local compose override or run it standalone:

```bash
docker run -d --name gitlab \
  --hostname gitlab.local \
  -p 8443:443 -p 8880:80 -p 2222:22 \
  -v gitlab_config:/etc/gitlab \
  -v gitlab_logs:/var/log/gitlab \
  -v gitlab_data:/var/opt/gitlab \
  --restart unless-stopped \
  --shm-size 256m \
  gitlab/gitlab-ce:latest
```

| Port | Maps to | Purpose |
|------|---------|---------|
| `localhost:8880` | GitLab HTTP | Web UI, API |
| `localhost:8443` | GitLab HTTPS | (if TLS configured) |
| `localhost:2222` | GitLab SSH | Git over SSH |

GitLab CE takes **3–5 minutes** to start. Check readiness:

```bash
docker exec gitlab gitlab-ctl status
# or watch the web UI at http://localhost:8880
```

### Step 2 — Initial root login

1. Open `http://localhost:8880`
2. Set the root password (first visit prompts for it)
3. Sign in as `root` / `<your-password>`

### Step 3 — Create a project + access token

1. Create a new project (e.g. `test-repo`)
2. Push some code to it:
   ```bash
   git remote add local-gitlab http://localhost:8880/root/test-repo.git
   git push local-gitlab main
   ```
3. Create a **project access token**:
   - Go to **Settings → Access Tokens**
   - Name: `lightbridge-bot`
   - Role: **Maintainer** (needed to post notes on MRs)
   - Scopes: `api`, `read_repository`
   - Copy the token

### Step 4 — Configure webhooks in GitLab

1. Go to **Settings → Webhooks**
2. **URL**: `http://localhost:8080/webhook` (the unified route — platform is auto-detected from
   the `X-Gitlab-Event` header)
3. **Secret token**: a shared secret (this becomes `GITLAB_WEBHOOK_SECRET`)
4. **Trigger events**: Merge request events, Push events, Note events

### Step 5 — Start the control plane with GitLab env vars

```bash
# Start the dev dependencies (Postgres, Neo4j, Keycloak)
just up

# Run the control plane with GitLab configured
export DATABASE_URL=postgres://lightbridge:lightbridge@localhost:5432/lightbridge
export NEO4J_URI=bolt://localhost:7687
export NEO4J_USER=neo4j
export NEO4J_PASSWORD=lightbridge
export ALLOW_NO_DB=1
export GITLAB_API_URL=http://localhost:8880/api/v4
export GITLAB_API_TOKEN=<the-token-from-step-3>
export GITLAB_WEBHOOK_SECRET=<the-secret-from-step-4>
export GITLAB_BOT_HANDLE=lightbridge-bot

cargo run -p control-plane
```

The control plane will start with both the `serve` role (receiving webhooks on `/webhook`) and
the internal API. The readiness check passes because `GITLAB_WEBHOOK_SECRET` is set.

### Step 6 — Approve the repo

GitLab repos follow the same approval gate as GitHub (ADR-0063). After the first webhook arrives
(a push or MR event), the repo is registered as `pending`. Approve it via the admin API:

```bash
# List pending repos
curl -s http://localhost:8080/admin/repos | jq '.[] | select(.status == "pending")'

# Approve (requires a valid OIDC token with repo:approve permission)
curl -X POST http://localhost:8080/admin/repos/<repo_id>/approve \
  -H "Authorization: Bearer <oidc-token>"
```

### Step 7 — Test the GitLab flow

1. **Open an MR** in GitLab → the control plane receives a `Merge Request Hook` webhook →
   creates a fast-tier review task (after approval)
2. **Push to the default branch** → `Push Hook` → creates an index task (after approval)
3. **Comment `@lightbridge-bot review` on an MR** → `Note Hook` → creates a deep-tier review task
4. Watch the logs: `cargo run -p control-plane` will log the platform detection, event routing,
   and task creation

### Step 8 — Run the agent-runner manually

The agent-runner is platform-agnostic. The control plane hands it the right `clone_url` (with
the GitLab token embedded) via the internal API. Run it manually:

```bash
# Get a task ID from the control plane logs or the DB
TASK_ID=<uuid>

# Run the agent-runner
CONTROL_PLANE_URL=http://localhost:8080 \
AGENT_RUNNER_TOKEN=<your-runner-token> \
TASK_ID=$TASK_ID \
cargo run -p agent-runner
```

The runner will clone from `http://oauth2:<token>@localhost:8880/root/test-repo.git`, build the
index, run SAST, run the review agent, and post results back to the control plane. The
reconciler will then post the review to GitLab as MR notes + inline discussions.

### GitLab webhook events handled

| GitLab event | Header `X-Gitlab-Event` | Maps to |
|--------------|------------------------|---------|
| Merge request opened | `Merge Request Hook` | Fast review task (after approval gate + bot/draft skip) |
| Merge request closed | `Merge Request Hook` | Cancel active tasks for that MR |
| Push to default branch | `Push Hook` | Re-index task (after approval gate) |
| Note with `@mention` on MR | `Note Hook` | Deep review task (SHAs fetched via GitLab API) |
| Note with `@mention` on Issue | `Note Hook` | Deep review/answer task |

### GitLab API endpoints used

| Action | GitLab API | Trait method |
|--------|-----------|--------------|
| Get MR changes (diff) | `GET /projects/:id/merge_requests/:iid/changes` | `list_changed_files` |
| Get project (default branch) | `GET /projects/:id` | `default_branch` |
| Get MR (diff_refs for SHAs) | `GET /projects/:id/merge_requests/:iid` | `pr_shas` |
| Post inline discussion | `POST /projects/:id/merge_requests/:iid/discussions` (with `position`) | `post_review` |
| Post MR note | `POST /projects/:id/merge_requests/:iid/notes` | `post_review` (body) / `post_comment` |
| Post issue note | `POST /projects/:id/issues/:iid/notes` | `post_comment` (fallback) |
| Add award emoji | `POST /projects/:id/merge_requests/:iid/award_emoji` | `add_reaction` |
| Set MR labels | `PUT /projects/:id/merge_requests/:iid` | `add_labels` |
| List MR discussions | `GET /projects/:id/merge_requests/:iid/discussions` | `list_review_comments` |

### Known limitations (GitLab)

- **Comment-reaction feedback polling** (`list_comment_reactions`) returns an empty Vec — the
  GitLab award-emoji endpoint needs the MR/issue `iid` to reach a note, and the outbox only
  stores the note `id`. Reviews, comments, labels, and issue/MR-body reactions all post correctly.
- **`post_comment` probes MR then issue** — costs one 404 on the wrong noteable type; acceptable
  for a reply/failure-notice path that fires once per task.
- **Bot detection** is less precise than GitHub — checks author name suffix patterns
  (`_bot`/`-bot`/`GitLab`), fails open.
- **No installation/system hooks** — GitLab repos are registered manually via the admin console.

---

## 8. Full local cluster (multipass + k3s)

For closer-to-prod testing, run a local Kubernetes cluster where the dispatcher can launch real
Jobs.

```bash
just k3s-up         # multipass VM + k3s (TENTATIVE — see docs/kubernetes-deployment.md)
```

This creates a multipass VM with k3s installed. You then need to:
1. Deploy Postgres/Neo4j/Keycloak into the cluster (or use the compose versions with port-forwarding)
2. Deploy the control plane (serve + dispatcher + reconciler roles)
3. Deploy the agent-runner image into the cluster's registry or pull from GHCR
4. Create the `lightbridge-agents` namespace and `lightbridge-agent` ServiceAccount

See [`docs/kubernetes-deployment.md`](docs/kubernetes-deployment.md) for the prod chart shape.

```bash
just k3s-down       # tear down
```

---

## 9. Quality gate (run before pushing)

```bash
just fmt             # Biome (JS/TS) + rustfmt (Rust)
just lint            # biome check + clippy -D warnings
just test            # pnpm test + cargo nextest (or cargo test)
just ci              # full local CI gate (schema lint + build + xtask ci)
```

---

## 10. Env reference

### Control plane

| Var | Required | Default | Local value |
|-----|----------|---------|------------|
| `CONTROL_PLANE_ROLE` | no | `serve` | `serve` / `dispatcher` / `reconciler` |
| `DATABASE_URL` | prod: yes | — | `postgres://lightbridge:lightbridge@localhost:5432/lightbridge` |
| `ALLOW_NO_DB` | no | unset | `1` for degraded dev |
| `NEO4J_URI` | no | `bolt://localhost:7687` | `bolt://localhost:7687` |
| `NEO4J_USER` | no | `neo4j` | `neo4j` |
| `NEO4J_PASSWORD` | no | empty | `lightbridge` |
| `OIDC_ISSUER` | prod: yes | — | `http://localhost:8081/realms/lightbridge` |
| `OIDC_AUDIENCE` | no | `account` | `lightbridge-api` |
| `OIDC_JWKS_URI` | no | `${OIDC_ISSUER}/protocol/openid-connect/certs` | — |
| `GITHUB_APP_ID` | GitHub: yes | — | from your GitHub App |
| `GITHUB_APP_PRIVATE_KEY` | GitHub: yes | — | PEM file contents |
| `GITHUB_WEBHOOK_SECRET` | GitHub: yes | — | from your GitHub App |
| `GITHUB_APP_HANDLE` | no | `lightbridge-assistant` | your App's handle |
| `AGENT_RUNNER_TOKEN` | yes | — | any shared dev string (e.g. `dev-runner-token`) |
| `BIND_ADDR` | no | `0.0.0.0:8080` | — |
| `METRICS_ADDR` | no | `0.0.0.0:9090` | — |
| `PERMISSIONS_CLAIM` | no | `permissions` | `permissions` |
| `CONTROL_PLANE_CONFIG` | no | `/etc/lightbridge/control-plane.json` | path to JSON config |
| `HOSTNAME` | no | hostname | used as dispatcher lease owner |

### K8s launcher (dispatcher)

| Var | Default | Local value |
|-----|---------|-------------|
| `AGENT_NAMESPACE` | `lightbridge-agents` | `lightbridge-agents` |
| `AGENT_RUNNER_IMAGE` | `ghcr.io/vymalo/lightbridge-agent-runner:latest` | — |
| `AGENT_INDEXER_RUNNER_IMAGE` | — | optional per-kind override |
| `AGENT_REVIEW_RUNNER_IMAGE` | — | optional per-kind override |
| `AGENT_SERVICE_ACCOUNT` | `lightbridge-agent` | `lightbridge-agent` |
| `CONTROL_PLANE_INTERNAL_URL` | `http://lightbridge-ci-control-plane:8080` | `http://localhost:8080` |
| `AGENT_CA_SECRET` | — | optional K8s Secret with internal CA |
| `AGENT_JOB_DEADLINE_SECONDS` | `3600` | `3600` |
| `AGENT_CONFIG_CONFIGMAP` | — | `lightbridge-agent-config` |

### Agent runner

| Var | Required | Default | Local value |
|-----|----------|---------|--------------|
| `TASK_ID` | yes | — | UUID from the database |
| `CONTROL_PLANE_URL` | yes | — | `http://localhost:8080` |
| `AGENT_RUNNER_TOKEN` | yes | — | `dev-runner-token` |
| `WORKDIR` | no | `/workspace` | — |
| `EMBEDDINGS_BASE_URL` | yes | — | `https://api.openai.com/v1` |
| `EMBEDDINGS_API_KEY` | yes | — | your API key |
| `EMBEDDINGS_MODEL` | yes | — | `text-embedding-3-small` |
| `EMBEDDINGS_CA_CERT` | no | — | path to CA cert |
| `EMBEDDINGS_REQUEST_TIMEOUT_SECS` | no | `30` | — |
| `LLM_BASE_URL` | review: yes | — | `https://api.openai.com/v1` |
| `LLM_API_KEY` | review: yes | — | your API key |
| `LLM_MODEL` | review: yes | — | `gpt-4o` |
| `LLM_STREAM` | no | `0` | `1` |
| `LLM_REQUEST_TIMEOUT_SECS` | no | `180` | — |
| `LLM_MAX_RETRIES` | no | `3` | — |
| `LLM_CIRCUIT_BREAKER_THRESHOLD` | no | `5` | — |
| `LLM_MAX_TURNS` | no | `20` | — |
| `LLM_MAX_BATCH_SIZE` | no | `10` | — |
| `LLM_MAX_FILES_READ` | no | `50` | — |
| `LLM_MAX_SEARCHES` | no | `20` | — |
| `LLM_MAX_BATCHES` | no | `5` | — |
| `LLM_CONTEXT_WINDOW` | no | — | `128000` |
| `REVIEW_SYSTEM_PROMPT` | no | — | inline prompt (or use file) |
| `SAST_ENABLED` | no | unset | `1` |
| `SAST_BIN` | no | `opengrep` | — |
| `SAST_RULES` | no | `/opt/opengrep-rules` | path to rules dir |
| `SAST_MIN_SEVERITY` | no | `error` | `error` / `WARNING` / `INFO` |
| `SAST_MAX_FINDINGS` | no | `25` | — |
| `SAST_TIMEOUT_SECS` | no | `300` | — |
| `AGENT_CONFIG` | no | `/etc/lightbridge/agent.json` | path to JSON config |

### Web app

| Var | Local value |
|-----|-------------|
| `NEXTAUTH_URL` | `http://localhost:3000` |
| `OIDC_ISSUER` | `http://localhost:8081/realms/lightbridge` |
| `OIDC_AUDIENCE` | `lightbridge-api` |
| `OIDC_CLIENT_ID` | `lightbridge-web` |
| `CONTROL_PLANE_URL` | `http://localhost:8080` |
| `GITHUB_APP_INSTALL_URL` | GitHub App install page URL |
| `PERMISSIONS_CLAIM` | `permissions` |

---

## 11. Troubleshooting

### Control plane

| Symptom | Cause | Fix |
|---------|-------|-----|
| Fails readiness | Missing `OIDC_ISSUER` (and `DATABASE_URL` unless `ALLOW_NO_DB=1`) | Set the env vars |
| `/internal/tasks/{id}/graph` returns 503 | `NEO4J_URI` unset | Set `NEO4J_URI` (structural indexing is skipped, non-fatal) |
| Webhook 401 | Wrong `GITHUB_WEBHOOK_SECRET` | Recompute HMAC over the exact body |
| Webhook 400 | Missing `X-GitHub-Delivery` header | Ensure GitHub is sending the delivery ID |
| Webhook 202 duplicate | Replayed delivery ID | Normal — dedup is working |
| API 403 after login | Token lacks `permissions` claim | Add permissions via Keycloak client scope |

### Agent runner

| Symptom | Cause | Fix |
|---------|-------|-----|
| Fails to start | Missing `EMBEDDINGS_*` env vars | All three are required (no defaults) |
| Index only, no review | Missing `LLM_*` env vars | Add `LLM_BASE_URL`, `LLM_API_KEY`, `LLM_MODEL` |
| Clone fails | Repo not accessible | Ensure the repo is public or the runner has credentials |
| Graphify fails | Python/Graphify not installed | Non-fatal — semantic index still lands; use the `indexer` image for full Graphify support |

### GitLab

| Symptom | Cause | Fix |
|---------|-------|-----|
| GitLab not ready | Takes 3–5 min to start | `docker exec gitlab gitlab-ctl status` |
| Webhook 404 | `POST /gitlab/webhook` route doesn't exist | This is expected — GitLab integration is not yet implemented |
| Can't push to GitLab | Authentication | Use `root` / `<password>` or set up SSH keys |

### Docker compose

| Symptom | Cause | Fix |
|---------|-------|-----|
| Port conflict | Another service using 5432/7474/7687/8081 | Stop the conflicting service or remap ports |
| Keycloak realm not imported | Volume mount path wrong | Ensure `deploy/keycloak/realm-lightbridge.json` exists |
| pgvector extension missing | Wrong Postgres image | Must be `pgvector/pgvector:pg17`, not stock `postgres` |