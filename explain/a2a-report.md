# Lightbridge Code Intelligence — A2A Report

**Scope:** what A2A is, how it's implemented in `lightbridge-code-intelligence`, how it's deployed on the `hetzner-prod` cluster right now, and how to test it end-to-end with Postman.

Generated: 2026-08-21. Sources: `~/ai/lightbridge-code-intelligence` (code), `~/ai/ai-helm` (chart defaults), `~/ai/ai-helm-values` (deployed values), and a live `kubectl`/`curl` inspection of the `hetzner-prod` cluster.

---

## 1. What the project is

`lightbridge-code-intelligence` is a GitHub/GitLab App that does automated deep code review and repo Q&A. A Rust control-plane (Axum) receives forge webhooks, queues review "tasks" in Postgres, and dispatches each to an isolated Kubernetes Job that does retrieval-augmented review (Neo4j knowledge graph + pgvector) and posts results back to the PR/MR.

## 2. What "A2A" is here

**It's a real implementation of the [A2A protocol](https://a2a-protocol.org/latest/specification/)** (Agent2Agent, Linux Foundation, spec **v1.0.1**) — not a bespoke scheme. It's built on the official Rust SDK crates:

```toml
a2a-lf = "=0.3.0"          # proto-derived wire types (crate name `a2a`)
a2a-server-lf = "=0.4.0"   # Axum-native server framework (crate name `a2a_server`)
```

The idea (from [`docs/rfc/0006-a2a-agent-surface.md`](../docs/rfc/0006-a2a-agent-surface.md)): let *other agent platforms* (AWS Bedrock AgentCore, Microsoft Foundry, Google Vertex AI Agent Engine, GitLab Duo Agent Platform, etc.) call Lightbridge as a peer agent to request a deep PR review, with a durable, pollable/streamable/webhook-able task handle — instead of impersonating a human commenter on the PR.

> MCP and A2A are complementary: **MCP is agent-to-*tool***, **A2A is agent-to-*agent***. Lightbridge's own review agent still uses MCP tools internally; A2A is how *other agents reach Lightbridge*.

Only **one skill ships**: `review` (deep PR/MR review). A second skill, `ask`, was proposed in the RFC but never implemented. RFC-0006 has 4 phases:

| Phase | Feature | Status |
|---|---|---|
| 1 | Card + `SendMessage` + `GetTask` + `CancelTask` (polling) | ✅ shipped |
| 2 | Streaming (`SubscribeToTask` + streaming `SendMessage`) — ADR-0077 | ✅ shipped |
| 3 | Push notifications (webhooks) — ADR-0079 | ✅ shipped |
| 4 | `input-required` clarify/confirm loop + `ListTasks` — ADR-0081 | ❌ design-only, returns `UNSUPPORTED_OPERATION` |

## 3. Where it lives in the code

All production A2A code is in the Rust control-plane, `services/control-plane/src/a2a/`:

| Path | Role |
|---|---|
| `a2a/mod.rs` | Role entrypoint (`run`), router assembly, OIDC auth middleware, env config |
| `a2a/card.rs` | Builds the public Agent Card (`/.well-known/agent-card.json`) |
| `a2a/handler.rs` (+ `handler/lifecycle.rs`, `handler/streaming.rs`) | `A2aHandler` — implements every A2A method |
| `a2a/mapping.rs` | Lightbridge status ↔ A2A `TaskState`; parses the `review` request; builds artifacts |
| `a2a/store.rs` | `PgTaskStore` — Postgres-backed `TaskStore`, with optimistic-concurrency (CAS) versioning |
| `a2a/events.rs` | Append-only per-task streaming event log, written in the same DB transaction as status changes |
| `a2a/ssrf.rs`, `a2a/push_crypto.rs` | SSRF-guarded webhook URL validation + AEAD encryption of webhook auth tokens |
| `queue/notifier.rs` | The `notifier` role — drains the event log, POSTs webhooks |
| `queue/a2a_sweeper.rs` | TTL sweeper for terminal `a2a_tasks` rows |
| `migrations/0025_a2a_tasks.sql`, `0026_a2a_task_events.sql`, `0027_a2a_push_configs.sql` | Schema |
| `docs/rfc/0006-a2a-agent-surface.md` | Design RFC |
| `docs/adr/0077…0081-*.md` | Phase-specific ADRs |
| `docs/a2a-review-skill.md` | **The calling guide** — curl walkthrough this report's Postman section mirrors |

No TypeScript/web code touches A2A — it's exclusively a Rust control-plane concern, exposed as its own Kubernetes role.

## 4. How it works mechanically

### 4.1 Transport

One Axum router serves two A2A-spec transports off the same handler (`build_router`, `a2a/mod.rs:130-158`):

```rust
let card_router =
    a2a_server::agent_card::agent_card_router(Arc::new(a2a_server::StaticAgentCard::new(card)));

let protected = a2a_server::rest::rest_router(handler.clone())
    .merge(a2a_server::jsonrpc::jsonrpc_router(handler))
    .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))       // 256 KiB cap
    .layer(axum::middleware::from_fn_with_state(auth_state, a2a_auth));

Router::new().merge(card_router).merge(protected)
```

- **JSON-RPC 2.0** over `POST /` — preferred transport, PascalCase methods (`SendMessage`, `GetTask`, …).
- **REST/HTTP+JSON** — `POST /message:send`, `GET /tasks/{id}`, `POST /tasks/{id}/cancel`, `POST /tasks/{id}/subscribe`, `POST /message:stream`, `POST /tasks/{id}/pushNotificationConfigs`, etc.
- **SSE** for the two streaming methods.
- No gRPC, no WebSocket. Lightbridge only ever acts as an A2A **server** — it never calls out to other A2A agents.

### 4.2 The Agent Card

Served **unauthenticated** at `/.well-known/agent-card.json` (note: `agent-card.json`, the v1.0 name — not the older `agent.json`). Built in `a2a/card.rs`:

```rust
AgentCard {
    name: "Lightbridge Code Intelligence".to_string(),
    description: "A2A surface over Lightbridge's deep code-review agent. …".to_string(),
    version: env!("CARGO_PKG_VERSION").to_string(),
    supported_interfaces: vec![
        AgentInterface::new(base_url.to_string(), TRANSPORT_PROTOCOL_JSONRPC),
        AgentInterface::new(base_url.to_string(), TRANSPORT_PROTOCOL_HTTP_JSON),
    ],
    capabilities: AgentCapabilities { streaming: Some(true), push_notifications: Some(true), .. },
    skills: vec![AgentSkill {
        id: "review".to_string(),
        name: "Deep PR review".to_string(),
        description: REVIEW_SKILL_DESCRIPTION.to_string(), // inline JSON-Schema draft-07
        security_requirements: Some(vec![HashMap::from([
            (OIDC_SCHEME_NAME.to_string(), vec!["a2a:review".to_string()]),
        ])]),
        ..
    }],
    security_schemes: Some(security_schemes), // OpenID Connect, Keycloak discovery URL
    ..
}
```

I fetched the **live** card from the running cluster to confirm this matches production exactly (see §6). It reports `"version":"0.1.1"`, the same two transports, `capabilities: {"streaming":true,"pushNotifications":true}`, one skill (`review`), and `securitySchemes.keycloak-oidc.openIdConnectSecurityScheme.openIdConnectUrl` pointing at `https://auth.verif.fyi/realms/camer-digital/.well-known/openid-configuration`.

### 4.3 Endpoints

| Method (JSON-RPC) | REST | Status | Purpose |
|---|---|---|---|
| `SendMessage` | `POST /message:send` | ✅ | Submit a `review` request → `Task` (`SUBMITTED` or `REJECTED`) |
| `GetTask` | `GET /tasks/{id}` | ✅ | Poll state; caller-scoped |
| `CancelTask` | `POST /tasks/{id}/cancel` | ✅ | Cancel a non-terminal task |
| `SendStreamingMessage` | `POST /message:stream` | ✅ | Submit + stream (SSE) in one call |
| `SubscribeToTask` | `POST /tasks/{id}/subscribe` | ✅ | Stream (SSE) an existing task |
| `CreateTaskPushNotificationConfig` | `POST /tasks/{id}/pushNotificationConfigs` | ✅ | Register a webhook |
| `Get/List/DeleteTaskPushNotificationConfig` | matching REST | ✅ | Manage webhooks, caller-scoped |
| `ListTasks` | — | ❌ `UNSUPPORTED_OPERATION` | Phase 4, not shipped |

**Request** — the review target/scope goes in a `data` part of a `ROLE_USER` message; an optional `text` part (ADR-0078) only steers emphasis, never the target:

```json
{
  "jsonrpc": "2.0", "id": "req-1", "method": "SendMessage",
  "params": { "message": { "messageId": "b7c1e0a2-…", "role": "ROLE_USER",
    "parts": [ { "data": {
      "skill": "review", "forge": "github", "repo": "acme/api", "pr": "164",
      "headSha": "9f2a1c4e8b7d6053a1f4c2e9b8d70a5c3e1f2b6d",
      "baseSha": "1b0dd7a4c9e2f6538a0c4b1e9d7f2a5c3e8b6d04",
      "prompt": "Focus on the auth changes and the new migration."
    } } ] } }
}
```

| Field | Required | Effect |
|---|---|---|
| `repo` | **yes** | `"owner/name"`, must be an already-approved repo, else `REJECTED` |
| `pr` | **yes** | int or numeric string, `>0` |
| `headSha` | **yes** | server holds no forge creds, can't resolve a head itself — omit it and you get `REJECTED` |
| `baseSha` | recommended | **present → diff-scoped review of the PR delta; absent → whole-working-tree review at `headSha`** (a much broader audit — this is the #1 gotcha) |
| `forge` | no (`github`) | `github`\|`gitlab` |
| `prompt` | no | free-text emphasis hint; a `text` part on the message wins over this |

**Response (`SUBMITTED`):**

```json
{ "task": { "id": "0190c3f2-…", "contextId": "0190c3f2-…",
  "status": { "state": "TASK_STATE_SUBMITTED" },
  "metadata": { "lb.underlyingTaskId": "0190c3ee-…" } } }
```

**State mapping:**

| A2A wire state | Lightbridge status | Terminal |
|---|---|---|
| `TASK_STATE_SUBMITTED` | `received`, `queued`, `waiting_for_index` | no |
| `TASK_STATE_WORKING` | `running`, `posting_result` | no |
| `TASK_STATE_COMPLETED` | `succeeded` | yes |
| `TASK_STATE_FAILED` | `failed`, `timed_out` | yes |
| `TASK_STATE_CANCELED` | `cancelled` | yes |
| `TASK_STATE_REJECTED` | refused at the submission gate (bad auth, quota, unapproved repo, missing `headSha`) | yes |

**On `COMPLETED`**, `GetTask` returns one `review` artifact, three parts in order: a text summary, a `data` part of structured findings, and a `data` "context" part (`repo`/`pr`/`baseSha`/`headSha`/`scope`/`reviewUrl`).

### 4.4 AuthN / AuthZ

- **AuthN** — Keycloak OIDC, **client-credentials grant only** (service accounts — no anonymous access, no browser flow). Enforced by the `a2a_auth` Axum middleware (`a2a/mod.rs:172-206`): validates the bearer JWT, **strips any client-supplied spoof headers first**, then injects the verified identity as internal headers (`x-lb-a2a-caller`, `x-lb-a2a-perms`) the client can never forge. A JWKS outage returns a retryable `503`, not `401`, so SDKs back off instead of giving up.
- **AuthZ** — per-skill permission claim `a2a:review` (ADR-0023). Missing it is **not** a transport error — it comes back as a terminal `TASK_STATE_REJECTED` task.
- **Quota** — `A2A_QUOTA_MAX` submissions per `A2A_QUOTA_WINDOW_SECS` per identity (default **20/hour**, clamped ≥1). Breach → `REJECTED`.
- **Repo-approval gate** (ADR-0063) is re-checked at A2A submission time — "A2A is never a side door" around it.
- The role holds **zero forge credentials** — it only creates task rows through the *same code path* as a human `@mention`, so an A2A review of a head already under review dedups onto the same run.

### 4.5 Data model

Three Postgres tables, cascade-linked:
- `a2a_tasks` — task_id/context_id/caller_id/skill/underlying_task_id/state/**version** (CAS)/task_json snapshot.
- `a2a_task_events` — append-only `(a2a_task_id, seq)` event log (`status-update`/`artifact-update`, `final` flag, serialized `StreamResponse` payload).
- `a2a_push_configs` — webhook registrations: url, `token_enc` (AEAD-encrypted), delivery cursor, attempts, lease, state (`active`/`disabled`).

The store implements its own **optimistic-concurrency CAS** because the SDK's default `TaskStore::update()` is a blind upsert with no expected-version check — under multiple `a2a` replicas that would silently lose updates:

```rust
"UPDATE a2a_tasks SET task_json = $2, state = $3, ..., version = version + 1, updated_at = now() \
 WHERE a2a_task_id = $1 AND version = $5 RETURNING version"
```

### 4.6 Streaming (SSE)

"Replay-then-tail", durable across replicas (ADR-0077):
1. Emit the current `Task` snapshot.
2. **Replay** every already-logged event from `a2a_task_events` in `seq` order.
3. **Tail**: Postgres `LISTEN`/`NOTIFY` per task, with a bounded 5s poll fallback — NOTIFY is only a wake hint, the `seq`-cursor `SELECT` is the actual ordering authority.
4. Close on the terminal event.

Events are appended **inside the same DB transaction** that flips task status — polling and streaming can never disagree. A dedicated, bounded `LISTEN` connection pool (max 64 total, 8 per caller) protects the request pool from long-lived SSE connections. Max stream lifetime: 3 hours.

### 4.7 Push notifications (webhooks) — Phase 3

Instead of holding a connection: `CreateTaskPushNotificationConfig` registers a URL; the separate **`notifier` role** drains the same event log per-config and POSTs `StreamResponse` bodies to it, at-least-once, strictly in `seq` order, header `X-A2A-Notification-Id: {taskId}:{seq}` for dedup, exponential backoff, dead-lettered after repeated failures. SSRF-hardened: **HTTPS-only, port 443, must resolve to a public address**, re-validated at every delivery (DNS-rebinding defence), connection pinned to the checked IP, redirects never followed.

### 4.8 How it fits the wider architecture

A2A is a **fourth ingress face** on the control plane (alongside the forge-webhook handler, the admin surface, and the internal API). It creates review tasks through the exact same path as an `@mention`, runs the `deep` tier only (2-hour budget, no `fast`/auto tier), and never touches the forge directly — posting the PR review stays on the existing reconciler/egress path (ADR-0059). `a2a` and `notifier` are separate Deployments off the *same* control-plane binary (`CONTROL_PLANE_ROLE=a2a`/`notifier`), deliberately split so the notifier's "POST to arbitrary caller URLs" role can carry a tighter NetworkPolicy.

---

## 5. How it's deployed — Helm chart shape

Chart: `~/ai/ai-helm/charts/lightbridge-code-intelligence` (Helm release name `lightbridge-ci`, chart version `lci-4.6.2` at time of writing). The `a2a` role is the **same container image** as `control-plane`/`mcp`/`notifier`, just role-selected via `CONTROL_PLANE_ROLE`.

Chart defaults (`values.yaml`) ship the `a2a` controller **`enabled: false`** — it's deployment-specific wiring (OIDC issuer/audience, base URL, DB URL, Ingress host) that only `ai-helm-values` supplies per environment. Relevant chart-default env block:

```yaml
a2a:
  enabled: false
  replicas: 2
  containers:
    main:
      image:
        repository: ghcr.io/adorsys-gis/lightbridge-control-plane
        tag: "0.1.0"
      env:
        CONTROL_PLANE_ROLE: "a2a"
        A2A_BIND: "0.0.0.0:8080"
        METRICS_ADDR: "0.0.0.0:9090"
        A2A_BASE_URL: ""            # deployment-specific
        PERMISSIONS_CLAIM: "permissions"
        OIDC_ISSUER: ""             # deployment-specific
        OIDC_AUDIENCE: ""           # deployment-specific
        DATABASE_URL: { dependsOn: DB_PASSWORD, value: "" }
        A2A_PUSH_TOKEN_KEY: { valueFrom: { secretKeyRef: { name: '{{ .Release.Name }}-a2a-push', key: push-token-key, optional: true } } }
  probes:
    liveness:  { httpGet: { path: /healthz, port: 9090 } }
    readiness: { httpGet: { path: /.well-known/agent-card.json, port: 8080 } }   # unauthenticated card doubles as readiness
```

**Per-environment values** live in the *private* `ai-helm-values` repo (never in `ai-helm` itself — see `ai-helm/CLAUDE.md`'s ADR-0055/0056 split), file `environments/prod/values/lightbridge-code-intelligence.yaml`:

```yaml
a2a:
  enabled: true
  containers:
    main:
      image: { repository: ghcr.io/adorsys-gis/lightbridge-control-plane, tag: "sha-71f3e5e" }
      env:
        OIDC_ISSUER:   "https://auth.verif.fyi/realms/camer-digital"
        OIDC_AUDIENCE: "code-intelligence"
        A2A_BASE_URL:  "https://code-intelligence-api.ai.camer.digital/a2a"
        DATABASE_URL:  { value: "postgresql://codeintel:$(DB_PASSWORD)@lightbridge-main-db-rw.converse.svc.cluster.local:5432/codeintel" }
service: { a2a: { enabled: true } }
ingress:
  a2a:
    enabled: true
    className: traefik
    annotations:
      cert-manager.io/cluster-issuer: cert-home-cert-http
      traefik.ingress.kubernetes.io/router.middlewares: "converse-lci-strip-prefix-a2a@kubernetescrd"
    hosts:
      - host: code-intelligence-api.ai.camer.digital
        paths: [{ path: /a2a, pathType: Prefix, service: { identifier: a2a, port: http } }]
    tls: [{ secretName: code-intelligence-api.ai.camer.digital-tls, hosts: [code-intelligence-api.ai.camer.digital] }]
rawResources:
  lci-strip-prefix-a2a:                    # Traefik Middleware — the pod expects requests at `/`, not `/a2a`
    apiVersion: traefik.io/v1alpha1
    kind: Middleware
    spec: { spec: { stripPrefix: { prefixes: ["/a2a"] } } }
```

The comment block in that file flags the surface as **"INERT-BUT-SAFE until operator prereqs exist"** (a Keycloak client emitting `a2a:review`, DNS, confirmed quota defaults) — see §7 for what that means for testing.

## 6. What's actually running right now (`hetzner-prod`)

Note on `kubectl`: your shell has `kubectl` aliased to `multipass exec k3s -- kubectl`, which doesn't resolve on this machine — I bypassed it with `command kubectl` (the real binary), which correctly picks up `~/.kube/config`'s `hetzner-prod` context (OIDC-authenticated, `k8s.ssegning.com:6443`). You may want to fix or drop that alias so plain `kubectl` works.

**Namespace:** `converse`. **Deployment:** `lightbridge-ci-a2a` (label `app.kubernetes.io/controller: a2a`), **2/2 replicas Ready**.

```
$ kubectl get deploy lightbridge-ci-a2a -n converse
NAME                 READY   UP-TO-DATE   AVAILABLE
lightbridge-ci-a2a   2/2     2            2
```

Live pod-spec env (as of this check):

```
image: ghcr.io/adorsys-gis/lightbridge-control-plane:sha-05e91bd
env:
  A2A_BASE_URL:      https://code-intelligence-api.ai.camer.digital/a2a
  A2A_BIND:          0.0.0.0:8080
  CONTROL_PLANE_ROLE: a2a
  DATABASE_URL:      postgresql://codeintel:$(DB_PASSWORD)@lightbridge-main-db-rw.converse.svc.cluster.local:5432/codeintel
  METRICS_ADDR:      0.0.0.0:9090
  OIDC_AUDIENCE:     code-intelligence
  OIDC_ISSUER:       https://auth.verif.fyi/realms/camer-digital
  PERMISSIONS_CLAIM: permissions
  RUST_LOG:          info
readinessProbe: GET :8080/.well-known/agent-card.json
livenessProbe:  GET :9090/healthz
```

> Note: the running image tag (`sha-05e91bd`) doesn't match the tag currently sitting in my local `ai-helm-values` checkout (`sha-71f3e5e`) — that checkout is on a feature branch behind `origin/main`, and `argocd-image-updater` commits new tags on its own cadence. The **live cluster is the source of truth**; treat the values-repo file as chart-shape documentation, not a live tag reference.

**Ingress + routing:**

| Ingress | Host | Path | Backend | Notes |
|---|---|---|---|---|
| `lightbridge-ci-a2a` | `code-intelligence-api.ai.camer.digital` | `/a2a` (Prefix) | `svc/lightbridge-ci-a2a:8080` | Traefik middleware `lci-strip-prefix-a2a` strips `/a2a` before it reaches the pod |
| `lightbridge-ci-control-plane` | same host | `/` | `svc/lightbridge-ci-control-plane:8080` | admin/webhook surface |
| `lightbridge-ci-mcp` | same host | `/mcp` | `svc/lightbridge-ci-mcp:8080` | MCP surface, unrelated to A2A |

So externally: **`https://code-intelligence-api.ai.camer.digital/a2a/...`** ⇒ internally the pod sees `.../...` (prefix stripped). E.g. the public agent card is at:

```
https://code-intelligence-api.ai.camer.digital/a2a/.well-known/agent-card.json
```

**I fetched it live and it works — the A2A surface is fully up, not just "inert":**

```bash
$ curl -s https://code-intelligence-api.ai.camer.digital/a2a/.well-known/agent-card.json | jq '{name,version,capabilities,supportedInterfaces}'
{
  "name": "Lightbridge Code Intelligence",
  "version": "0.1.1",
  "capabilities": { "streaming": true, "pushNotifications": true },
  "supportedInterfaces": [
    { "url": "https://code-intelligence-api.ai.camer.digital/a2a", "protocolBinding": "JSONRPC", "protocolVersion": "1.0" },
    { "url": "https://code-intelligence-api.ai.camer.digital/a2a", "protocolBinding": "HTTP+JSON", "protocolVersion": "1.0" }
  ]
}
```

`securitySchemes.keycloak-oidc.openIdConnectSecurityScheme.openIdConnectUrl` = `https://auth.verif.fyi/realms/camer-digital/.well-known/openid-configuration` — confirming the realm the deployment actually uses is **`camer-digital`**, not `lightbridge` (the example in `docs/a2a-review-skill.md` §1 uses a `realms/lightbridge` URL — that's illustrative/generic in the doc, not this deployment's real endpoint; use `camer-digital` for prod).

## 7. What you need before you can call anything beyond the public card

The card is deliberately public (A2A spec requirement) — you can fetch it right now with no auth. Everything else (`SendMessage`, `GetTask`, …) needs a **Keycloak client-credentials client in the `camer-digital` realm** whose token:

1. has audience (`aud`) = `code-intelligence`, and
2. carries `a2a:review` in the `permissions` claim (the claim path is configurable per-role but deployed as literally `permissions`).

Per `ai-helm-values`' own comment, this client is an **operator prerequisite that lives outside both `ai-helm` and `ai-helm-values`** — the `camer-digital` realm is manually managed in Keycloak (see `ai-helm/CLAUDE.md`: *"⚠️ `charts/keycloak-baseline` is NOT deployed or reconciled by anything… The `camer-digital` realm is manually managed"*). I found no client secret / `A2A_CLIENT_ID`-shaped Secret already wired into the cluster for this purpose, so:

- **If you already have a Keycloak client for this** (client_id/secret with the `code-intelligence` audience + `a2a:review` permission), skip to §8 — everything below just needs that client's credentials.
- **If not**, you (or whoever administers `auth.verif.fyi` → realm `camer-digital`) need to create one: a confidential client with `Service accounts roles` / client-credentials grant enabled, `aud: code-intelligence` mapped into its tokens, and a role/mapper that lands `a2a:review` in the `permissions` claim. That's a Keycloak-admin-console task, not something this repo or chart automates.

Without that client you can still fully exercise the **auth boundary** (confirm `401`s, confirm the card, confirm `TASK_STATE_REJECTED` semantics can't be reached without a token) — you just can't get past authentication to submit a real review.

## 8. Testing with Postman

I built an importable Postman collection mirroring `docs/a2a-review-skill.md` exactly — see the sibling file `lightbridge-a2a.postman_collection.json`. Structure:

**Collection variables** (set these before running anything):

| Variable | Value |
|---|---|
| `keycloak_base` | `https://auth.verif.fyi` |
| `realm` | `camer-digital` |
| `client_id` | *(your provisioned A2A client id)* |
| `client_secret` | *(its secret)* |
| `a2a_base` | `https://code-intelligence-api.ai.camer.digital/a2a` |
| `access_token` | *(auto-filled by the "Get Token" request's test script)* |
| `task_id` | *(auto-filled by "SendMessage"'s test script)* |

### 8.1 Folder: `0 — Discovery` (no auth)

**GET Agent Card**
`GET {{a2a_base}}/.well-known/agent-card.json`
Expect `200`, JSON body with `skills[0].id == "review"`. This is your smoke test that the surface is up at all — safe to run any time.

### 8.2 Folder: `1 — Auth`

**POST Get Token**
`POST {{keycloak_base}}/realms/{{realm}}/protocol/openid-connect/token`
Body (`x-www-form-urlencoded`):
```
grant_type=client_credentials
client_id={{client_id}}
client_secret={{client_secret}}
scope=code-intelligence
```
Postman **Tests** tab script (already in the collection) saves the token:
```javascript
const body = pm.response.json();
pm.collectionVariables.set("access_token", body.access_token);
pm.test("got access_token", () => pm.expect(body.access_token).to.be.a("string"));
```
Expect `200` + a JWT. Decode it (jwt.io or Postman's built-in) and confirm `aud` includes `code-intelligence` and the `permissions` claim includes `a2a:review` — if either is missing, every protected call below will either `401` or come back `TASK_STATE_REJECTED`.

### 8.3 Folder: `2 — Review skill (JSON-RPC)`

All requests: `POST {{a2a_base}}/`, header `Authorization: Bearer {{access_token}}`, `Content-Type: application/json`.

**SendMessage** — body:
```json
{
  "jsonrpc": "2.0",
  "id": "req-1",
  "method": "SendMessage",
  "params": {
    "message": {
      "messageId": "{{$guid}}",
      "role": "ROLE_USER",
      "parts": [
        { "data": {
            "skill": "review",
            "forge": "github",
            "repo": "acme/api",
            "pr": "164",
            "headSha": "9f2a1c4e8b7d6053a1f4c2e9b8d70a5c3e1f2b6d",
            "baseSha": "1b0dd7a4c9e2f6538a0c4b1e9d7f2a5c3e8b6d04",
            "prompt": "Focus on the auth changes and the new migration."
        } }
      ]
    }
  }
}
```
Use `{{$guid}}` (Postman's built-in dynamic variable) so `messageId` is always unique — it's a required field. Replace `repo`/`pr`/`headSha` with a real **approved** repo in this Lightbridge install, or expect `TASK_STATE_REJECTED`.

Tests script (included) captures the task id:
```javascript
const body = pm.response.json();
const task = body.result ? body.result.task : body.task;
if (task) pm.collectionVariables.set("task_id", task.id);
pm.test("status is SUBMITTED or REJECTED", () =>
  pm.expect(["TASK_STATE_SUBMITTED", "TASK_STATE_REJECTED"]).to.include(task.status.state));
```

**GetTask** — body:
```json
{ "jsonrpc": "2.0", "id": 2, "method": "GetTask", "params": { "id": "{{task_id}}" } }
```
Run repeatedly (Postman Runner, or just hit Send again) until `status.state` is terminal. On `TASK_STATE_COMPLETED`, `result.task.artifacts[0].parts` has the summary/findings/context — see §4.3.

**CancelTask** — body:
```json
{ "jsonrpc": "2.0", "id": 3, "method": "CancelTask", "params": { "id": "{{task_id}}" } }
```

### 8.4 Folder: `3 — Review skill (REST)`

Equivalent REST calls, for comparing transports:
- `POST {{a2a_base}}/message:send` — body is just the `message` object (no JSON-RPC envelope).
- `GET {{a2a_base}}/tasks/{{task_id}}`
- `POST {{a2a_base}}/tasks/{{task_id}}/cancel`

### 8.5 Folder: `4 — Streaming (SSE)`

`POST {{a2a_base}}/message:stream` with the same body as `SendMessage`, or `POST {{a2a_base}}/tasks/{{task_id}}/subscribe` for an existing task.

**Postman caveat:** Postman's native SSE support varies by version/platform — in some Desktop builds a `POST` that returns `text/event-stream` just streams into the response body pane rather than being parsed frame-by-frame. If that's the case for you, this is one place I'd actually reach for `curl -N` instead to watch frames arrive live:
```bash
curl -N -s "$A2A_BASE/message:stream" \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{ "message": { "messageId": "'$(uuidgen)'", "role": "ROLE_USER", "parts": [ { "data": { "skill":"review","repo":"acme/api","pr":"164","headSha":"…" } } ] } }'
```
Expect the ordered frame sequence: one `task` snapshot → `statusUpdate` frames (`SUBMITTED→WORKING→…`) → `artifactUpdate` on completion → stream close.

### 8.6 Folder: `5 — Push notifications`

**CreateTaskPushNotificationConfig** — `POST {{a2a_base}}/tasks/{{task_id}}/pushNotificationConfigs`:
```json
{ "taskId": "{{task_id}}", "url": "https://webhook.site/<your-unique-id>", "token": "optional-shared-secret" }
```
Grab a free bin at [webhook.site](https://webhook.site) to watch deliveries land in real time — remember the URL policy: **HTTPS, port 443, publicly resolvable** only (no `localhost`/RFC1918/webhook.site-behind-VPN tricks). Then re-run `SendMessage` (or wait on an in-flight task) and watch the bin receive `StreamResponse` POSTs with header `X-A2A-Notification-Id: {taskId}:{seq}`.

**List/Delete** the same way against `.../pushNotificationConfigs` and `.../pushNotificationConfigs/{configId}`.

### 8.7 Expected failure-mode checks worth running once

| Test | Expect |
|---|---|
| `SendMessage` with no `Authorization` header | `401` |
| `SendMessage` with a garbage bearer | `401` |
| `SendMessage` with a valid token lacking `a2a:review` | task returned, `status.state = TASK_STATE_REJECTED` (not a `4xx`!) |
| `SendMessage` omitting `headSha` | `TASK_STATE_REJECTED` |
| `SendMessage` with an unapproved `repo` | `TASK_STATE_REJECTED` |
| `SendMessage` with `role: "user"` instead of `"ROLE_USER"` | transport-level `INVALID_PARAMS`/`400` — enums are ProtoJSON SCREAMING_SNAKE, this is documented as the #1 wire gotcha |
| `GetTask` for someone else's / an unknown task id | clean `TaskNotFound`, no existence leak |

---

## Appendix: files referenced

- `docs/rfc/0006-a2a-agent-surface.md` — design RFC
- `docs/a2a-review-skill.md` — the canonical calling guide (this report's §8 mirrors it)
- `docs/adr/0077-a2a-streaming-event-log.md`, `0078-a2a-natural-language-text-part.md`, `0079-a2a-push-notifications-webhook-egress.md`, `0081-a2a-input-required-and-list-tasks.md`
- `services/control-plane/src/a2a/*.rs`
- `~/ai/ai-helm/charts/lightbridge-code-intelligence/values.yaml` (chart defaults)
- `~/ai/ai-helm-values/environments/prod/values/lightbridge-code-intelligence.yaml` (deployed values)
- Live cluster: context `hetzner-prod`, namespace `converse`, deployment `lightbridge-ci-a2a`
