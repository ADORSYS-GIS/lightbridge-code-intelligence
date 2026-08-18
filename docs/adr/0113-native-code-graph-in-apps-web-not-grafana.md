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

- **New endpoint:** `GET /admin/repositories/{id}/graph?seed=<node_id>&hops=2`, registered next to
  the existing `/admin/repositories/{id}/{approve,settings,preset}` routes
  (`services/control-plane/src/main.rs`) and gated by the same `repo:read` scope already used for
  `list_repositories` / `get_settings` (`services/control-plane/src/http/admin.rs`). Neo4j
  credentials stay control-plane-side; the browser only ever calls this endpoint. `seed` is optional —
  omitted, the endpoint picks a deterministic arbitrary symbol (`any_symbol_id` in `neo4j.rs`) so the
  view always has something to show before a symbol picker exists. Which symbol should seed the
  initial view (top-degree? most recently changed?) is a product decision left open by this ADR.
- **Scoping:** the endpoint resolves the repo's latest indexed commit and queries Neo4j with the
  existing `repo_id` + `commit` predicate — the same scoping the agent's graph tools already use, so
  no new Cypher shape is introduced.
- **Reduction for large repos — the neighborhood strategy, concretely.** The endpoint never returns
  the full graph. The first (and, as shipped, only) reduction strategy is an N-hop neighborhood around
  a seed symbol, matching how `graph_get_callers` already thinks about traversal:
  ```cypher
  MATCH (seed:Symbol {repo_id: $repo, commit: $commit, node_id: $id})
  MATCH path = (seed)-[:REL*1..2]-(neighbor:Symbol {repo_id: $repo, commit: $commit})
  UNWIND relationships(path) AS r
  WITH DISTINCT startNode(r) AS a, endNode(r) AS b, r.relation AS relation
  RETURN a.node_id AS src_id, a.label AS src_label, a.source_file AS src_file, a.start_line AS src_line,
         b.node_id AS dst_id, b.label AS dst_label, b.source_file AS dst_file, b.start_line AS dst_line,
         relation
  LIMIT $limit
  ```
  The `2` in `*1..2` is the request's `hops` value (clamped server-side to `1..=3`) formatted directly
  into the query text, not bound as a `$` parameter — Cypher's variable-length path syntax takes a
  literal bound, not a parameter, in stock Neo4j. Safe here because `hops` is clamped before it ever
  reaches the query string. `$limit` is a fixed server-side ceiling (`GRAPH_EDGE_LIMIT = 500`) on the
  edge count, independent of what the caller asked for. A **per-file slice** and a **top-degree
  overview** are named as candidate future strategies, selectable the same way (a query parameter),
  but neither is built — the neighborhood strategy alone is what #615 shipped.
- **Response shape** (`GraphResponse` in `admin.rs`; `SymbolHit`/`RelHit` in `neo4j.rs`):
  ```json
  {
    "commit": "a1b2c3d...",
    "nodes": [
      { "node_id": "string", "label": "string", "source_file": "string", "start_line": 0 }
    ],
    "edges": [
      { "source": "string", "target": "string", "relation": "calls | contains | method" }
    ]
  }
  ```
  `label` is `Symbol.label` (the actual function/type name), resolved server-side and returned as
  part of the payload — not left to the frontend to derive from a generic type, and not the Neo4j
  graph *label* (`Symbol`) a naive Grafana-style embed would have shown instead (the #515 finding this
  ADR exists to fix).
- **Rendering library:** the frontend graph-drawing library (e.g. a force-directed layout component)
  is a follow-up decision; #615 ships a plain list against this same response shape so the data
  contract and reduction behavior could be verified before picking one.

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
