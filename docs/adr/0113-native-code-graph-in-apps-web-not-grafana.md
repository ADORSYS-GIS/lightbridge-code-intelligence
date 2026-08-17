# ADR-0113: Render the code graph natively in apps/web

- **Status:** Accepted
- **Date:** 2026-08-17
- **Deciders:** @leghadjeu-christian
- **Source of truth:** #615, Epic #493 (background: #515)

## Context and Problem Statement
Lightbridge already builds a structural code graph — `Symbol` nodes and `REL` relationships, scoped
by `repo_id` + `commit` — in Neo4j during indexing (`services/control-plane/src/integrations/neo4j.rs`).
The agent already reads this graph server-side via its `graph_find_symbol` / `graph_get_callers`
tools, but a repo owner has no way to see it: the repo detail page
(`apps/web/app/dashboard/repositories/[id]/page.tsx`) exposes commits, tasks, and cost/token
analytics (`apps/web/components/repos/repo-analytics-embed.tsx`), nothing structural. This ADR
answers: **how does the repo detail page get a graph view, and who owns rendering it?**

## Decision

**Add a graph view as a first-party `apps/web` component**, sitting alongside
`repo-analytics-embed.tsx` on the repo detail page, backed by a new admin endpoint rather than a
client-side Neo4j connection.

- **New endpoint:** `GET /admin/repositories/{id}/graph`, registered next to the existing
  `/admin/repositories/{id}/{approve,settings,preset}` routes (`services/control-plane/src/main.rs`)
  and gated by the same `repo:read` scope already used for `list_repositories` / `get_settings`
  (`services/control-plane/src/http/admin.rs`). Neo4j credentials stay control-plane-side; the
  browser only ever calls this endpoint.
- **Scoping:** the endpoint resolves the repo's latest indexed commit and queries Neo4j with the
  existing `repo_id` + `commit` predicate — the same scoping the agent's graph tools already use, so
  no new Cypher shape is introduced.
- **Node labels** come from `Symbol.label` (the actual function/type name), resolved server-side and
  returned as part of the response payload — not left to the frontend to derive from a generic type.
- **Reduction for large repos:** the endpoint does not return the full graph unconditionally. It
  serves a bounded subset — a neighborhood around a chosen symbol (N-hop traversal, matching how
  `graph_get_callers` already thinks), a per-file slice, or a top-degree overview — selected by a
  query parameter. Which strategy(ies) ship first is decided during #615's implementation, not by
  this ADR.
- **Rendering library:** the frontend graph-drawing library (e.g. a force-directed layout component)
  is chosen during #615; this ADR fixes the data path and ownership boundary, not the exact visual.

## Consequences

- **Good:** the trust boundary is unchanged — Neo4j credentials never reach the browser, mirroring
  how the agent's own graph tools are mediated; the new endpoint is one more read route on the
  existing `repo:read`-gated admin surface, not a new credential class.
- **Good:** the view lives on the same admin surface as the rest of the repo detail page
  ([ADR-0112](0112-invest-in-apps-web-supersede-0063.md)), reusing its existing auth, layout, and
  data-fetching conventions instead of introducing a second rendering destination.
- **Good:** node labeling and the reduction strategy are both implemented server-side, where the
  Neo4j schema and its indexing conventions already live — no duplicate logic in the frontend.
- **Bad:** this is new surface area, not a reuse of an existing panel — a new endpoint, a new
  response shape, and a new frontend component all have to be built and maintained in #615.
- **Neutral:** the endpoint returns a bounded subset by default; a "give me everything" mode, if ever
  needed, is a deliberate future extension, not the default behavior.

## More Information

- [ADR-0112](0112-invest-in-apps-web-supersede-0063.md) — commits to `apps/web` as the permanent rich
  admin surface; this decision extends that investment.
- #615 — the implementation story this ADR unblocks.
