# 14. The frontend console — what it is, and how it talks to the control plane

Every earlier file in this set treats the control plane as a backend with no face. It has one: a
Next.js console that reads and writes the same HTTP API a webhook or a human with `curl` would use.
This file covers that console — where it actually lives now, how it's built, exactly which
control-plane endpoints each page calls, what data lives where, and a few of its queries explained.

## 14.1 Where this actually lives now — a cross-repo split

The console is **not** `apps/web` in this repo. `apps/web` still exists on disk here, but its
*deployment* was retired by
[ADR-0115](../docs/adr/0115-retire-apps-web-move-console-to-lci-ui.md) (2026-09-11): a full
replacement, **`apps/lci` in the sibling repo `converse-frontends`**, had already shipped and taken
over the production domain (`lci.ai.camer.digital`, which `code-intelligence.ai.camer.digital` now
301-redirects to). ADR-0115's own words: `apps/lci` "does not cover a narrower slice of `apps/web`'s
job — it covers *both* halves" (the general browsing surface, and the admin/approval surface).

What that ADR's linked infra PRs did, concretely: removed `apps/web`'s Kubernetes Deployment,
Service, and RBAC from the Helm chart, and permanently redirected the old domain. What it explicitly
did **not** do: delete `apps/web`'s source code from this repo (tracked as its own follow-up,
[#646](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/646)) or stop CI from
still building and signing a `lightbridge-web` image nobody deploys. So: if you go looking for the
console's code in *this* repo, you will find a real, building, but **undeployed and unmaintained**
app. The live one is `converse-frontends/apps/lci`, and everything below describes that app.

This is a genuine architectural consequence worth sitting with: the console now lives in a
**different repository** from the control plane it talks to — ADR-0006 never had to account for
that when `apps/web` was co-located. ADR-0115 says so itself: "a cross-repo split...this ADR
documents that reality, it does not resolve the coordination cost." Concretely, that means the
console's TypeScript types for control-plane payloads (`Task`, `Repository`,
`FeedbackAnalyticsResponse`, ...) are hand-maintained copies of the Rust response shapes, not
generated from them — see §14.9 for a real example of the two having drifted.

One more ADR-0115 detail worth flagging, in the spirit of this whole deep-dive set's "call out
doc drift rather than silently pick one" rule: ADR-0115's own text describes `apps/web`'s auth as
"`better-auth`," which disagrees with file [`11`](11-keycloak-and-oidc-auth.md)'s direct source
read of `apps/web`'s actual code (`openid-client` + `jose`, not `better-auth`). This file does not
re-adjudicate that — `apps/web` is out of scope now that it's undeployed — but the discrepancy
between an ADR and a direct source read is exactly the kind of drift worth naming instead of
silently trusting one side.

## 14.2 Shape of the app

`apps/lci` is a Next.js 16 App Router app (`next dev --turbopack -p 3001`), part of the
`converse-frontends` Turborepo monorepo (shared design system in `packages/ui-web`, shared OTel
setup in `packages/otel`). The route tree, under `src/app/(lci)/`:

```
(lci)/                      -- the shell: nav chrome, auth-gated
  page.tsx                  -- Overview (recent runs)
  runs/page.tsx              -- Runs (paginated, filterable)
  runs/[id]/page.tsx          -- One run's detail + its review
  repositories/page.tsx       -- Repositories (paginated)
  repositories/[id]/page.tsx   -- One repository
  repositories/[id]/settings/  -- Per-repo review settings (read-only view)
  repositories/[id]/graph/     -- Structural code-graph canvas
  repositories/[id]/feedback/  -- Per-repo feedback analytics
  admin/page.tsx              -- Pending-repository approval queue
  admin/accepted|denied/       -- The approved/denied lists
  feedback/page.tsx           -- Estate-wide feedback analytics
  settings/page.tsx           -- The caller's own permissions
auth/{login,callback,logout,error}/  -- the OIDC dance (route handlers, see §14.4)
api/repositories/[id]/graph|.../similar/  -- same-origin BFF proxy routes (see §14.6)
```

Almost every page is a **React Server Component**: it `await`s a server-only data function
directly in the component body, with no client-side fetch and no loading spinner for the initial
paint — the HTML that reaches the browser already has the data in it. The two exceptions that need
a client component (the code-graph canvas, because it re-fetches on node click without a page
navigation) go through the proxy-route pattern in §14.6 instead of calling the control plane
directly from the browser.

## 14.3 Talking to the control plane: two server-only clients

All control-plane calls go through exactly two files, both explicitly server-only (they read
`next/headers`' `cookies()`, which only works in a Server Component, a Route Handler, or a Server
Action):

- **`src/lib/server/api.ts`** — the **read** API: tasks, repositories, review bodies, deployment
  config, feedback analytics. One mutation lives here too (`cancelTask`).
- **`src/lib/server/admin.ts`** — the **admin** API: repository approval, per-repo settings,
  the structural-graph browse/similarity endpoints. Gated by permission claims (§14.4), not just
  authentication.

Both resolve the control plane's base URL the same way:

```ts
// apps/lci/src/lib/server/api.ts:13-15 (admin.ts repeats this verbatim)
function controlPlaneUrl(): string {
  return (process.env.CONTROL_PLANE_URL ?? 'http://localhost:8080/api/v2').replace(/\/+$/, '');
}
```

`/api/v2` is not a guess — it is exactly the prefix `main.rs` nests the whole read/write/admin
router under: `.nest("/api/v2", api_v2_router())` (`services/control-plane/src/main.rs:503`), the
same router whose routes are tabulated in §14.5.

Every call forwards the signed-in user's own access token as a Bearer credential — the console
never holds a service credential of its own:

```ts
// apps/lci/src/lib/server/api.ts:21-29
async function authedFetch(path: string, init?: RequestInit): Promise<Response | null> {
  const token = (await cookies()).get(SESSION_COOKIE)?.value;
  if (!token) return null;
  return fetch(`${controlPlaneUrl()}${path}`, {
    ...init,
    headers: { authorization: `Bearer ${token}`, accept: 'application/json', ...init?.headers },
    cache: 'no-store',
  });
}
```

Two details worth being explicit about, both stated directly in the source's own comments:

1. **`cache: 'no-store'` on every call, deliberately.** Next's Data Cache does not key on the
   `authorization` header, so without this, one user's response could be served back to a
   different user holding a different token — a real cross-user data leak, not a performance nit.
2. **Every function returns `ApiResult<T>`, never throws.** `{ ok: true; data: T }` or
   `{ ok: false; reason: 'unauthenticated' | 'unavailable' | 'error'; status? }` — a missing
   session cookie, a 401/403 from the control plane, a 503, and a network failure are four
   different `reason`s a page can render four different ways (sign-in prompt vs. "try again" vs.
   a generic error), not one undifferentiated catch block.

This is the same trust-boundary shape file [`01`](01-control-plane-roles.md) §1.1 and file
[`10`](10-appstate-and-rust-shared-state.md) describe from the control-plane side, seen from the
other end of the wire: the console holds no database credential, no Neo4j driver, no GitHub App
key — it holds one short-lived OIDC access token per signed-in user, and every byte of actual data
comes back over this one HTTP boundary.

## 14.4 Auth: its own OIDC client, not a shared one

`apps/lci` runs its **own** independent OIDC client — `openid-client` for the Authorization-Code
exchange (Node-only; `src/lib/auth/oidc.ts`) and `jose` for verifying the access token on every
request (Edge-safe; used from the proxy middleware). This is architecturally the same pattern file
[`11`](11-keycloak-and-oidc-auth.md) documents in depth for `apps/web` and for the Rust resource
server — Auth Code+PKCE, JWKS/issuer discovery, a `permissions` claim read by
`hasPermission()`/`permissionsClaim()` (`src/lib/server/admin.ts:17-37`, same ADR-0023 shape file
`11` covers — the claim path is configurable via `PERMISSIONS_CLAIM`, defaulting to `"permissions"`)
— but it is a **separate, freshly-built client**, not `apps/web`'s client relocated. File `11`'s
general OIDC primer (what Auth Code+PKCE is, access vs. ID token, JWKS discovery) applies here
unchanged; this file does not repeat it.

The session itself is one httpOnly cookie (`SESSION_COOKIE`, from `src/lib/auth/session-cookie.ts`)
holding the raw access token. `currentClaims()` (`src/lib/server/session.ts:10-14`) verifies it
fresh on every request via `verifyAccessToken` + `jose` — there is no separate server-side session
store; the cookie *is* the session, exactly as validated JWTs-as-session-state work on the
control-plane's own resource-server side.

## 14.5 Page-by-page: what calls what

Every row below is a real import, grepped directly out of the page component that uses it — not
inferred from the route name.

| Page | Server function called | Real control-plane route |
|---|---|---|
| Overview (`(lci)/page.tsx`) | `listTasks()` | `GET /tasks` |
| Runs (`runs/page.tsx`) | `listTasksPage({ page, pageSize, status?, repositoryId?, q? })` | `GET /tasks?page=&page_size=&status=&repository_id=&q=` |
| One run (`runs/[id]/page.tsx`) | `getTask(id)`, `getReview(id)`, `getDeploymentConfig()`, `hasPermission(...)` | `GET /tasks/{id}`, `GET /tasks/{id}/review`, `GET /config` |
| Repositories (`repositories/page.tsx`) | `listRepositoriesPage({ pageSize, q?, after?, before? })` | `GET /repositories?page_size=&q=&after_activity_at=&after_id=` (or `before_*`) |
| One repository (`repositories/[id]/page.tsx`) | `getAdminRepo(id)` | `GET /admin/repositories` (filtered client-side to one id — no single-repo admin `GET` exists) |
| Repo settings (`repositories/[id]/settings/page.tsx`) | `getRepoSettings(id)` | `GET /admin/repositories/{id}/settings` |
| Repo graph (`repositories/[id]/graph/page.tsx`) | (client component; see §14.6) | `GET /admin/repositories/{id}/graph`, `GET .../symbols/{nodeId}/similar` |
| Repo feedback (`repositories/[id]/feedback/page.tsx`) | `getFeedbackAnalytics({ repositoryId, from, to, bucket })` | `GET /analytics/feedback?repository_id=&from=&to=&bucket=` |
| Admin queue (`admin/page.tsx`) | `listAdminRepos('pending')` | `GET /admin/repositories?status=pending` |
| Estate feedback (`feedback/page.tsx`) | `getFeedbackAnalytics({ from, to, bucket })` | `GET /analytics/feedback?from=&to=&bucket=` |
| Settings (`settings/page.tsx`) | `permissions(claims)` | *(no API call — reads the already-verified JWT claims)* |

Mutations are narrow and explicit, each gated by a permission check before the fetch ever fires
(`mutateRepoApproval` in `admin.ts:91-107` checks `repo:approve`/`repo:deny` itself, independent of
whatever the page's own UI already hid):

- `cancelTask(id)` → `POST /tasks/{id}/cancel`
- `setRepoStatus(id, 'approve' | 'deny')` → `POST /admin/repositories/{id}/{approve|deny}`
- `setRepoSettingsOverride(id, patch)` → `POST /admin/repositories/{id}/settings/override`

Every one of these routes is real, confirmed directly against the router that defines them
(`services/control-plane/src/main.rs:347-407`, `api_v2_router()`) — not just the frontend's own
claim about what it calls.

## 14.6 A deeper example: the structural-graph canvas

The repo-graph page is the one place the console needs genuine client-side interactivity — click a
node, see its neighborhood; ask for "symbols like this one" — which collides with `admin.ts` being
server-only (it reads the httpOnly cookie via `next/headers`, which a Client Component cannot do).
The fix is a **same-origin proxy route**: a tiny Route Handler that runs server-side (so it *can*
read the cookie), calls the same `admin.ts` function a Server Component would, and hands back plain
JSON the browser's own `fetch()` can reach without ever seeing the session token:

```ts
// apps/lci/src/app/api/repositories/[id]/graph/route.ts:12-34 (trimmed)
export async function GET(request: Request, { params }: { params: Promise<{ id: string }> }) {
  const { id: rawId } = await params;
  const id = Number(rawId);
  // ...validate id...
  const node = url.searchParams.get('node') ?? undefined;
  const hops = url.searchParams.get('hops');
  const result = await getRepoGraph(id, { node, hops: hops ? Number(hops) : undefined, ... });
  if (!result.ok) {
    const status = result.status ?? (result.reason === 'unauthenticated' ? 401 : 502);
    return NextResponse.json({ code: result.reason, detail: result.detail }, { status });
  }
  return NextResponse.json(result.data);
}
```

The client-side hook (`use-code-graph.ts`) fetches that same-origin path — `/api/repositories/{id}/graph?node=&hops=`
or `/api/repositories/{id}/symbols/{nodeId}/similar` — never the control plane directly, and
re-fetches whenever the selected node or mode changes. Three hops total for one click: **browser →
Next.js proxy route (re-attaches the cookie) → control plane's `/admin/repositories/{id}/graph` →**
which is the exact endpoint file [`03`](03-neo4j-structural-graph.md) documents as running the real
Cypher `find_symbol`/neighborhood queries against Neo4j, and whose "similar" counterpart
(`/symbols/{nodeId}/similar`) is pgvector cosine-similarity search over that symbol's own stored
embedding, from file [`04`](04-pgvector-and-dual-retrieval.md). This page is the single clearest
place in the whole system where a human can see hybrid search happen one click at a time — browse
the structural graph (Neo4j), or hop sideways into "what else means this" (pgvector), on the same
screen.

One response shape worth knowing because it changes what the canvas renders: a 404 from the
`similar` endpoint carries a `reason` field distinguishing "this repository doesn't exist"
(`not_found`) from "this symbol exists but was never embedded" (`no_embedding`, since embedding
coverage is not 100% — file `04` explains why). The frontend keeps the last successfully-rendered
graph on screen rather than blanking the canvas on either error — a failed "find similar" is an
answer about *that query*, not a reason to discard what was already showing
(`use-code-graph.ts:64-66`'s own comment says exactly this).

## 14.7 Feedback analytics: one query, many panels

The Feedback pages (estate-wide and per-repository) are the console's one real "some queries
briefly explained" case. Both pages call the *same* function, `getFeedbackAnalytics`, differing
only in whether `repositoryId` is set:

```ts
// apps/lci/src/lib/server/api.ts:219-229
/** `GET /analytics/feedback` — the standing 👍/👎 on the comments posted in one window, with the
 *  previous window beside every total (LCI ADR-0118 D2). */
export function getFeedbackAnalytics(params: AnalyticsParams): Promise<ApiResult<FeedbackAnalyticsResponse>> {
  return getAnalytics<FeedbackAnalyticsResponse>('/analytics/feedback', params);
}
```

The query parameters are computed client-of-the-API-side, in `resolveFeedbackWindow()`
(`src/lib/domain/feedback.ts:97-129`), from one of seven human-named ranges (`this-week`, `7d`,
`30d`, ...) — never a raw date picker. Two decisions worth stating because they shape every number
on the page:

- **The bucket size is derived from the span, never chosen independently** (`bucketFor()`,
  `feedback.ts:84-88`): ≤ 7 days of window → hourly buckets, ≤ 90 days → daily, otherwise weekly.
  There's no separate "group by" control a user could set to contradict their own date range.
- **Every figure is something the control plane already aggregated** for exactly that window —
  the frontend's own file-level comment says it plainly: "Nothing here aggregates rows. Every
  figure is read off an aggregate the control plane computed... the adapters only format, order
  and caption." The 👍/👎 counts, the `up / (up + down)` acceptance rate, the per-repository
  breakdown — none of it is recomputed client-side from raw rows; the frontend never sees raw
  rows at all.

One response feeds **nine different panels** (stat cards, a time series, a share chart, a ranked
list, a table) through a closed registry of named adapter functions (`FEEDBACK_ADAPTERS`,
`feedback.ts:439-534`) — each one a pure function from `FeedbackAnalyticsResponse` to a
`DashboardPanelView` the shared `ui-web` panel kit already knows how to render. Nothing in this
system posts a reaction, either: 👍/👎 are left by human reviewers directly on the forge's own PR
comments (GitHub, GitLab, Bitbucket), and the reconciler role polls them back in
(file [`02`](02-postgres-schema-and-queue.md), ADR-0035) — the frontend is read-only here, exactly
like every other page in §14.5.

## 14.8 Data ownership: what lives where

Worth stating plainly, since the cross-repo split (§14.1) could make it look like the console has
its own backend state. It does not:

- **Postgres, Neo4j, pgvector** — all owned by the control plane, exactly as files
  [`02`](02-postgres-schema-and-queue.md)–[`04`](04-pgvector-and-dual-retrieval.md) describe. The
  console never connects to either store directly; it has no connection string for them at all.
- **The session cookie** — the *only* state the console itself holds, and it is not really state
  so much as a cache of one OIDC access token, re-verified against JWKS on every request (§14.4).
- **Everything rendered on every page in §14.5** is fetched fresh per request (`cache: 'no-store'`,
  §14.3) — there is no client-side store, no `apps/lci`-owned database, and (deliberately) no
  stale cached copy of another user's data.

## 14.9 What the backend exposes that the console doesn't use yet

The control plane's own `api_v2_router()` (`main.rs:347-407`) has routes the console has no caller
for at all — not hidden, just unbuilt UI for already-shipped backend capability:

- `GET /tasks/{id}/feedback` — per-task feedback detail (distinct from the aggregate
  `/analytics/feedback` the Feedback pages use).
- `GET`/`POST /admin/repositories/{id}/preset` — review-preset selection.
- `GET /admin/models`, `GET`/`POST /admin/repositories/{id}/model`,
  `GET`/`POST /admin/organizations/{installation_id}/model` — the ADR-0110 per-identity model
  allowlist and override.

`api.ts`'s own file comment says this plainly rather than leaving it to be discovered by a missing
button: "model-override and review-preset selection are not implemented here."

A second, smaller gap is a real type-level discrepancy worth naming rather than silently
"fixing" in a doc: the console's own types — `Repository.platform` (`domain/repos.ts:11`) and
`Task.repo_platform` (`domain/tasks.ts:30`) — are both typed `'github' | 'gitlab'`, with no
`'bitbucket'` variant, even though the control plane's own router has had a live
`/webhook/bitbucket/{installation_id}` route (`main.rs:359-363`) since ADR-0072's platform
abstraction (file [`01`](01-control-plane-roles.md) §1.5, file
[`13`](13-pre-dispatch-cli-utilities.md)'s sibling reconciler discussion). This is exactly the
frontend-vs-backend version of the `github_outbox` naming drift file `02` catches on the backend
side: the backend grew a third platform, and this one TypeScript union hasn't caught up — a
Bitbucket-connected repository will presently reach these two fields with a string value the type
system didn't promise exists.

## 14.10 What to read next

- File [`01`](01-control-plane-roles.md) for the trust-boundary framing this console's
  no-stored-credentials design mirrors.
- File [`02`](02-postgres-schema-and-queue.md) for what `tasks`, `repositories`, and the feedback
  tables actually look like server-side — the exact rows these pages' JSON responses are read from.
- File [`03`](03-neo4j-structural-graph.md) and [`04`](04-pgvector-and-dual-retrieval.md) for the
  real queries behind the graph canvas's two modes (§14.6).
- File [`11`](11-keycloak-and-oidc-auth.md) for the OIDC concepts this console's own client
  (§14.4) implements independently of `apps/web`'s.
- File [`09`](09-observability.md) for how the `review-cost`/`review-delivery`/`feedback`
  Grafana dashboards relate to (and differ from) the in-console Feedback pages this file covers.
