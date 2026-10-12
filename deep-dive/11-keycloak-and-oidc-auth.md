# 11. Keycloak / OIDC authentication — the general concepts and this codebase's exact wiring

This chapter covers how this monorepo authenticates people and services: Keycloak as the identity
provider, OIDC/OAuth2 as the protocol, and the four places that protocol shows up in the actual
code — the Next.js web app (`apps/web`), the shared TypeScript client (`packages/auth`), the Rust
control plane (`services/control-plane`), and a standalone TUI/CLI client (`clients/lci`). It
builds on file `01`'s role-selection framing (`OIDC_ISSUER` gating the `a2a`/`mcp` roles) and
file `10`'s exact memory layout of `AppState.jwt`/`AppState.permissions_claim` — both are cited
below rather than re-derived.

## 11.1 The general concepts, just enough to read the rest of this file

**OIDC (OpenID Connect)** is a thin identity layer on top of **OAuth2**. OAuth2 by itself is about
*authorization* — handing a client a token that lets it act on a resource owner's behalf; OIDC adds
*authentication* — a standard way for the client to learn *who* signed in, via an **ID token**, a
signed JWT describing the person. In practice here, the thing that actually matters is simpler: one
**identity provider** (Keycloak) issues **access tokens** that other services verify; nobody but
Keycloak ever sees a password.

- **Authorization Code flow + PKCE** — the browser is redirected to Keycloak to log in; Keycloak
  redirects back with a one-time `code`; the app's *server* (never the browser) exchanges that code
  for tokens. **PKCE** (`code_challenge`/`code_verifier`) stops a stolen authorization code from
  being replayed by anyone who doesn't also hold the original random verifier — needed because a
  **public client** (no client secret, the default here) has no other way to prove it's the same
  party that started the flow.
- **Access token vs ID token** — an access token is the bearer credential sent to a resource server
  on every API call (`Authorization: Bearer <token>`); an ID token asserts identity to the client
  itself. This codebase is RS256-JWT-access-token-only on the wire — it reads identity straight off
  the access token's claims (`sub`, `email`, ...) rather than carrying a separate ID token around.
- **JWKS (JSON Web Key Set) / issuer discovery** — Keycloak publishes its current public signing
  keys at a well-known JWKS URL and its endpoints at `{issuer}/.well-known/openid-configuration`.
  A verifier fetches the JWKS once, checks a token's signature against the key named by the token's
  `kid` header, and re-fetches on an unrecognized `kid` — this is how key rotation works without a
  deploy on either side.
- **Claims** — the JSON payload inside the JWT (`sub`, `email`, `exp`, `aud`, plus whatever the IdP
  is configured to add). Every verifier here treats `iss`/`aud`/`exp`/signature as the trust
  boundary and reads everything else, including a custom **permissions** claim (§11.4), straight out
  of that payload.
- **Refresh tokens** — a longer-lived credential, exchanged at the token endpoint for a new access
  token without a fresh login. Used by the web proxy (§11.2) and the CLI (§11.5); the control plane
  never sees or handles one — it only ever validates the access token handed to it.

## 11.2 `apps/web` — the OIDC client

ADR-0014 (`docs/adr/0014-keycloak-oidc-resource-server.md:34`) frames the whole system in one line:
**Keycloak issues tokens, the web app is an OIDC client, the control plane is a pure OAuth2
resource server.** The web app runs the Authorization-Code+PKCE exchange in Node route handlers
and stores the result in httpOnly cookies; it never touches a password.

### Login

`apps/web/app/api/auth/login/route.ts:10-30` starts the flow:

```ts
// apps/web/app/api/auth/login/route.ts:10-29
export async function GET() {
  const { config, clientConfig } = await getOidc();
  const codeVerifier = client.randomPKCECodeVerifier();
  const codeChallenge = await client.calculatePKCECodeChallenge(codeVerifier);
  const state = client.randomState();
  const authorizationUrl = client.buildAuthorizationUrl(config, {
    redirect_uri: clientConfig.redirectUri,
    scope: clientConfig.scope,
    code_challenge: codeChallenge,
    code_challenge_method: "S256",
    state,
  });
  const res = NextResponse.redirect(authorizationUrl.href);
  res.cookies.set(PKCE_COOKIE, codeVerifier, cookieOptions(600));
  res.cookies.set(STATE_COOKIE, state, cookieOptions(600));
  return res;
}
```

`getOidc()` (`apps/web/lib/auth/oidc.ts:13-31`) runs `openid-client`'s `discovery()` once per
server instance and caches the resulting `Configuration` — this is the actual issuer-discovery step
(`.well-known/openid-configuration`), memoized so every login doesn't re-fetch it. The verifier and
state ride in short-lived (`600`s) httpOnly cookies so the callback can check them.

`route.ts:7` pins `export const runtime = "nodejs"` — the comment on `oidc.ts:9` explains why:
`openid-client` is not Edge-safe, so this leg of the flow is explicitly Node-only. Token
*verification* (§11.2's proxy, below) is the Edge-safe half, built on `jose` instead.

### Callback

`apps/web/app/api/auth/callback/route.ts:15-62` handles the redirect back from Keycloak:

```ts
// apps/web/app/api/auth/callback/route.ts:33-48 (trimmed)
tokens = await client.authorizationCodeGrant(config, callbackUrl, {
  pkceCodeVerifier: codeVerifier,
  expectedState,
});
const maxAge = typeof tokens.expires_in === "number" ? tokens.expires_in : 1800;
const res = NextResponse.redirect(new URL("/dashboard", appOrigin));
res.cookies.set(SESSION_COOKIE, tokens.access_token, cookieOptions(maxAge));
if (tokens.refresh_token) {
  res.cookies.set(REFRESH_COOKIE, tokens.refresh_token, cookieOptions(refreshMaxAge));
}
```

Two details worth citing precisely:

- **The cookie *is* the access token**, not a session id pointing at server-side state
  (`callback/route.ts:48`'s own comment: "The access token is the bearer credential sent to the
  control plane ... and the value the proxy validates"). There is no server-side session store on
  the web tier — `SESSION_COOKIE` (`lb_session`, `packages/auth/src/session-cookie.ts:2`) carries
  the real RS256 JWT, cookie lifetime tracking the token's own `expires_in`.
  REFRESH_COOKIE` (`lb_refresh`, `session-cookie.ts:3`) carries the refresh token.
- **`callbackUrl` is rebuilt from the *configured* redirect URI, not `req.url`**
  (`callback/route.ts:17-19,27-31`) — behind an ingress, `req.url`'s host is the pod's internal
  bind address, which would send the token exchange's `redirect_uri` out of sync with the one used
  to start the flow and Keycloak would reject it (`invalid_grant`). The same anchoring pattern
  recurs in `oidc-config.ts:57-63` (`appBaseUrl()`) and in `proxy.ts` below — a single running theme
  for anything that has to match a URL Keycloak was told about up front.

### Logout

`apps/web/app/api/auth/logout/route.ts:9-26` clears both cookies and redirects to Keycloak's
`end_session_endpoint` via `client.buildEndSessionUrl` — RP-initiated single logout, not just a
local cookie clear, so Keycloak's own session is terminated too (falling back to a local redirect
if the discovered provider has none).

### Protecting `/dashboard/*` — and a real doc/code drift

ADR-0014 (`docs/adr/0014-keycloak-oidc-resource-server.md:43`) says: *"the access token rides in an
httpOnly cookie; `middleware.ts` validates it with `jose` (Edge runtime) on protected routes."* That
sentence is **stale relative to the live source**: there is no `apps/web/middleware.ts`. The file
that does this job today is **`apps/web/proxy.ts`**:

```ts
// apps/web/proxy.ts:13-19
// Protect the dashboard (and anything under it). Next.js 16 Proxy defaults to the Node.js
// runtime; this file only uses Edge-safe `jose` and `fetch`, so it works in either.
export const config = { matcher: ["/dashboard", "/dashboard/:path*"] };

export async function proxy(req: NextRequest) {
  const token = req.cookies.get(SESSION_COOKIE)?.value;
  const claims = token ? await verifyAccessToken(token, verifyConfigFromEnv()) : null;
```

Next.js 16 renamed the `middleware.ts` convention to a `proxy.ts` export named `proxy` (the file's
own comment at line 14 says so explicitly, and `apps/web/AGENTS.md` separately warns that this
Next.js version has convention changes from what any training data would expect). The *mechanism*
ADR-0014 describes is otherwise accurate — Edge-safe `jose`-based verification on protected
routes — only the filename changed. This same stale `middleware.ts` reference also appears in
`packages/auth/README.md:21` and in the doc comment at `packages/auth/src/oidc-config.ts:19`
("Silent renewal (`middleware.ts`) runs on a session-bound refresh token...") and in
`docs/architecture.md:189`. None of these were updated when the file was renamed; this deep-dive
calls it out rather than silently treating `proxy.ts` as if the docs had always said so.

`proxy.ts:17-66` does three things, not just verification:

1. Verify the session cookie's token (`verifyAccessToken`, §11.3). If valid, let the request through.
2. If invalid/absent but a `REFRESH_COOKIE` exists, call `performRefreshGrant` (§11.3) against
   Keycloak's token endpoint; on success it re-sets both cookies and lets the request through — this
   is the **silent renewal** the stale comment above was describing.
3. If both fail, redirect to `/api/auth/login`, deleting `SESSION_COOKIE` — but **deliberately not**
   `REFRESH_COOKIE` on the refresh-failure path:

```ts
// apps/web/proxy.ts:49-54
const res = NextResponse.redirect(new URL("/api/auth/login", appBaseUrl()));
res.cookies.delete(SESSION_COOKIE);
// We explicitly do NOT delete REFRESH_COOKIE here to gracefully handle rotation races.
// If two tabs refresh concurrently, one fails with invalid_grant. If we deleted it here,
// we would wipe out the new valid cookie set by the winning tab.
```

That comment is worth sitting with: it's a genuine multi-tab race in refresh-token rotation, and
the fix is "don't touch the cookie you didn't just set," not a mutex — there's no shared state to
lock across two browser tabs making independent requests.

### Reading the session, server-side

`apps/web/lib/auth/session.ts:10-14`'s `currentClaims()` is the read-side counterpart to the
proxy — any server component downstream of `/dashboard/*` calls it to get typed claims back out of
the already-validated cookie:

```ts
// apps/web/lib/auth/session.ts:10-14
export async function currentClaims(): Promise<SessionClaims | null> {
  const token = (await cookies()).get(SESSION_COOKIE)?.value;
  if (!token) return null;
  return verifyAccessToken(token, verifyConfigFromEnv());
}
```

Note it **re-verifies** the token rather than trusting that `proxy.ts` already did — there is no
shared per-request cache between the proxy and the page render; each independently calls
`verifyAccessToken`. `apps/web/app/sign-in/page.tsx:12` uses it to skip the sign-in prompt for an
already-authenticated visitor; `apps/web/app/dashboard/settings/page.tsx:12,25-37` uses it to render
the account panel (name/email/username/subject) and the permissions chips (§11.4);
`apps/web/app/dashboard/layout.tsx:8-13` uses it plus `hasPermission` to decide whether the admin
nav item renders.

**Nothing in `apps/web` calls better-auth.** ADR-0007 (`docs/adr/0007-better-auth-rust-backend-plugin.md:1,6-10`)
describes a `better-auth` + custom `rust-backend` plugin that POSTed credentials to the control
plane's own `/auth/verify`; its own header states it is **superseded** by ADR-0014, and that "the
better-auth `rust-backend` plugin and the first-party `/auth/verify` credential store... have been
removed." A repo-wide search for `better-auth`/`betterAuth` under `apps/web` and `packages/auth`
turns up nothing — the whole stack here is `openid-client` (Node, token exchange) + `jose` (Edge/Node,
verification), with no better-auth dependency at all. The `CLAUDE.md` project-root description
("`apps/web` (Next.js + better-auth)") and the task brief that cites it are themselves stale in the
same direction as ADR-0007's own superseded body — worth flagging alongside the `middleware.ts`
drift as a second doc/code mismatch in this area, not silently corrected.

## 11.3 `packages/auth` — the shared TypeScript OIDC client

This is the one package all of `apps/web`'s auth code imports from (`@lightbridge/auth`,
`packages/auth/src/index.ts:1-14`); nothing here is Edge-unsafe, so every export is usable from both
`proxy.ts` (Edge-capable) and the Node route handlers.

| File | What it owns |
|---|---|
| `oidc-config.ts` | `OidcClientConfig`, `oidcClientConfigFromEnv()`, `oidcTokenUri()`, `appBaseUrl()` |
| `verify-jwt.ts` | `VerifyConfig`, `verifyAccessToken()`, `verifyConfigFromEnv()` |
| `claims.ts` | `SessionClaims` |
| `refresh-grant.ts` | `performRefreshGrant()`, `RefreshGrantResult`, `AuthResult<T>` |
| `session-cookie.ts` | `SESSION_COOKIE`/`REFRESH_COOKIE`/`PKCE_COOKIE`/`STATE_COOKIE`, `cookieOptions()` |

**`verifyAccessToken`** (`packages/auth/src/verify-jwt.ts:36-54`) is the actual verification:

```ts
// packages/auth/src/verify-jwt.ts:41-48
const { payload } = await jwtVerify(token, remoteJwks(config), {
  issuer: config.issuer,
  audience: config.audience,
  // Pin the expected signature algorithm. jose v6 no longer infers a default
  // from the key type, and the access tokens issued by our Keycloak realm are
  // RS256-signed. This prevents algorithm-confusion attacks.
  algorithms: ["RS256"],
});
```

It uses `jose`'s `createRemoteJWKSet`, one cached per JWKS URI (`verify-jwt.ts:19,21-30`) — `jose`
itself handles the fetch-on-unknown-`kid` + cache behavior internally, so this module doesn't
reimplement key-rotation logic the way the Rust `JwtValidator` (§11.4) explicitly does. Any failure
— missing token, bad signature, wrong issuer/audience, expired — collapses to `null`
(`verify-jwt.ts:51-53`); the comment pinning `algorithms: ["RS256"]` is calling out a real
`jose` v6 behavior change (no more inferring the algorithm from key type), guarding against an
algorithm-confusion attack class.

**`performRefreshGrant`** (`packages/auth/src/refresh-grant.ts:25-75`) is a plain `fetch` POST to
the token endpoint with `grant_type=refresh_token` — "fully Edge-runtime compatible" per its own
doc comment (line 23), which is why `proxy.ts` can call it directly. It classifies the response into
`"unauthenticated" | "unavailable" | "error"` (`refresh-grant.ts:15-19`) rather than a bare boolean,
so the proxy can tell "refresh token is dead, go log in" apart from "token endpoint is down."

**`SessionClaims`** (`packages/auth/src/claims.ts:5-15`) types only the fields the app actually
reads (`sub`, `email?`, `preferred_username?`, `name?`, `exp`) and then an index signature
(`[claim: string]: unknown`) for everything else — explicitly so the configurable permissions claim
(§11.4) can be read by an arbitrary dotted path without the type fighting the access pattern.

Nothing in this package stores a credential or mints a token — it is read-only (verify) plus a
thin wrapper over one token-endpoint POST (refresh). `packages/auth` has no test files (none found
under `packages/auth/**/*.test.ts`), unlike the Rust side's `jwt.rs` (§11.4), which carries a full
unit-test module.

## 11.4 The Rust control plane — pure resource server, plus permission-based authz

### Validating the token (`jwt.rs`)

File `10` already named the field: `AppState.jwt: Option<Arc<JwtValidator>>`
(`deep-dive/10-appstate-and-rust-shared-state.md` §10.3, `main.rs:60`) — `None` whenever
`OIDC_ISSUER` is unset, and file `01` already covers two roles (`a2a`, `mcp`) that refuse to start
at all without it (`deep-dive/01-control-plane-roles.md` lines around 261-262/370-371: *"the a2a
role requires OIDC_ISSUER (no anonymous access)"*). This file is about what that `Arc<JwtValidator>`
actually does.

`jwt.rs:1-6` states the role precisely: *"The control plane is a pure resource server: it does not
issue tokens or store credentials. It validates RS256 access tokens minted by an external OIDC
provider... against the provider's published JWKS."* `OidcConfig::from_env()`
(`jwt.rs:91-104`) is the `Option`-returning constructor that makes `AppState.jwt` fail closed:

```rust
// services/control-plane/src/jwt.rs:91-104
pub fn from_env() -> Option<Self> {
    let issuer = std::env::var("OIDC_ISSUER").ok()?.trim_end_matches('/').to_string();
    let audience = std::env::var("OIDC_AUDIENCE").unwrap_or_else(|_| "account".to_string());
    let jwks_uri = std::env::var("OIDC_JWKS_URI")
        .unwrap_or_else(|_| format!("{issuer}/protocol/openid-connect/certs"));
    Some(Self { issuer, audience, jwks_uri })
}
```

`JwtValidator` (`jwt.rs:136-233`) caches `kid → DecodingKey` in a `tokio::sync::RwLock<HashMap<...>>`
(an async-aware lock, unlike `AppState.seen_deliveries`'s `std::sync::Mutex` from file `10` §10.5 —
here the critical sections genuinely do span `.await` points, since refreshing the JWKS is a network
call). `decoding_key()` (`jwt.rs:192-207`) is the actual rotation handling: an unknown `kid` triggers
one `refresh()` against the JWKS URL, and **only** a `kid` still missing *after* a successful
refresh is treated as a bad token (`AuthError::InvalidToken`, 401); a refresh that itself fails
(IdP unreachable) is `AuthError::JwksUnavailable` (503) — a deliberately different, retryable error
so a transient IdP outage never gets permanently misclassified as "this caller's token is garbage."
`validate()` (`jwt.rs:209-221`) does the actual `jsonwebtoken::decode` with issuer + audience pinned
via `Validation::set_issuer`/`set_audience`.

The test module (`jwt.rs:374-548`) earns its keep: it mints real RS256 tokens against a
runtime-generated RSA keypair (no key material committed, `jwt.rs:317-339`) and separately exercises
wrong issuer, wrong audience, expired token, unknown `kid` against an unreachable JWKS endpoint (the
`JwksUnavailable`-not-`InvalidToken` distinction above), and a bit-flipped signature. There is also a
`#[cfg(test)] pub(crate) mod test_support` (`jwt.rs:307-372`) — a second, *reusable* copy of the same
keypair-generation and minting logic, exported crate-internally so other modules' tests (the `a2a`
auth layer, below) can drive the real validation path without a live IdP, rather than each module
hand-rolling its own token minter.

### Permissions-in-claims, not roles (ADR-0023)

ADR-0023 (`docs/adr/0023-db-backed-rbac.md`) is the record of a genuine design pivot, worth reading
in its own words: an *earlier draft* of the same ADR proposed a roles→permissions table in this
service's own database (hence the filename), and was **replaced before shipping** by a simpler
rule, per the decision owner (`0023-db-backed-rbac.md:13-16`): *"there are no role names on our
side — the token carries a flat list of permissions directly, and we simply enforce them. The
IdP / Envoy AI Gateway owns whatever role→permission policy it likes; we consume the result."* The
filename itself is a fossil of the superseded draft, same category of drift as file `02`'s
`outbox`/`github_outbox` naming note that file `00` points to for calibration — not corrected here,
just named.

Why this over roles-in-a-local-table: a role name is an indirection the control plane would have to
define, store, and keep in sync with the IdP's own role model; a flat permission string
(`repo:approve`, `task:cancel`, ...) is the thing a handler actually needs to know, and asking the
IdP to assert it directly means **authorization is a pure function of the verified token** — no
database read, no admin UI to manage roles, no role-definition drift between this service and the
IdP (`0023-db-backed-rbac.md:83-87`'s "Consequences": *"dead simple... no DB, no policy UI to
build"*). The cost, stated plainly in the same ADR, is that the role→permission mapping now lives
entirely in the IdP/gateway and can't be inspected or managed from inside this app
(`0023-db-backed-rbac.md:88-89`).

The claim path is itself configurable — `PERMISSIONS_CLAIM` (default `permissions`, dotted paths
supported for e.g. `code_intelligence.permissions`, `0023-db-backed-rbac.md:38-40`) — because the
ADR explicitly keeps the token shape swappable per IdP/deployment rather than hard-coding Keycloak's
own default claim layout.

`Claims::permissions()` (`jwt.rs:55-75`) is the dotted-path reader:

```rust
// services/control-plane/src/jwt.rs:55-75
pub fn permissions(&self, claim_path: &str) -> std::collections::HashSet<String> {
    let mut node: Option<&serde_json::Value> = None;
    for (i, segment) in claim_path.split('.').enumerate() {
        node = if i == 0 { self.extra.get(segment) } else { node.and_then(|n| n.get(segment)) };
        if node.is_none() { return std::collections::HashSet::new(); }
    }
    node.and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}
```

It walks `Claims.extra` — a `#[serde(flatten)]` catch-all (`jwt.rs:39-40`) that captures every claim
the typed struct doesn't name — and returns an **empty set** on anything unexpected: claim absent,
wrong JSON type, path doesn't resolve. That emptiness is the fail-closed property ADR-0023's
"Migration / cutover" section calls out explicitly (`0023-db-backed-rbac.md:76-78`): *"Until [the IdP
emits the claim], a token without the claim has no permissions → /admin/* 403s and the read
endpoints 403 (fail-closed)."* The unit test at `jwt.rs:456-480` exercises exactly this: a top-level
claim, a nested dotted path, a missing claim, and a wrong-typed (`"not-an-array"`) claim, asserting
the last two both yield an empty set.

### The `Caller` extractor — ADR-0023's "authorization seam"

ADR-0023 names the seam directly (`0023-db-backed-rbac.md:63-66`): replace the old realm-role
`Admin` extractor with a `Caller` extractor carrying verified `Claims` plus the parsed permission
set, with a `require(perm)` method. That is exactly `jwt.rs:250-279`:

```rust
// services/control-plane/src/jwt.rs:250-279
#[derive(Clone)]
pub struct Caller {
    pub claims: Claims,
    pub permissions: std::collections::HashSet<String>,
}

impl Caller {
    pub fn require(&self, permission: &str) -> Result<(), AuthError> {
        if self.permissions.contains(permission) { Ok(()) } else { Err(AuthError::Forbidden) }
    }
}

impl FromRequestParts<AppState> for Caller {
    type Rejection = AuthError;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AuthError> {
        let claims = Claims::from_request_parts(parts, state).await?;
        let permissions = claims.permissions(&state.permissions_claim);
        Ok(Caller { claims, permissions })
    }
}
```

`Caller`'s own `FromRequestParts` impl *composes* `Claims`'s — it first runs the exact same
bearer-token extraction and JWKS validation `Claims` does (`jwt.rs:237-245`), then reads permissions
off the result using `state.permissions_claim` — the `Arc<String>` field file `10` already placed in
`AppState`'s memory layout (`deep-dive/10...md` §10.3's struct listing, `main.rs:76`). There is only
one validation path in this service; `Caller` is a thin, composed extractor on top of it, not a
second one.

Every admin handler in `services/control-plane/src/http/admin.rs` takes `Caller` as an axum
extractor argument and calls `.require(...)` as its first line — e.g.
`admin.rs:34,38` (`caller.require("repo:read")`), `admin.rs:56,60` (`"repo:approve"`),
`admin.rs:68-69` (`"repo:deny"`), `admin.rs:564,569` and `admin.rs:624,629`
(`"model:configure"` — ADR-0110's model-override surface, a *different* permission from
`repo:configure` specifically because model selection is operator-cost-relevant, per the comment at
`apps/web/lib/server/admin.ts:302-305`'s mirror of the same rule). `jwt.rs:293-297`'s `me()` handler
is the one endpoint every authenticated caller can reach regardless of permission — it returns the
caller's own verified claims plus their effective permission set, which is exactly what
`apps/web/app/dashboard/settings/page.tsx:58-64`'s "Access" panel and
`apps/web/lib/server/admin.ts:27-50`'s `permissions()`/`hasPermission()` mirror client-side (reading
the *same* `PERMISSIONS_CLAIM`-named env var, `admin.ts:28`) — purely to decide what UI to show; the
control plane, per ADR-0023's own framing, remains the actual enforcement point no matter what the
web renders.

### A second integration shape, same validator: the `a2a` role

The task brief's premise that `services/a2a-probe` might be "a second, independent
JWT-verification consumer" does not hold up against the source: `services/a2a-probe/src/lib.rs` is
an **evaluation probe** (its own header, `lib.rs:1-7`, calls it "THROWAWAY" — answering whether the
official `a2a-server-lf` SDK's `TaskStore` trait is pluggable enough to back with Postgres, RFC-0006
/ #298). It contains **no JWT validation code at all** — a repo-wide grep for `jwt|jsonwebtoken|decode|Validation`
under `services/a2a-probe/` returns nothing. Its only auth-adjacent content is a placeholder OIDC
discovery URL baked into a fake agent card (`lib.rs:66-69`), explicitly commented *"kept
obviously-fake so nobody mistakes the probe card for a live one."*

The *real* second integration is the shipped `a2a` role under
`services/control-plane/src/a2a/`, and it's a genuinely different **shape**, not different
*logic*. The A2A SDK's `RequestHandler` trait methods aren't axum handlers, so they can't take a
`Caller`/`Claims` extractor argument the way `admin.rs` does. Instead, `build_router`
(`services/control-plane/src/a2a/mod.rs:133-159`) wraps the SDK's own JSON-RPC + REST routers in an
axum **middleware layer**:

```rust
// services/control-plane/src/a2a/mod.rs:155-159
let protected = a2a_server::rest::rest_router(handler.clone())
    .merge(a2a_server::jsonrpc::jsonrpc_router(handler))
    .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
    .layer(axum::middleware::from_fn_with_state(auth_state, a2a_auth));
```

`a2a_auth` (`a2a/mod.rs:166-212`) strips any inbound copy of two internal headers
(`x-lb-a2a-caller`/`x-lb-a2a-perms`, `HDR_CALLER`/`HDR_PERMS`, `a2a/mod.rs:48,51`) as an anti-spoof
measure, then calls **the same `auth.jwt.validate(&token)`** (`JwtValidator::validate`, `jwt.rs:209`)
and **the same `claims.permissions(&auth.permissions_claim)`** (`jwt.rs:55`) the REST admin surface
uses, and re-injects the result as those two headers before handing the request to the SDK's own
router. `A2aHandler::caller()` (`services/control-plane/src/a2a/handler.rs:240-258`) then reads
those headers back out of the SDK's `ServiceParams` to reconstruct a `CallerCtx { id, perms }`
(`handler.rs:186-189`) — commented explicitly as trusted *because* "a request without them never
reached here in production (the layer 401s first)." So: one validator, one permissions reader,
two different plumbing mechanisms for getting the result to the handler — `FromRequestParts` for
plain axum handlers, a middleware-plus-internal-header relay for a third-party SDK's own routing.

The `review` skill's permission check (`a2a/handler.rs:41` `const REVIEW_PERMISSION: &str =
"a2a:review"`, checked at `a2a/handler/lifecycle.rs:100-112`) is deliberately **not** a transport
error on failure — it returns a normal `SendMessageResponse` carrying a `TASK_STATE_REJECTED` task
(`lifecycle.rs:107-113`), per the comment there: the caller authenticated fine, they're just not
allowed, and RFC-0006's state table wants that surfaced as a task outcome, not a 403. This is a
genuine difference from the REST admin surface's `AuthError::Forbidden` → HTTP 403
(`jwt.rs:129,259-263`) — same permission model, different failure shape because the two are
different protocols.

`services/control-plane/src/a2a/card.rs` is the **unauthenticated** discovery document served at
`/.well-known/agent-card.json` (card.rs:1-9) — it advertises the `keycloak-oidc` security scheme
(`card.rs:18,60-69`) pointing at the real `{issuer}/.well-known/openid-configuration`
(`a2a/mod.rs:158-161`'s `oidc_discovery_url()`, called with `std::env::var("OIDC_ISSUER")`), and its
description names the extra `a2a:review` permission requirement in prose (`card.rs:66`). Like the
probe, the card module itself performs no validation — it only *describes* where a caller should go
to get a token; the middleware above is what actually checks one.

## 11.5 A third, independent OIDC client: `clients/lci`

Worth a brief mention since the task asked for "everywhere it appears": `clients/lci` is a
standalone TUI/CLI client (`deploy/config-reference.md:362-372`, separate from the deployed
platform) that is its **own** OIDC client — not a `packages/auth` consumer (it's Rust, not
TypeScript) and not sharing code with `services/control-plane/src/jwt.rs` either (that module
*verifies* tokens; this one *obtains* them, a different job). `clients/lci/src/auth/mod.rs:1-2`
states it plainly: *"Hand-rolled (no `oauth2` crate) — everything is plain reqwest + sha2/base64."*

Its flow is Authorization-Code+PKCE like the web app, but adapted for a native CLI: it binds a
**loopback TCP listener** on `127.0.0.1:{LCI_REDIRECT_PORT}` (default `8765`,
`deploy/config-reference.md:371`) *before* printing the authorize URL, so the port is guaranteed
available before the browser is even opened (`clients/lci/src/auth/mod.rs:149-157`), then opens the
system browser (`open::that`), waits on that listener for the redirect carrying the code, and
exchanges it — the same code-for-token exchange the web app's callback route does, just received
over a one-shot local HTTP server instead of a Next.js route handler. Tokens are cached to disk
(`clients/lci/src/auth/store.rs`) and refreshed silently on a stale-but-refreshable cache
(`auth/mod.rs:63-77`'s `ensure_token()` ladder: fresh cache → refresh → interactive login) — the same
three-rung fallback shape `proxy.ts` uses server-side, independently reinvented for a CLI context
that has no shared cookie jar.

## 11.6 Local dev and deployment wiring

### Local dev: Keycloak itself, in `compose.yaml`

```yaml
# compose.yaml:39-58
keycloak:
  # OIDC provider for local dev (ADR-0014). The web app is an OIDC client and the control plane a
  # resource server; both trust tokens this realm issues. Imports the `lightbridge` realm on boot.
  # Exposed on 8081 to avoid clashing with the control plane (8080).
  image: quay.io/keycloak/keycloak:26.0
  command: ["start-dev", "--import-realm"]
  environment:
    KC_BOOTSTRAP_ADMIN_USERNAME: admin
    KC_BOOTSTRAP_ADMIN_PASSWORD: admin
    KC_HTTP_PORT: "8080"
  ports:
    - "8081:8080"
  volumes:
    - ./deploy/keycloak/realm-lightbridge.json:/opt/keycloak/data/import/realm-lightbridge.json:ro
```

`--import-realm` plus the mounted `realm-lightbridge.json` means the dev realm — its clients,
users, and (per ADR-0023's migration note) whatever claim mapper emits the `permissions` claim — is
fully declared in that one committed JSON file (`deploy/keycloak/realm-lightbridge.json`,
referenced again at `deploy/config-reference.md:305` as where `OIDC_CLIENT_ID` is pre-seeded), not
clicked together by hand on every fresh checkout. Keycloak is exposed on host port `8081`
specifically so it doesn't collide with the control plane's own `8080`
(`compose.yaml:42`'s comment) — and `KC_HTTP_PORT: "8080"` is Keycloak's *internal* container port,
mapped out to that different host port, which is why `apps/web/.env.example`'s
`OIDC_ISSUER=http://localhost:8081/realms/lightbridge` uses `8081`.

### Env vars that wire a deployment to a real Keycloak

`deploy/config-reference.md` documents three independent OIDC surfaces, each with its own table:

**Control plane** (`config-reference.md:65-73`, "OIDC / auth"):

| Variable | Default | Notes |
|---|---|---|
| `OIDC_ISSUER` | unset → auth disabled, fails closed | Keycloak realm URL |
| `OIDC_AUDIENCE` | `account` | expected JWT `aud` |
| `OIDC_JWKS_URI` | `{issuer}/protocol/openid-connect/certs` | override |
| `PERMISSIONS_CLAIM` | `permissions` | dotted claim path (ADR-0023) |

**Web console** (`config-reference.md:304-314`, "Web console (`apps/web`)"): `OIDC_ISSUER`,
`OIDC_CLIENT_ID`, `OIDC_CLIENT_SECRET` (unset → public/PKCE client), `OIDC_REDIRECT_URI`,
`OIDC_POST_LOGOUT_REDIRECT_URI`, `OIDC_SCOPE`, `OIDC_JWKS_URI`, `OIDC_TOKEN_URI`, `OIDC_AUDIENCE`,
and `PERMISSIONS_CLAIM` again — explicitly noted there as mirroring the control plane's own claim
path, so a deployment that changes one must change both or the web's affordance-hiding and the
control plane's real enforcement will disagree about what the caller can do (the control plane
still wins in that case — it's the enforcement point regardless of what the web thinks).

**`lci` CLI** (`config-reference.md:362-372`): its own `OIDC_ISSUER`/`OIDC_CLIENT_ID`, defaulting to
a *production* issuer (`https://auth.verif.fyi/realms/camer-digital`) and a dedicated public client
id (`lightbridge-cli`) — i.e. the CLI, unlike the web app and control plane, ships with a real
deployed issuer as its default rather than `localhost`, because it's meant to be run against the
live service out of the box.

No `docs/adr/` entry is dedicated solely to "why Keycloak specifically" as a product (as opposed to
"why OIDC generally") — ADR-0014's "Considered Options" section
(`docs/adr/0014-keycloak-oidc-resource-server.md:22-30`) folds that choice into the main OIDC
decision: Keycloak was picked over a hosted IdP (Auth0/Entra/Okta) because it's self-hostable, "fits
the banking-grade/adorsys context," and — being itself OIDC-compliant — can federate to one of
those hosted IdPs later without a code change if ever needed. There is no separate ADR number for
that sub-choice; it's ADR-0014 end to end.

## 11.7 Summary table — one validator, four callers

| Caller | Language | Obtains a token via | Verifies via |
|---|---|---|---|
| `apps/web` (browser session) | TS, Node+Edge | Auth Code+PKCE in `app/api/auth/*` (`openid-client`) | `packages/auth` (`jose`), called from `proxy.ts` and `lib/auth/session.ts` |
| `packages/auth` consumers generally | TS | n/a (library) | `verifyAccessToken` (`jose`, RS256 pinned) |
| `services/control-plane` REST (`/admin/*`, `/tasks*`, `/me`) | Rust | n/a (resource server only) | `JwtValidator::validate` via the `Claims`/`Caller` extractors (`jwt.rs`) |
| `services/control-plane` `a2a` role (JSON-RPC/REST) | Rust | n/a (resource server only) | the *same* `JwtValidator::validate`, relayed through middleware + internal headers (`a2a/mod.rs`) instead of an axum extractor |
| `clients/lci` (TUI/CLI) | Rust | Auth Code+PKCE via a loopback listener, hand-rolled (no `oauth2` crate) | n/a (it's a token holder, not a verifier — the control plane verifies what it sends) |

Every verifier in this table ultimately checks the same three things against the same kind of
RS256 JWT: signature against the IdP's JWKS, `iss`/`aud`, and `exp`. What differs is *who* in the
system is obtaining versus verifying a token, and the plumbing used to carry the result to the
code that needs it — not the cryptographic trust model, which is one Keycloak realm's JWKS, read
independently by two languages and three call shapes.
