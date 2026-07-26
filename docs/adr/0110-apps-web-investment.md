# ADR-0110: apps/web investment — session refresh, cursor pagination, per-repo pages, Grafana-embedded graphs

- **Status:** Accepted
- **Date:** 2026-07-26
- **Deciders:** @stephane-segning
- **Supersedes:** [ADR-0063](0063-cli-only-repository-approval.md), [ADR-0064](0064-observability-via-grafana-behind-caddy-oauth2.md), the web-retirement direction of Epic #241
- **Extends:** [ADR-0102](0102-grafana-loki-embedded-run-logs.md)

## Context and Problem Statement

Epic #241 and its underlying ADR-0063 (CLI-only repository approval) / ADR-0064 (Grafana behind
Caddy, retiring the web dashboards) pointed apps/web toward eventual retirement — down to just the
`lci` CLI plus standalone Grafana. That direction is reversed: apps/web is invested in instead.
Three concrete gaps make the current state unfit for that investment: (1) auth never refreshes —
`apps/web`/`packages/auth` reads only `tokens.access_token` from the OIDC callback and never stores
or uses `refresh_token` (confirmed zero references anywhere in the repo); a session simply expires
and the user gets bounced through a full IdP redirect; (2) list views have no real pagination —
`TASK_LIST_LIMIT: i64 = 100` is a hard cap with no cursor
(`services/control-plane/src/queue/tasks.rs:13`), and `/repositories` has no limit at all; (3) there
is no per-repo detail page — `apps/web/app/dashboard/repositories/` is a flat list, unlike
`runs/[id]/page.tsx`, which already has the detail-page + Grafana-embed pattern
([ADR-0102](0102-grafana-loki-embedded-run-logs.md)) this ADR reuses for repos.

## Decision Drivers

- Reuse the proven `d-solo` Grafana iframe embed pattern from
  [ADR-0102](0102-grafana-loki-embedded-run-logs.md) rather than inventing a second embed
  mechanism.
- Reuse the `runs/[id]` detail-page pattern for the new `repositories/[id]` page rather than a
  divergent structure.
- Token refresh must not weaken the existing Keycloak OIDC posture
  ([ADR-0014](0014-keycloak-oidc-resource-server.md)) — it adds silent renewal via the refresh
  token the IdP already issues, it does not change the auth model.
- Pagination must be cursor-based (keyset), not offset-based, consistent with the one existing
  pagination precedent in this codebase ([ADR-0081](0081-a2a-input-required-and-list-tasks.md)).

## Considered Options

- **A — Keep pursuing #241's retirement direction; only patch the session-refresh bug.** Rejected:
  contradicts the user's explicit ask for per-repo detail pages and richer apps/web analytics: an
  investment decision, not a bugfix-only one.
- **B — Reverse #241's direction: apps/web gets a real session-refresh flow, cursor pagination, a
  per-repo detail page with an approve/deny action and embedded Grafana panels for both the repo's
  Neo4j code graph and its run analytics.** Chosen.

## Decision Outcome

Chosen option: **B**.

- **Session refresh:** `apps/web/app/api/auth/callback/route.ts` persists `tokens.refresh_token`
  (httpOnly, same cookie posture as the access token); `middleware.ts` attempts a silent
  refresh-grant exchange before falling back to the full Authorization-Code redirect it does today.
- **Pagination:** `services/control-plane` replaces `TASK_LIST_LIMIT`/the unbounded repositories
  query with keyset pagination (`created_at, id` cursor, matching the existing
  [ADR-0081](0081-a2a-input-required-and-list-tasks.md) shape), exposed via `?cursor=`/`?limit=` query
  params; `apps/web`'s list views gain cursor-based "load more"/pagination controls.
- **Per-repo detail page:** `apps/web/app/dashboard/repositories/[id]/page.tsx`, following the
  `runs/[id]` pattern, with approve/deny actions (reusing the existing admin approve/deny API, not
  reintroducing anything ADR-0063 removed — CLI approval stays available too, this is an additional
  surface, not a replacement).
- **Grafana embeds:** two new `d-solo` panels on the repo detail page, same mechanism as
  `run-logs-embed.tsx` — (1) a Neo4j code-graph panel (spike: Grafana's Neo4j datasource + node-graph
  panel type, Cypher query scoped to the repo), (2) per-repo analytics reusing the existing
  `deploy/observability/dashboards/{review-cost,review-quality,review-runs}.json` dashboards scoped
  by a repo variable. The open LogQL bug (#483) on the existing embed is fixed as a prerequisite,
  since the new embeds sit on the same page family and shouldn't inherit a known-broken panel.

### Consequences

- Good, because a user's session survives longer than 30 minutes without a full IdP redirect.
- Good, because list views scale past 100 items without a UI/API rewrite later.
- Good, because the Neo4j-graph-in-Grafana idea reuses a pattern already proven in prod
  ([ADR-0102](0102-grafana-loki-embedded-run-logs.md)) instead of building a bespoke graph
  renderer in apps/web.
- Bad, because the Neo4j-in-Grafana panel is genuinely new integration surface (no existing ADR
  covers Grafana+Neo4j) — scoped as a spike story, not assumed to be a known quantity.
- Neutral, because `lci` CLI approval (ADR-0063) is not removed — the web detail page adds a second
  surface for the same action, which is a deliberate, explicit reversal of "CLI-only," not an
  oversight.

## More Information

Per-identity (org/user/repo) model selection + ACL, carried forward from Epic #241, lands under
[ADR-0103](0103-repo-configurable-opencode-review-presets.md)'s epic instead — it's model-selection
scope, not a UI-surface concern, and doesn't need to block or be blocked by this ADR.
