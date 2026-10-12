# Epic #492 — Unify Service Domain under `code-intelligence-api.ai.camer.digital`

**Status:** Open · **Assignee:** @leghadjeu-christian · **Label:** `epic`
**Created:** 2026-07-26 · **Sub-issues:** 6 open, 0 completed (0%)
**Source of truth:** [ADR-0109](docs/adr/) · Fulfills RFC-0007

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Problem Statement — Current State](#2-problem-statement--current-state)
3. [Desired Outcome — Target Architecture](#3-desired-outcome--target-architecture)
4. [Scope](#4-scope)
5. [Child Stories](#5-child-stories)
   - [#506 — Migrate every route to /api/v2](#506--migrate-every-route-to-apiv2)
   - [#507 — Path-scoped multi-forge webhook routes](#507--path-scoped-multi-forge-webhook-routes)
   - [#508 — Expose review-mcp externally at /mcp](#508--expose-review-mcp-externally-at-mcp)
   - [#509 — Fold A2A ingress into the unified router at /a2a](#509--fold-a2a-ingress-into-the-unified-router-at-a2a)
   - [#510 — ai-helm/ai-helm-values Ingress/DNS/TLS companion](#510--ai-helmai-helm-values-ingressdnstls-companion)
   - [#511 — Fix the dangling ADR-0098 roadmap reference](#511--fix-the-dangling-adr-0098-roadmap-reference)
6. [Implementation Plan](#6-implementation-plan)
7. [Risk Register](#7-risk-register)
8. [Success Metrics](#8-success-metrics)
9. [Dependencies & Related Epics](#9-dependencies--related-epics)

---

## 1. Executive Summary

Epic #492 consolidates every external surface of the Lightbridge Code Intelligence control plane
under a single domain (`code-intelligence-api.ai.camer.digital`) and a uniform path scheme. The
work is the concrete realisation of RFC-0007's already-accepted "one binary hosts serve/a2a/mcp"
direction.

The epic produces three first-class protocol surfaces at distinct paths:

| Path prefix | Protocol | Description |
|---|---|---|
| `/api/v2` | REST/OIDC | All existing dashboard, runner-internal, and admin routes — versioned |
| `/api/v2/webhook/{github,gitlab/<id>,bitbucket/<id>}` | HTTPS POST | Path-routed, per-forge webhook ingress |
| `/a2a` | A2A JSON-RPC + REST | Agent-to-agent task protocol, folded from the standalone module |
| `/mcp` | HTTP/SSE | External MCP tool server, converted from stdio-only |

It also formally retires two superseded topology plans (closed Epic #294 — Restate, closed
Epic #295 — A2A own-Ingress) and fixes a dangling `ADR-0098` label in `ROADMAP.md`.

ADR-0109 is the architectural source of truth for this epic. It was merged via PR #520 on
2026-07-26.

---

## 2. Problem Statement — Current State

### 2.1 Flat, unversioned route tree

Every route in `services/control-plane/src/main.rs` is registered at root with no `/api/` or
`/v2/` prefix. Three very different auth models share the same unversioned namespace:

```rust
// services/control-plane/src/main.rs  (current state, lines 285-393)
fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz",   get(liveness))
        .route("/readyz",    get(readiness))
        .route("/metrics",   get(metrics_endpoint))
        // — OIDC-gated dashboard routes —
        .route("/me",          get(jwt::me))
        .route("/tasks",       get(tasks::list))
        .route("/tasks/{id}",  get(tasks::get))
        .route("/repositories", get(tasks::list_repositories))
        // — Admin routes (admin-realm role) —
        .route("/admin/repositories",           get(admin::list_repositories))
        .route("/admin/repositories/{id}/approve", post(admin::approve))
        // — Runner-internal routes (shared-bearer) —
        .route("/internal/tasks/{id}",         get(internal::get_context))
        .route("/internal/tasks/{id}/status",  post(internal::set_status))
        // ... 15+ more flat routes ...
        .with_state(state)
}
```

### 2.2 Legacy webhook alias still alive

The unified `/webhook` route (header-sniffed platform detection) coexists with a legacy
`/github/webhook` alias:

```rust
// Unified webhook route — detects the platform (GitHub/GitLab) from headers.
.route(
    "/webhook",
    post(webhook::webhook_router).layer(DefaultBodyLimit::max(webhook::MAX_BODY_BYTES)),
)
// Legacy GitHub webhook route — forwards to the unified handler. Kept during the
// transition so existing webhook configurations don't break.
.route(
    "/github/webhook",
    post(webhook::github_webhook_legacy).layer(...),
)
```

There is no GitLab-specific path and no Bitbucket path at all.

### 2.3 A2A serves at `/` from its own role, with a dead separate-Ingress plan

The A2A module runs as a separate `a2a` binary role, starting its own Axum listener on
`A2A_BIND` (default `0.0.0.0:8080`), advertising its base URL via `base_url_from_env()`.
Its module doc comment still carries a dead plan:

```rust
// services/control-plane/src/a2a/mod.rs, line 18
//! The role's k8s Deployment/Ingress is an ai-helm follow-up; this module adds the role + logic.
```

Epic #295 (A2A own-Ingress) was closed as superseded — but the code assumption persists.

### 2.4 `review-mcp` is stdio-only and unreachable externally

`services/review-mcp` exposes the code graph/retrieval tool surface (`read_file`, `graph`,
`vector search`, `run_sast`) over stdio only, as an internal child process of the OpenCode
review agent. No external MCP client can reach it.

### 2.5 Dangling `ADR-0098` reference in ROADMAP.md

`ROADMAP.md`'s "In progress" section cites "ADR-0098" for A2A per-finding streaming — but
`docs/adr/` jumps from `0097` to `0099`. The label points at open PR #458, not a real ADR file.

---

## 3. Desired Outcome — Target Architecture

### 3.1 Unified router structure (target)

One control-plane binary, one Ingress, one domain. All path segments and the domain itself are
configurable, not hardcoded literals.

```
code-intelligence-api.ai.camer.digital
│
├── /a2a                         ← A2A JSON-RPC + REST (was: standalone role at /)
│   └── /.well-known/agent-card.json
│
├── /mcp                         ← review-mcp over HTTP/SSE (was: stdio-only)
│
└── /api/v2                      ← all versioned REST
    ├── /me
    ├── /tasks/**
    ├── /repositories/**
    ├── /admin/**
    ├── /internal/**
    └── /webhook
        ├── /github              ← path-routed, no header-sniffing
        ├── /gitlab/<install-id> ← per-installation path
        └── /bitbucket/<install-id>
```

### 3.2 Target `app()` skeleton

```rust
// services/control-plane/src/main.rs (target state after #506/#507/#508/#509)
fn app(state: AppState) -> Router {
    let api_v2 = Router::new()
        .route("/me",          get(jwt::me))
        .route("/tasks",       get(tasks::list))
        .route("/tasks/{id}",  get(tasks::get))
        // ... all existing routes ...
        // webhook sub-tree
        .route("/webhook/github",
               post(webhook::github_handler).layer(body_limit()))
        .route("/webhook/gitlab/{installation_id}",
               post(webhook::gitlab_handler).layer(body_limit()))
        .route("/webhook/bitbucket/{installation_id}",
               post(webhook::bitbucket_handler).layer(body_limit()));

    let mcp = mcp_http_router(state.clone());   // new: HTTP/SSE adapter for review-mcp
    let a2a = a2a::axum_router(state.clone());  // existing logic, re-mounted

    Router::new()
        .route("/healthz", get(liveness))
        .route("/readyz",  get(readiness))
        .route("/metrics", get(metrics_endpoint))
        .nest("/api/v2", api_v2)
        .nest("/a2a",    a2a)
        .nest("/mcp",    mcp)
        .layer(axum::middleware::from_fn(track_http_metrics))
        .with_state(state)
}
```

Old paths (`/webhook`, `/github/webhook`, every flat route) are **removed** — hard cutover,
no dual-path period.

---

## 4. Scope

### In Scope

| Item | Story |
|---|---|
| Prefix every existing route under `/api/v2`; remove old flat paths (hard cutover) | #506 |
| Introduce path-scoped, per-forge webhook routes; remove `/webhook` + `/github/webhook` | #507 |
| Expose `review-mcp` over HTTP/SSE at `/mcp` with authn/authz | #508 |
| Re-mount A2A under `/a2a` in the unified router; kill standalone-Ingress plan | #509 |
| File the cross-repo Ingress/DNS/TLS ticket in `ai-helm`/`ai-helm-values` | #510 |
| Fix the dangling `ADR-0098` label in `ROADMAP.md` | #511 |

### Out of Scope

- The actual k8s Ingress/DNS/cert change — lives in `ai-helm`/`ai-helm-values` (tracked via #510
  as a cross-repo pointer only).
- Bitbucket's `CodePlatform` implementation — tracked under Epic #353 / PR #522. This epic only
  defines the path it lands on.
- Any change to the underlying MCP tool implementations themselves (ADR-0104).
- A transition window for the webhook cutover — the instruction is an explicit hard cutover.

---

## 5. Child Stories

### #506 — Migrate every route to `/api/v2`

**Status:** Open · **Label:** `user-story`

#### Intent

Every route in the flat, unversioned namespace — OIDC dashboard routes, admin routes, and
runner-internal routes — is moved under a `/api/v2` prefix. Every consumer (apps/web, `lci` CLI,
agent-runner internal client) is updated in the **same PR**. Old paths return 404 immediately.

#### Key files to change

| File | Change |
|---|---|
| `services/control-plane/src/main.rs` | Wrap all routes in `.nest("/api/v2", ...)` |
| `apps/web/lib/server/api.ts` | Update base path from `/` to `/api/v2` |
| `lci` HTTP client | Update base path |
| Agent-runner internal API client | Update all `/internal/tasks/...` call sites |

#### Acceptance Criteria

- `GET /api/v2/tasks` behaves identically to today's `GET /tasks`.
- `GET /tasks` → 404 (hard cutover).
- Every consumer updated in the same change; no broken call site ships.

#### Implementation sketch

```rust
// services/control-plane/src/main.rs
fn app(state: AppState) -> Router {
    let api_v2 = existing_routes_router(state.clone());

    Router::new()
        .route("/healthz", get(liveness))
        .route("/readyz",  get(readiness))
        .route("/metrics", get(metrics_endpoint))
        .nest("/api/v2", api_v2)   // <-- all routes versioned here
        .layer(axum::middleware::from_fn(track_http_metrics))
        .with_state(state)
}
```

```typescript
// apps/web/lib/server/api.ts  (illustrative)
const BASE = process.env.CONTROL_PLANE_URL + "/api/v2";

export const getTasks = () =>
  fetch(`${BASE}/tasks`, { headers: authHeaders() });
```

---

### #507 — Path-scoped multi-forge webhook routes

**Status:** Open · **Label:** `user-story`

#### Intent

Webhook routing becomes explicit path-based, not header-sniffed. GitHub, GitLab, and Bitbucket
each get their own path. The legacy `/webhook` and `/github/webhook` are removed (hard cutover).

#### Key files to change

| File | Change |
|---|---|
| `services/control-plane/src/http/webhook.rs` | Split `webhook_router()` into per-forge handlers |
| `services/control-plane/src/main.rs` | Register 3 distinct routes |
| External webhook configurations | Repoint every GitHub App + GitLab + Bitbucket webhook |

#### Proposed route scheme

| Platform | Path | Notes |
|---|---|---|
| GitHub | `POST /api/v2/webhook/github` | Singleton; signature verified via `GITHUB_WEBHOOK_SECRET` |
| GitLab | `POST /api/v2/webhook/gitlab/{installation_id}` | Per-installation, keyed by ID in `control-plane.json::gitlab.projects[]` |
| Bitbucket | `POST /api/v2/webhook/bitbucket/{installation_id}` | Per-installation, mirroring GitLab |

#### Acceptance Criteria

- GitHub events arriving at `/api/v2/webhook/github` route correctly (no header-sniffing needed).
- GitLab + Bitbucket events route to the correct installation via path extraction.
- The legacy `/webhook` and `/github/webhook` paths → 404 after cutover.
- Path segments are configurable (base path/domain), not literals.

#### Implementation sketch

```rust
// services/control-plane/src/http/webhook.rs (new)
pub async fn github_webhook_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    // HMAC-SHA256 signature check using state.github_webhook_secret
    verify_github_signature(&headers, &body, &state.github_webhook_secret)?;
    let event: GithubWebhookEvent = serde_json::from_slice(&body)?;
    handle_github_event(state, event).await
}

pub async fn gitlab_webhook_handler(
    State(state): State<AppState>,
    Path(installation_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let config = state.gitlab_configs.get(&installation_id)?;
    verify_gitlab_signature(&headers, &body, &config.webhook_secret)?;
    let event: GitlabWebhookEvent = serde_json::from_slice(&body)?;
    handle_gitlab_event(state, config, event).await
}

pub async fn bitbucket_webhook_handler(
    State(state): State<AppState>,
    Path(installation_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let config = state.bitbucket_configs.get(&installation_id)?;
    verify_bitbucket_signature(&headers, &body, &config.webhook_secret)?;
    let event: BitbucketWebhookEvent = serde_json::from_slice(&body)?;
    handle_bitbucket_event(state, config, event).await
}
```

```rust
// services/control-plane/src/main.rs (target, nested under /api/v2)
.route("/webhook/github",
       post(webhook::github_webhook_handler)
         .layer(DefaultBodyLimit::max(webhook::MAX_BODY_BYTES)))
.route("/webhook/gitlab/{installation_id}",
       post(webhook::gitlab_webhook_handler)
         .layer(DefaultBodyLimit::max(webhook::MAX_BODY_BYTES)))
.route("/webhook/bitbucket/{installation_id}",
       post(webhook::bitbucket_webhook_handler)
         .layer(DefaultBodyLimit::max(webhook::MAX_BODY_BYTES)))
```

#### Rollout checklist

Before removing the legacy paths, repoint:
- [ ] Every GitHub App webhook configuration to `/api/v2/webhook/github`
- [ ] Every GitLab integration to `/api/v2/webhook/gitlab/{installation_id}`
- [ ] Every Bitbucket installation to `/api/v2/webhook/bitbucket/{installation_id}`

---

### #508 — Expose `review-mcp` externally over HTTP/SSE at `/mcp`

**Status:** Open · **Label:** `user-story`

#### Intent

`services/review-mcp` currently serves its tool surface (`read_file`, `graph`, `vector search`,
`run_sast`, etc.) over stdio, as an internal child process of the OpenCode review agent. This
story converts it to an HTTP/SSE MCP server, mounted at `/mcp` in the unified router, reachable
by external MCP clients (e.g. Claude Desktop, other agents).

The internal stdio path used by the review agent is **not forced** onto HTTP/SSE unless that
turns out to be the cleanest implementation approach — this is a decision left to the implementer.

#### Security gate

`/mcp` is new external attack surface. Authn/authz must be scoped at least as tightly as
`/api/v2`'s runner-internal routes before the endpoint ships to any externally-reachable
environment.

#### Key files to change

| File | Change |
|---|---|
| `services/review-mcp/src/main.rs` | Add HTTP/SSE transport alongside or in place of stdio |
| `services/control-plane/src/main.rs` | Mount `mcp_router` under `/mcp` |
| New: `services/review-mcp/src/http.rs` | HTTP/SSE adapter implementing the MCP transport spec |

#### Acceptance Criteria

- An authenticated external MCP client can list and call the same tools the internal review
  agent uses via `/mcp`.
- Unauthenticated and unauthorized access to `/mcp` is rejected (4xx).
- The internal review agent's stdio path is unaffected (or intentionally migrated — decide
  during implementation and document the choice).

#### Implementation sketch

```rust
// services/review-mcp/src/http.rs (new)
use axum::{Router, routing::get, response::Sse};
use tokio_stream::StreamExt;

/// Mount the MCP tool server over HTTP/SSE.
/// Each SSE connection receives MCP protocol messages (initialize, tools/list, tools/call).
pub fn mcp_http_router(tools: Arc<ReviewMcpTools>) -> Router {
    Router::new()
        // MCP discovery endpoint
        .route("/",     get(mcp_capabilities))
        // SSE streaming endpoint — client sends requests as JSON in query/body,
        // server streams results back as SSE events.
        .route("/sse",  get(move |auth, req| mcp_sse_handler(tools.clone(), auth, req)))
}

async fn mcp_sse_handler(
    tools: Arc<ReviewMcpTools>,
    AuthBearer(token): AuthBearer,
    Json(request): Json<McpRequest>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Validate bearer token (same validator as /api/v2's runner-internal routes)
    let _identity = validate_bearer(&token).await?;

    let stream = tools.dispatch(request).map(|msg| {
        Ok(Event::default().data(serde_json::to_string(&msg).unwrap()))
    });
    Sse::new(stream)
}
```

```rust
// services/control-plane/src/main.rs
let mcp = review_mcp::http::mcp_http_router(state.mcp_tools.clone());

Router::new()
    // ...
    .nest("/mcp",    mcp)
    .nest("/a2a",    a2a)
    .nest("/api/v2", api_v2)
```

---

### #509 — Fold A2A ingress into the unified router at `/a2a`

**Status:** Open · **Label:** `user-story`

#### Intent

A2A already lives inside the control-plane binary (it never got its own Deployment). The only
real change is: re-mount the existing A2A routers under `/a2a` in the main `app()` function
instead of starting a separate Axum listener, and update `base_url_from_env()` / the agent
card's advertised base URL to reflect the unified domain + `/a2a` prefix.

The stale doc comment in `a2a/mod.rs` ("The role's k8s Deployment/Ingress is an ai-helm
follow-up") must be corrected to reflect the unified-domain mount.

#### Key files to change

| File | Change |
|---|---|
| `services/control-plane/src/a2a/mod.rs` | Export a plain `axum_router()` fn; remove standalone listener |
| `services/control-plane/src/main.rs` | `.nest("/a2a", a2a::axum_router(state))` instead of `a2a::run(state)` |
| `services/control-plane/src/a2a/card.rs` | Advertise `<domain>/a2a` in the agent card |

#### Acceptance Criteria

- A2A JSON-RPC (`POST /a2a/`) and REST (`/a2a/message:send`, `/a2a/tasks/{id}`, etc.) routes
  behave identically to today's routes, just under the `/a2a` prefix.
- `/.well-known/agent-card.json` is reachable at the new base path.
- The stale "own Ingress" doc comment is gone.
- Existing A2A test suite passes unchanged under the new mount path.

#### Current state of `a2a::run()`

```rust
// services/control-plane/src/a2a/mod.rs (current)
pub async fn run(state: AppState) -> anyhow::Result<()> {
    // ...
    let card_router  = a2a_server::agent_card::agent_card_router(Arc::new(card));
    let protected    = a2a_server::rest::rest_router(handler.clone())
        .merge(a2a_server::jsonrpc::jsonrpc_router(handler));
    let app = Router::new()
        .merge(card_router)
        .merge(protected.layer(...));
    // Starts its OWN listener — this is what gets replaced:
    axum::serve(listener, app).await?;
    Ok(())
}
```

#### Target state

```rust
// services/control-plane/src/a2a/mod.rs (target)
/// Returns the A2A router to be mounted under `/a2a` by the unified app().
pub fn axum_router(state: AppState) -> Router {
    let card     = build_agent_card(&state); // base_url = config or env, e.g. "/a2a"
    let handler  = build_handler(state.clone());
    let card_router = a2a_server::agent_card::agent_card_router(Arc::new(card));
    let protected   = a2a_server::rest::rest_router(handler.clone())
        .merge(a2a_server::jsonrpc::jsonrpc_router(handler))
        .layer(a2a_auth_layer(state));
    Router::new()
        .merge(card_router)
        .merge(protected)
}
```

```rust
// services/control-plane/src/main.rs (target)
.nest("/a2a", a2a::axum_router(state.clone()))
```

#### Agent card base URL update

```rust
// services/control-plane/src/a2a/card.rs (target)
fn base_url_from_config(state: &AppState) -> String {
    // Prefer explicit config; fall back to env; final default uses unified domain.
    state.config.a2a_base_url.clone()
        .or_else(|| std::env::var("A2A_BASE_URL").ok())
        .unwrap_or_else(|| "https://code-intelligence-api.ai.camer.digital/a2a".to_string())
}
```

---

### #510 — `ai-helm`/`ai-helm-values` companion — Ingress/DNS/TLS

**Status:** Open · **Label:** `user-story`

#### Intent

This is a **cross-repo pointer**, not executable from `lightbridge-code-intelligence` alone. It
exists to track the Kubernetes Ingress/DNS/TLS work explicitly so that it is not assumed to ship
atomically with the code-side route changes.

No Kubernetes manifests, Ingress objects, or Helm charts exist in this repo — they live in the
separate `ai-helm`/`ai-helm-values` repositories.

#### What needs to happen (in `ai-helm`/`ai-helm-values`)

1. **DNS entry** — `code-intelligence-api.ai.camer.digital` pointing at the cluster's Ingress.
2. **TLS certificate** — cert-manager (or equivalent) provisioned for the new subdomain, matching
   the cluster's existing pattern for other `ai.camer.digital` subdomains.
3. **Ingress object** — routes all traffic on that domain to the `control-plane` Service, with
   `nginx.ingress.kubernetes.io/rewrite-target` or equivalent for the path prefixes if needed.

#### Sample Ingress manifest (illustrative)

```yaml
# ai-helm-values/control-plane/ingress.yaml (target, illustrative)
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: code-intelligence-api
  annotations:
    cert-manager.io/cluster-issuer: "letsencrypt-prod"
    nginx.ingress.kubernetes.io/proxy-body-size: "32m"
spec:
  ingressClassName: nginx
  tls:
    - hosts:
        - code-intelligence-api.ai.camer.digital
      secretName: code-intelligence-api-tls
  rules:
    - host: code-intelligence-api.ai.camer.digital
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: control-plane
                port:
                  number: 8080
```

#### Acceptance Criteria

- DNS resolves `code-intelligence-api.ai.camer.digital` to the cluster Ingress.
- TLS handshake is valid; no certificate warning.
- Smoke requests to `/a2a/`, `/mcp/`, and `/api/v2/healthz` from outside the cluster succeed.

---

### #511 — Fix the dangling ADR-0098 roadmap reference

**Status:** Open · **Label:** `user-story`

#### Intent

Small governance hygiene. `ROADMAP.md`'s "In progress" section cites "ADR-0098" for A2A
per-finding streaming — but `docs/adr/` jumps from `0097` to `0099`; no `0098-*.md` file exists.
The label actually points at the still-open PR #458.

#### Current state

```markdown
<!-- ROADMAP.md, "In progress" section — current -->
- **A2A per-finding review streaming** — stream findings as they are confirmed at finalize.
  ([ADR-0098](https://github.com/vymalo/lightbridge-code-intelligence/pull/458), #458 — open)
```

#### Target state

```markdown
<!-- ROADMAP.md, "In progress" section — target -->
- **A2A per-finding review streaming** — stream findings as they are confirmed at finalize.
  ([PR #458](https://github.com/vymalo/lightbridge-code-intelligence/pull/458) — open)
```

#### Acceptance Criteria

- No remaining `ADR-0098` string in `ROADMAP.md` or `docs/`.
- The line still links to PR #458 (content unchanged, label corrected).
- Note: this story does **not** pre-allocate a `0098` ADR number; that happens when PR #458
  actually merges.

> **Implementation note:** This was already applied as part of the PR #520 batch
> (`ROADMAP.md` edit in the ADR-0109 merge). This issue exists to confirm/track it explicitly.

---

## 6. Implementation Plan

### Phase overview

The six stories have a natural dependency ordering. Stories that touch the same files (routes,
webhook handlers) should coordinate but can be implemented sequentially. The infra story (#510)
is a cross-repo blocker for production reachability only — the code-side work can be verified
locally and in a test cluster first.

```
Phase 1 — Housekeeping (no-risk, no-dep)
  └── #511  Fix dangling ADR-0098 reference in ROADMAP.md

Phase 2 — Route versioning (lowest blast-radius code change)
  └── #506  Migrate every route to /api/v2 + update all consumers

Phase 3 — Webhook path restructure (depends on #506 for /api/v2 base)
  └── #507  Path-scoped multi-forge webhook routes

Phase 4 — Protocol surface changes (independent, but security-gated)
  ├── #509  Fold A2A into /a2a in the unified router
  └── #508  Expose review-mcp at /mcp   ← security review before exposing externally

Phase 5 — Infrastructure (unblocks production reachability)
  └── #510  ai-helm/ai-helm-values Ingress/DNS/TLS companion
```

### Phase 1 — Fix ADR-0098 reference (#511)

This is a single-line `ROADMAP.md` edit. It was already applied in the PR #520 batch. Close
after verifying `grep -r "ADR-0098" ROADMAP.md docs/` returns empty.

**Effort:** Trivial (< 30 min). **Risk:** None.

### Phase 2 — `/api/v2` migration (#506)

1. In `services/control-plane/src/main.rs`, extract all non-health routes into a
   `fn api_v2_router(state: AppState) -> Router` function and wrap with `.nest("/api/v2", ...)`.
2. Update `apps/web/lib/server/api.ts` — find all fetch calls to `/tasks`, `/repositories`,
   `/me`, `/admin/...`, `/internal/...` and prefix with `/api/v2`.
3. Update the `lci` CLI HTTP client.
4. Update the agent-runner's internal API client (`/internal/tasks/...` call sites).
5. Add 404 regression test for one removed flat path.
6. Run `cargo test --workspace` and Next.js build to confirm no broken call sites.

**Effort:** Medium (1–2 days). **Risk:** Low — purely mechanical rename.

```bash
# Verification: confirm no remaining flat-path fetch calls in apps/web
grep -rn 'fetch.*"/tasks\|fetch.*"/me\|fetch.*"/repositories' apps/web/
```

### Phase 3 — Path-scoped webhook routes (#507)

1. Create `webhook::github_webhook_handler`, `webhook::gitlab_webhook_handler`,
   and `webhook::bitbucket_webhook_handler` in `webhook.rs`, each with platform-specific
   signature verification and a typed event deserializer.
2. The `bitbucket` handler requires the Bitbucket `CodePlatform` implementation from PR #522
   (Epic #353) to be merged first, or can be stubbed as a 501 until then.
3. Remove `webhook_router` (the header-sniffer), `github_webhook_legacy`, and their routes.
4. Update external webhook configurations per the rollout checklist (see §5.2).

**Effort:** Medium–High (2–3 days including rollout coordination). **Risk:** High — missed
repoint silently drops webhook delivery. The rollout checklist must be executed and verified
before removing the legacy paths.

```bash
# Confirm legacy alias is gone post-cutover
curl -X POST https://code-intelligence-api.ai.camer.digital/webhook        # expect 404
curl -X POST https://code-intelligence-api.ai.camer.digital/github/webhook # expect 404
```

### Phase 4a — A2A path fold (#509)

1. Refactor `a2a::run()` into `a2a::axum_router(state) -> Router` — move the router-building
   logic out of the async run function, drop the standalone `axum::serve` call.
2. Update `main.rs`: replace `"a2a" => a2a::run(state).await` with mounting the router via
   `.nest("/a2a", a2a::axum_router(state))` in the `serve` role's `app()`.
3. Update `base_url_from_env()` / card builder to default to the unified domain + `/a2a`.
4. Fix the stale doc comment in `a2a/mod.rs`.
5. Run the existing A2A test suite (`cargo test -p control-plane`).

**Effort:** Low–Medium (1 day). **Risk:** Low — the routers already exist; this is plumbing.

### Phase 4b — MCP HTTP/SSE exposure (#508)

1. Implement an HTTP/SSE transport for `services/review-mcp`, following current MCP spec
   conventions. This involves adding an Axum router that wraps the existing stdio tool
   dispatch in an SSE streaming response.
2. Gate the `/mcp` mount behind authn (bearer token, same validator as `/api/v2` internal
   routes) — this must be reviewed before the endpoint ships anywhere externally reachable.
3. Decide whether the internal stdio path stays (preferred: dual transport, stdio for OpenCode,
   HTTP for external) or is migrated to HTTP.
4. Integration test: external MCP client lists and calls at least one tool over `/mcp`.

**Effort:** High (3–5 days for transport + authn + tests). **Risk:** Medium–High (new external
attack surface; security review is a gate, not optional).

### Phase 5 — Ingress/DNS/TLS (#510)

File a ticket in `ai-helm`/`ai-helm-values` linking back to this epic. The ticket should
include the sample Ingress manifest from §5.5, the three path prefixes to route, and the
cert-manager issuer name used by other `ai.camer.digital` subdomains.

**Effort:** Depends on `ai-helm` owner. **Risk:** Medium — if not coordinated, the code-side
route changes land but the domain stays unreachable in production.

---

## 7. Risk Register

| Risk | Probability | Impact | Mitigation |
|---|---|---|---|
| Missed webhook repoint at cutover silently drops delivery | Medium | **High** | Explicit per-integration rollout checklist (see §5.2) must be completed and verified before removing legacy paths in production |
| `ai-helm`-side Ingress/cert work doesn't land in the same window as the route change | Medium | Medium | Cross-repo companion ticket (#510) filed and linked; code-side and infra-side are explicitly not assumed atomic |
| Externally-exposed `/mcp` broadens attack surface | Medium | **High** | Authn/authz on `/mcp` scoped ≥ `/api/v2` runner-internal; security review is a gate before exposing to any externally-reachable env |
| Bitbucket webhook handler stubbed (CodePlatform not yet merged from #353/PR #522) | Low | Low | Stub returns 501 until Epic #353 lands; path is registered immediately so webhook configs can be set up in advance |
| Agent-card clients hardcoding the old A2A base URL break after `/a2a` remount | Low | Medium | Audit external A2A clients before shipping; update `base_url_from_env()` default and verify agent card content in tests |

---

## 8. Success Metrics

These are the epic's own stated metrics from ADR-0109:

| Metric | Current state | Target | How to verify |
|---|---|---|---|
| Routes under a version prefix | 0% | 100% (`/api/v2`) | Router inspection: `grep -c '\.route("/' main.rs` vs `.nest("/api/v2"` |
| Legacy `/github/webhook` alias live | Yes | Removed | `curl -X POST .../github/webhook` → 404 |
| Forges with a dedicated webhook path | 1 (GitHub + unified `/webhook`) | 3 (GitHub, GitLab, Bitbucket) | Router inspection |
| Externally-reachable MCP endpoint | None | `/mcp` live | `curl .../mcp` from outside the cluster → 200 or 401 (not 404) |

---

## 9. Dependencies & Related Epics

### Direct dependencies

| Dependency | Why | Tracked |
|---|---|---|
| Epic #353 / PR #522 — `CodePlatform` + Bitbucket | Bitbucket's `CodePlatform` implementation; #507's Bitbucket handler needs it | PR #522 open |
| `ai-helm`/`ai-helm-values` Ingress work | Required for production reachability of the unified domain | #510 |

### Related epics

| Epic | Relationship |
|---|---|
| **Epic #353** — Control-plane v2, StepRuntime generalization + CodePlatform | Provides the Bitbucket `CodePlatform` implementation that #507 routes to |
| **Epic #491** — Repo-configurable OpenCode review presets | Operates on the same control-plane binary; route prefix change from #506 must not break its webhook/runner-internal call sites |
| **Closed Epic #294** — Restate egress | Retired (ADR-0093); mentioned for completeness |
| **Closed Epic #295** — A2A own-Deployment/Ingress | Superseded by this epic; A2A stays in the same binary and gets a path prefix instead |

### ADRs in play

| ADR | Status | Relevance |
|---|---|---|
| [ADR-0109](docs/adr/0109-unify-domain-code-intelligence-api.md) | Accepted (merged PR #520, 2026-07-26) | **Source of truth** for this epic |
| [ADR-0108](docs/adr/0108-codeplatform-github-gitlab-bitbucket.md) | Accepted | Bitbucket CodePlatform wiring that #507 routes to |
| [ADR-0072](docs/adr/0072-platform-abstraction-codeplatform-trait.md) | Accepted | The `CodePlatform` trait underpinning multi-forge dispatch |
| [ADR-0104](docs/adr/0104-full-opencode-fs-tool-suite.md) | Accepted | Tool implementations exposed by #508; not changed by this epic |

---

_Report generated 2026-07-28. All issue data sourced directly from the GitHub API via MCP.
Epic #492 has 6 child stories (all open), 0% complete._
