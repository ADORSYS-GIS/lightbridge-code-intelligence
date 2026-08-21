# Configuration & environment variable reference

The single, central listing of every environment variable read anywhere in this repo — what it
does, its default, and whether it's a secret. Use this when wiring a new environment (Helm values,
a local `.env`, a CI job) or when tracking down why a knob isn't taking effect.

**File config wins, env is the fallback.** The control plane and agent runner each load one JSON
file (`control-plane.json` / `agent.json`, mounted from a Helm ConfigMap) via the shared
[`lci-config`](../services/config/README.md) loader, with `{env:NAME}` / `{env:NAME:-fallback}`
substitution inside it. Most of the "tunable" env vars below are really the **legacy/no-file
fallback** for a field the JSON config can also set — the file wins when present; an absent file
(or field) falls back to the env var, then to the built-in default listed here. Secrets keep flowing
through plain env / `secretKeyRef`, never through the file. See
[docs/kubernetes-deployment.md](../docs/kubernetes-deployment.md) for how the pieces are mounted in
the cluster, and [`compose.yaml`](../compose.yaml) for the local dev stack.

🔒 marks a secret/credential — never commit a real value, and prefer a Secret / `secretKeyRef` /
ExternalSecret over a plain ConfigMap for it.

## Contents

- [Local development (`compose.yaml`)](#local-development-composeyaml)
- [Control plane](#control-plane-servicescontrol-plane)
- [Agent runner (per-task Job)](#agent-runner-servicesagent-runner)
- [review-mcp (child MCP server)](#review-mcp-servicesreview-mcp)
- [Web console (`apps/web`)](#web-console-appsweb)
- [Shared observability](#shared-observability-servicesobservability)
- [opencode plugins](#opencode-plugins-integrationsopencode)
- [`lci` CLI client](#lci-cli-client-clientslci)
- [Notes and known gaps](#notes-and-known-gaps)

---

## Local development (`compose.yaml`)

Fixed dev-only credentials for the local data plane (`just up`) — not meant to be overridden, and
never used in a deployed environment.

| Variable | Default | Description |
|---|---|---|
| `POSTGRES_USER` / `POSTGRES_PASSWORD` / `POSTGRES_DB` 🔒 | `lightbridge` / `lightbridge` / `lightbridge` | Local Postgres+pgvector container credentials. |
| `NEO4J_AUTH` 🔒 | `neo4j/lightbridge` | Local Neo4j container credentials. |
| `KC_BOOTSTRAP_ADMIN_USERNAME` / `KC_BOOTSTRAP_ADMIN_PASSWORD` 🔒 | `admin` / `admin` | Local Keycloak admin bootstrap account. |
| `KC_HTTP_PORT` | `8080` | Keycloak's internal HTTP port (mapped to host `8081`). |

---

## Control plane (`services/control-plane`)

One binary, several roles selected by `CONTROL_PLANE_ROLE` — see
[the control-plane README](../services/control-plane/README.md#roles).

### Core wiring

| Variable | Default | Description |
|---|---|---|
| `CONTROL_PLANE_ROLE` | `serve` | Which of the 8 roles this process runs (`serve`, `dispatcher`, `reconciler`/`poller`, `a2a`, `notifier`, `replay`, `mcp`, `mint-runner-token`). |
| `CONTROL_PLANE_CONFIG` | `/etc/lightbridge/control-plane.json` | Path to the mounted JSON file config. |
| `BIND_ADDR` | `0.0.0.0:8080` | HTTP bind address for the `serve` role. |
| `METRICS_ADDR` | `0.0.0.0:9090` | Bind address for the headless `/metrics` + `/healthz` listener (`dispatcher`/`reconciler`/`notifier`/`replay`). |
| `HOSTNAME` | *(pod name)* | Queue-lease owner id for the `dispatcher`/`notifier` roles. |
| `DATABASE_URL` 🔒 | *(unset → dev no-DB mode)* | Postgres connection string. |
| `ALLOW_NO_DB` | `false` | Dev-only opt-in to run without `DATABASE_URL` (in-memory dedupe, single replica). |

### OIDC / auth

| Variable | Default | Description |
|---|---|---|
| `OIDC_ISSUER` | *(unset → auth disabled, fails closed)* | OIDC issuer URL (Keycloak realm). |
| `OIDC_AUDIENCE` | `account` | Expected JWT audience. |
| `OIDC_JWKS_URI` | `{issuer}/protocol/openid-connect/certs` | JWKS endpoint override. |
| `PERMISSIONS_CLAIM` | `permissions` | Dotted JWT claim path the caller's permission list is read from. |
| `RUNNER_TOKEN_SIGNING_KEY` 🔒 | *(unset → internal API fails closed, 503)* | HMAC key that mints/verifies per-task runner JWTs ([ADR-0092](../docs/adr/0092-per-task-runner-tokens.md)); never leaves this process. |

### GitHub App

| Variable | Default | Description |
|---|---|---|
| `GITHUB_APP_ID` | *(unset → GitHub App disabled)* | GitHub App id. |
| `GITHUB_APP_PRIVATE_KEY` 🔒 | *(unset → disabled)* | GitHub App RSA private key (PEM). |
| `GITHUB_WEBHOOK_SECRET` 🔒 | `""` (empty → GitHub webhooks disabled) | HMAC secret verifying `X-Hub-Signature-256`. |
| `GITHUB_APP_HANDLE` | `lightbridge-assistant` | Bot handle; `@handle` in a PR comment triggers a deep review. |

> GitLab and Bitbucket are **config-file only** (`control-plane.json`'s `gitlab`/`bitbucket`
> sections, per-project credentials) — there is intentionally no `GITLAB_*` or `BITBUCKET_*` env
> fallback. See [docs/kubernetes-deployment.md](../docs/kubernetes-deployment.md#gitlab-configuration-adr-0072).

### Neo4j

| Variable | Default | Description |
|---|---|---|
| `NEO4J_URI` | *(unset → graph disabled, 503 on ingest)* | Bolt connection URI. |
| `NEO4J_USER` | `neo4j` | Neo4j username. |
| `NEO4J_PASSWORD` 🔒 | `""` | Neo4j password. |

### Agent-Job dispatch (`dispatcher` role)

| Variable | Default | Description |
|---|---|---|
| `AGENT_NAMESPACE` | `lightbridge-agents` | Kubernetes namespace the per-task Jobs run in. |
| `AGENT_RUNNER_IMAGE` | `ghcr.io/vymalo/lightbridge-agent-runner:latest` | Shared runner image. |
| `AGENT_INDEXER_RUNNER_IMAGE` | *(falls back to the shared image)* | Override image for `index` Jobs. |
| `AGENT_REVIEW_RUNNER_IMAGE` | *(falls back to the shared image)* | Override image for review/ask Jobs. |
| `AGENT_SERVICE_ACCOUNT` | `lightbridge-agent` | ServiceAccount the Job runs as. |
| `CONTROL_PLANE_INTERNAL_URL` | `http://lightbridge-ci-control-plane:8080` | In-cluster URL the dispatcher tells each Job to call back on. |
| `AGENT_CA_SECRET` | *(unset → no CA mount)* | Secret name holding the internal CA cert mounted into the Job. |
| `AGENT_JOB_DEADLINE_SECONDS` | `3600` | Job's `activeDeadlineSeconds` hard runtime cap; also sizes the minted runner-token TTL. |
| `REVIEW_SYSTEM_PROMPT` | *(unset — see agent-runner)* | Reviewer system prompt, passed through to the Job. Prefer mounting a template via `config_configmap` instead. |
| `AGENT_CONFIG_CONFIGMAP` | *(unset → no ConfigMap mount)* | ConfigMap mounted at `/etc/lightbridge` in the Job, carrying `agent.json` + prompt templates. |
| `MCP_PUBLIC_URL` | *(unset → RFC 9728 metadata not served)* | Externally reachable base URL of the `mcp` role. |

**Task-Job env** — set *by* the dispatcher *into* every agent Job (read back by `agent-runner`,
see below): `TASK_ID`, `REPOSITORY_ID`, `INSTALLATION_ID`, `COMMAND`, `TARGET_TYPE`, `TARGET_ID`,
`ATTEMPT`, `BASE_SHA` / `HEAD_SHA`, `CONTROL_PLANE_URL`, `AGENT_RUNNER_TOKEN` 🔒 (a fresh per-task
signed JWT), `TRACEPARENT`, plus (from Secret `lightbridge-agent-secrets`) `LLM_BASE_URL` /
`LLM_API_KEY` 🔒 / `LLM_MODEL` (optional — absent skips review) and `EMBEDDINGS_BASE_URL` /
`EMBEDDINGS_API_KEY` 🔒 / `EMBEDDINGS_MODEL` (required), and the forwarded indexer-tuning vars
(`INDEX_EMBED_BATCH_SIZE`, `INDEX_MAX_CHUNK_LINES`, `INDEX_WINDOW_SIZE`, `INDEX_WINDOW_STEP`) and
observability vars (`OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_TRACES_SAMPLER_ARG`) from the dispatcher's
own env.

### A2A role ([RFC-0006](../docs/rfc/0006-a2a-agent-surface.md))

| Variable | Default | Description |
|---|---|---|
| `A2A_BIND` | `0.0.0.0:8080` | HTTP bind address for the `a2a` role. |
| `A2A_BASE_URL` | `http://localhost:8080` | Externally reachable base URL advertised in the A2A agent card. |
| `A2A_QUOTA_MAX` | `20` | Per-identity deep-run submissions allowed per window (clamped ≥1). |
| `A2A_QUOTA_WINDOW_SECS` | `3600` | Quota window length, seconds (clamped ≥1). |
| `A2A_PUSH_TOKEN_KEY` 🔒 | *(unset → tokened push webhooks refused)* | Base64 32-byte key encrypting stored webhook auth tokens ([ADR-0079](../docs/adr/0079-a2a-push-notifications-webhook-egress.md) §3). |
| `A2A_MAX_PUSH_CONFIGS_PER_TASK` | *(built-in default, clamped ≥1)* | Max push-notification configs per task. |
| `A2A_MAX_STREAMS_PER_CALLER` | *(built-in default, clamped ≥1)* | Per-caller concurrent SSE stream cap. |
| `A2A_PUSH_DENIED_CIDRS` | `""` | Extra comma-separated CIDRs the SSRF policy blocks for webhook push delivery, on top of the fixed ranges. |
| `A2A_TASK_TTL_DAYS` | `30` | Retention (days) for terminal `a2a_tasks` rows before GC. |
| `A2A_TASK_SWEEP_BATCH` | `500` | Max rows deleted per GC tick. |

### MCP role

| Variable | Default | Description |
|---|---|---|
| `MCP_BIND` | `0.0.0.0:8080` | HTTP bind address for the `mcp` role. |
| `MCP_QUOTA_MAX` | `20` | Per-caller MCP request quota. |
| `MCP_QUOTA_WINDOW_SECS` | `3600` | Quota window, seconds. |

### Notifier role ([ADR-0079](../docs/adr/0079-a2a-push-notifications-webhook-egress.md))

| Variable | Default | Description |
|---|---|---|
| `NOTIFIER_POLL_SECS` | `3` | Webhook-delivery poll cadence. |
| `NOTIFIER_LEASE_SECS` | `60` | Delivery-claim lease. |
| `NOTIFIER_MAX_ATTEMPTS` | `8` | Max delivery retry attempts. |
| `NOTIFIER_MAX_EVENTS_PER_CLAIM` | `50` | Max events claimed per batch. |

### Reconciler / poller role

| Variable | Default | Description |
|---|---|---|
| `RECONCILER_INTERVAL_SECS` (legacy `POLLER_INTERVAL_SECS`) | `300` | Reconciler drain/poll interval. |
| `RECONCILER_WINDOW_DAYS` (legacy `POLLER_WINDOW_DAYS`) | `14` (clamped ≥1) | "Completed within last N days" window for the feedback poll. |

### Durable-step replay role ([ADR-0087](../docs/adr/0087-durable-replay-checkpoint-runtime.md))

| Variable | Default | Description |
|---|---|---|
| `DURABLE_STEP_RETENTION` | `21600` (6h) | TTL, seconds, for the `durable_step` journal sweep. Must be > 0. |

---

## Agent runner (`services/agent-runner`)

The per-task Job binary — see [the agent-runner README](../services/agent-runner/README.md).

### Bootstrap

| Variable | Default | Description |
|---|---|---|
| `TASK_ID` | *(required)* | Which task this Job instance is running. |
| `CONTROL_PLANE_URL` | *(required)* | Where the runner calls back. |
| `AGENT_RUNNER_TOKEN` 🔒 | *(required)* | Bearer credential for the internal API. |
| `WORKDIR` | `/workspace` | Clone directory. |
| `AGENT_CONFIG` | `/etc/lightbridge/agent.json` | Path to the mounted JSON file config (embeddings/review/sast blocks). |

### Embeddings ([ADR-0018](../docs/adr/0018-openai-compatible-embeddings.md))

| Variable | Default | Description |
|---|---|---|
| `EMBEDDINGS_BASE_URL` | *(required)* | OpenAI-compatible embeddings endpoint. |
| `EMBEDDINGS_API_KEY` 🔒 | *(required)* | Bearer key. |
| `EMBEDDINGS_MODEL` | *(required)* | Embedding model id — changing dimensionality needs a migration. |
| `EMBEDDINGS_REQUEST_TIMEOUT_SECS` | `180` | Per-request timeout. |
| `EMBEDDINGS_CA_CERT` | *(unset → default TLS roots)* | Extra CA PEM trusted for the embeddings HTTPS client. |

### Review LLM ([ADR-0026](../docs/adr/0026-native-review-agent.md), [ADR-0037](../docs/adr/0037-agent-acts-via-mediated-tools.md), [ADR-0039](../docs/adr/0039-agent-llm-resilience-and-observability.md), [ADR-0042](../docs/adr/0042-risk-first-review-and-parallel-batching.md))

| Variable | Default | Description |
|---|---|---|
| `LLM_MODEL` | *(unset → review disabled, indexing-only)* | Chat model id. |
| `LLM_BASE_URL` | *(required if `LLM_MODEL` set)* | OpenAI-compatible Chat Completions endpoint. |
| `LLM_API_KEY` 🔒 | *(required if `LLM_MODEL` set)* | Gateway bearer key. |
| `REVIEW_SYSTEM_PROMPT` | *(no built-in default — fails closed per ADR-0037)* | The reviewer's persona/guidance prompt. |
| `LLM_MAX_TURNS` | `40` | Ceiling on model turns per review (clamped ≥1). |
| `LLM_MAX_BATCH_SIZE` | `8` | Max concurrent read-only tool calls per turn (clamped ≥1). |
| `LLM_MAX_FILES_READ` | `30` | Cumulative `read_file` budget (clamped ≥1). |
| `LLM_MAX_SEARCHES` | `15` | Cumulative retrieval-call budget (clamped ≥1). |
| `LLM_MAX_BATCHES` | `6` | Cumulative investigation-round budget (clamped ≥1). |
| `LLM_MAX_COVERAGE_BOUNCES` | `3` | Coverage-gate bounce cap; `0` disables the bounce (not clamped). |
| `LLM_MAX_CYCLES` | `8` | OpenCode-path re-prompt ceiling (clamped ≥1). |
| `LLM_CONTEXT_WINDOW` | *(unset → no budgeting)* | Model context window in tokens; `0` treated as unset. |
| `LLM_STREAM` | `false` | Enable SSE streaming of chat responses (`1` = on). |
| `LLM_REQUEST_TIMEOUT_SECS` | `180` | Per chat round-trip timeout. |
| `LLM_MAX_RETRIES` | `2` | Retries on a transient turn failure. |
| `LLM_CIRCUIT_BREAKER_THRESHOLD` | `3` | Consecutive-failure circuit-breaker trip count. |
| `LLM_CA_CERT` | *(falls back to `EMBEDDINGS_CA_CERT`)* | Extra CA PEM for the chat HTTP client. |

### SAST ([ADR-0061](../docs/adr/0061-sast-deterministic-finding-source.md), [ADR-0073](../docs/adr/0073-sast-as-agent-tool.md))

| Variable | Default | Description |
|---|---|---|
| `SAST_ENABLED` | `false` | Turns on the opengrep pass (opt-in). |
| `SAST_BIN` | `opengrep` | opengrep binary name/path. |
| `SAST_RULES` | `/opt/opengrep-rules` | `--config` value (vendored ruleset dir). |
| `SAST_MIN_SEVERITY` | `error` | Minimum SARIF level surfaced. |
| `SAST_MAX_FINDINGS` | `25` (clamped ≥1) | Cap on findings posted per review. |
| `SAST_TIMEOUT_SECS` | `300` (clamped ≥1) | Wall-clock ceiling on one scan. |

`LCI_MCP_SAST_*` (`LCI_MCP_SAST_BIN`, `LCI_MCP_SAST_RULES`, `LCI_MCP_SAST_MIN_SEVERITY`,
`LCI_MCP_SAST_MAX_FINDINGS`, `LCI_MCP_SAST_TIMEOUT_SECS`) mirror the `SAST_*` block above one-for-one
— the runner serializes the resolved config into the spawned `opencode`/`review-mcp` child's env
rather than re-deriving it there. `LCI_MCP_SAST_CHANGED_FILES` (presence = "SAST offered this run")
points at the newline-delimited changed-file list scoping the scan.

### Indexer tuning ([ADR-0010](../docs/adr/0010-graphify-treesitter-indexing-baseline.md))

| Variable | Default | Description |
|---|---|---|
| `INDEX_EMBED_BATCH_SIZE` | `32` (clamped ≥1) | Chunks embedded + submitted per round-trip. |
| `INDEX_MAX_CHUNK_LINES` | `150` (clamped ≥1) | Max lines a structured chunk spans before windowing. |
| `INDEX_WINDOW_SIZE` | `100` (clamped ≥1) | Windowed-fallback window size (lines). |
| `INDEX_WINDOW_STEP` | `50` (clamped ≥1) | Windowed-fallback step (lines). |
| `INDEX_MAX_CHUNK_BYTES` | `16000` (clamped ≥1) | Ceiling on a single chunk's byte length.¹ |

### Review-loop runtime

| Variable | Default | Description |
|---|---|---|
| `LCI_DURABLE_REPLAY` | `false` | Opt in to the durable `CheckpointRuntime` ([ADR-0087](../docs/adr/0087-durable-replay-checkpoint-runtime.md)) instead of the default passthrough runtime. |
| `TRACEPARENT` | *(unset → fresh root span)* | W3C trace-context to re-parent the runner's root span under. |
| `REASONING_LOG_CHARS` | `4000` (`0` = unbounded) | Per-turn "agent reasoning" log-line cap. |
| `CONTENT_LOG_CHARS` | `4000` (`0` = unbounded) | Per-turn "agent content" (visible answer) log-line cap. |
| `OPENCODE_BIN` | `opencode` | Path/name of the opencode binary to spawn. |
| `GITHUB_PERSONAL_ACCESS_TOKEN` 🔒 | *(set only when the GitHub MCP tool is offered, ADR-0105)* | GitHub MCP credential injected into the opencode child's env. |

### Status API (opt-in)

| Variable | Default | Description |
|---|---|---|
| `LCI_STATUS_API` | `false` | Enables the read-only `/status` HTTP server. |
| `LCI_STATUS_PORT` | `8091` | Port for the status server. |
| `LCI_STATUS_BIND` | `127.0.0.1` | Bind IP. |
| `LCI_STATUS_TOKEN` 🔒 | *(falls back to `AGENT_RUNNER_TOKEN`)* | Dedicated bearer token for `GET /status`. |

---

## review-mcp (`services/review-mcp`)

`lci-review-mcp` is a stdio MCP server spawned as a child of the agent-runner Job; all of its env is
set by the supervisor per task, not by the operator directly.

| Variable | Description |
|---|---|
| `LCI_MCP_CP_URL` | Control-plane internal API base. |
| `LCI_MCP_RUNNER_TOKEN` 🔒 | Bearer for the internal API. |
| `LCI_MCP_EMBED_URL` / `LCI_MCP_EMBED_KEY` 🔒 / `LCI_MCP_EMBED_MODEL` | Embeddings endpoint, key, and model. |
| `LCI_MCP_TASK_ID` | Task UUID this MCP server operates on. |
| `LCI_MCP_CHECKOUT` | Path to the repo checkout. |
| `LCI_MCP_MIN_PRIORITY` | Repo's `severity.min` filter ([ADR-0030](../docs/adr/0030-repo-review-config.md)) — findings below it are ack'd but not sent to the control plane. |

---

## Web console (`apps/web`)

See [`apps/web/.env.example`](../apps/web/.env.example) for the copy-to-`.env.local` template.

| Variable | Default | Description |
|---|---|---|
| `OIDC_ISSUER` | *(required)* | OIDC issuer URL. |
| `OIDC_CLIENT_ID` | *(required)* | OAuth client id (pre-seeded in [`deploy/keycloak/realm-lightbridge.json`](keycloak/realm-lightbridge.json) for local dev). |
| `OIDC_CLIENT_SECRET` 🔒 | *(unset → public/PKCE client)* | Confidential-client secret. |
| `OIDC_REDIRECT_URI` | `http://localhost:3000/api/auth/callback` | OAuth redirect URI; also used to derive the app's public origin. |
| `OIDC_POST_LOGOUT_REDIRECT_URI` | `http://localhost:3000` | Post-logout redirect. |
| `OIDC_SCOPE` | `openid profile email` | OAuth scopes. |
| `OIDC_JWKS_URI` | *(derived from issuer)* | JWKS endpoint override. |
| `OIDC_TOKEN_URI` | *(derived from issuer)* | Token endpoint override (refresh grants). |
| `OIDC_AUDIENCE` | *(optional)* | Expected JWT audience. |
| `CONTROL_PLANE_URL` (preferred) / `AUTH_BACKEND_URL` (Helm-set legacy name) | `http://localhost:8080/api/v2` | Control-plane base URL for server-side calls; **must include** the `/api/v2` prefix ([ADR-0109](../docs/adr/0109-api-v2-route-versioning.md)). |
| `PERMISSIONS_CLAIM` | `permissions` | Mirrors the control plane's claim path. |
| `GITHUB_APP_INSTALL_URL` | `https://github.com/apps/lightbridge-assistant` | GitHub App install-link CTA target. |
| `AGENT_NAMESPACE` | `lightbridge-agents` | Mirrors the control plane's namespace, used to render `kubectl` snippets. |
| `NEXT_PUBLIC_GRAFANA_URL` | *(unset → falls back to a `kubectl logs` snippet)* | **Client-visible** Grafana base URL for the embedded Loki run-logs panel. Requires `allow_embedding = true` on the Grafana instance (Epic #459). |
| `NODE_ENV` | *(standard Next.js)* | Gates the session cookie's `secure` flag (`production`). |

---

## Shared observability (`services/observability`)

Used by both the control plane and the agent runner.

| Variable | Default | Description |
|---|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | *(unset → fmt-only logging, no trace export)* | OTLP/HTTP collector base URL (Tempo). |
| `OTEL_TRACES_SAMPLER_ARG` | `0.1` | Trace-ID-ratio sampler argument. |
| `RUST_LOG` | `info` | Standard `tracing_subscriber::EnvFilter` log-level filter. |

---

## opencode plugins (`integrations/opencode`)

TypeScript plugins loaded into the `opencode` subprocess the agent runner spawns; env is set by the
runner (`opencode.rs`) and read here.

| Variable | Default | Description |
|---|---|---|
| `LCI_RECORDER_PATH` | `.lightbridge/recording.jsonl` | JSONL path the recorder plugin writes the tool-call trace to. |
| `LCI_SENTINEL_MARKER_PATH` | *(next to the recorder path)* | Fatal-situation marker file path. |
| `LCI_SENTINEL_TERMINAL_TOOLS` | `lightbridge_finish,lightbridge_abort` | Comma-separated terminal tool names. |
| `LCI_GATE_TERMINAL_TOOL` | `lightbridge_submit_findings` | Tool the gate-interlock plugin holds back until prerequisites are met. |
| `LCI_GATE_REQUIRED_TOOLS` | `lightbridge_refute_finding` | Comma-separated tools that must run first. |
| `LCI_GATE_MIN_CALLS` | `1` | Minimum completed calls per required tool. |
| `LCI_LOG_LEVEL` | `info` | Logger plugin's threshold (`error`/`warn`/`info`/`debug`). |
| `LCI_LOG_SERVICE` | `lci-opencode` | Service name tag on log lines. |
| `LCI_LOG_REASONING_CHARS` / `LCI_LOG_CONTENT_CHARS` / `LCI_LOG_TOOL_ARGS_CHARS` / `LCI_LOG_TOOL_OUTPUT_CHARS` | `4000` each (`0` = unbounded) | Per-field log-line character caps. |
| `NODE_EXTRA_CA_CERTS` | *(set from `EMBEDDINGS_CA_CERT` when present)* | bun/opencode's own CA-trust variable for the embeddings gateway's HTTPS. |
| `OPENCODE_CONFIG` / `OPENCODE_CONFIG_DIR` | *(set by the supervisor)* | opencode's own config-file / config-dir variables. |
| `OPENCODE_DISABLE_AUTOUPDATE` / `OPENCODE_DISABLE_MODELS_FETCH` | `1` | Hardening flags set on the spawned opencode child. |
| `LCI_PROBE_TIMEOUT_MS` | `180000` | ACP fidelity-probe timeout. |
| `LCI_PROBE_MARKER` | *(unset by default)* | Marker path for the probe MCP server. |

Simulation-only, test harness (never runs in production): `LCI_SIM_PROVIDER_PORT` (`8899`),
`LCI_SIM_PROVIDER_LOG`, `LCI_SIM_MCP_LOG`, `LCI_SIM_TOOLS_LOG`, `LCI_SIM_MSG_LOG`,
`LCI_SIM_NEVER_FINISH`.

---

## `lci` CLI client (`clients/lci`)

A standalone TUI/CLI client, separate from the deployed platform.

| Variable | Default | Description |
|---|---|---|
| `CONTROL_PLANE_URL` | `https://code-intelligence-api.ai.camer.digital/api/v2` | Control-plane base URL. |
| `OIDC_ISSUER` | `https://auth.verif.fyi/realms/camer-digital` | OIDC issuer. |
| `OIDC_CLIENT_ID` | `lightbridge-cli` | Public OAuth client id. |
| `LCI_REDIRECT_PORT` | `8765` | Loopback OAuth redirect port. |
| `LCI_THEME` | `midnight` | CLI color theme. |

---

## Notes and known gaps

- ¹ `INDEX_MAX_CHUNK_BYTES` is read by the runner but, unlike its four siblings
  (`INDEX_EMBED_BATCH_SIZE`/`INDEX_MAX_CHUNK_LINES`/`INDEX_WINDOW_SIZE`/`INDEX_WINDOW_STEP`), is not
  in the dispatcher's forwarding list in
  [`k8s.rs`](../services/control-plane/src/integrations/k8s.rs) — so an operator override never
  reaches the Job. Worth a fix or an explicit "intentional" note if you rely on it.
- No committed JSON config file in this repo uses `{env:NAME:-default}` templating today — that
  pattern is applied by the operator-managed values in the sibling `ai-helm-values` repo, not
  checked in here. The Rust-side defaults in this doc are the authoritative fallback regardless of
  which layer resolves the value.
- GitLab and Bitbucket credentials are **config-file only** — see the note under
  [Control plane → GitHub App](#github-app) above. Don't set `GITLAB_*` / `BITBUCKET_*` env vars
  expecting them to be read; they aren't.
