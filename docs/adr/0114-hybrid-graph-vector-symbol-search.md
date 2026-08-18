# ADR-0114: Hybrid graph + vector symbol search — a new MCP tool and a new frontend search endpoint

- **Status:** Accepted
- **Date:** 2026-08-18
- **Deciders:** @leghadjeu-christian
- **Builds on:** [ADR-0089](0089-embeddings-on-the-code-graph.md) (symbol embeddings on `:Symbol`),
  [ADR-0090](0090-hybrid-retrieval-tools.md) (hybrid retrieval tool), [ADR-0113](0113-native-code-graph-in-apps-web-not-grafana.md)
  (native graph view in `apps/web`)
- **Source of truth:** #615, Epic #493

## Context and Problem Statement

ADR-0113 shipped a structural-only neighborhood browse: pick a symbol, see what's within N hops. It
answers "show me around here," not "find me something." ADR-0089 and ADR-0090, written over a year
earlier, proposed exactly the missing piece — symbol embeddings and a hybrid semantic+exact+structural
tool — but neither has a single line of implementation, and neither addresses a human using this from
`apps/web` at all; both are agent/MCP-only designs.

Two facts changed since those ADRs were written, both verified empirically against this cluster rather
than assumed from documentation:

1. **Neo4j Community Edition supports vector indexes.** `CREATE VECTOR INDEX` and
   `db.index.vector.queryNodes` were tested directly against the real `neo4j:5.26-community` instance
   this project runs — real index, real cosine-similarity results, correctly ranked. There is no
   licensing blocker on the database capability itself.
2. **Neo4j's own visualization library (NVL) is licensed for Aura or Enterprise-Subscription products
   only** — confirmed from its license page directly, corroborated by the Subscription Agreement's
   explicit prohibition on mixing GPL-licensed Neo4j (Community Edition) with Subscription-governed
   software in the same system, and by npm's own `"license": "SEE LICENSE IN 'LICENSE.txt'"` flag on
   the package. This blocks NVL specifically — not vector search, not graph rendering in general.

This ADR turns ADR-0089/0090 into a concrete, buildable design; adds the frontend half neither one
covers; and answers the question those corrected facts raise: **how do we add real search — semantic,
lexical, and structural — without duplicating embedding logic, without ever putting an embeddings
credential in the browser, without touching the two existing structural MCP tools, and with a
rendering library we're actually allowed to use?**

## Decision Drivers

- **`lightbridge_graph_find_symbol` and `lightbridge_graph_get_callers` do not change.** Whatever ships
  here is additive — a new tool alongside them, not a modification.
- **The browser must never hold an embeddings credential.** Every existing credential in this system
  (Neo4j, Postgres, GitHub App) lives control-plane-side or runner-side; this can't be the exception.
- **Reuse Neo4j's actual documented hybrid-search pattern** — Weighted Reciprocal Rank Fusion (WRRF) —
  not an ad hoc union of two result sets.
- **`lci-codegraph` (external crate, `vymalo/lci-codegraph`) stays embeddings-free.** It's a pure
  tree-sitter structural walker today, with no HTTP client and no credentials; that separation is worth
  keeping. Embedding orchestration belongs in `agent-runner`, exactly where chunk embedding already
  lives — `lci-codegraph`'s own crate never touches an embeddings client today, and shouldn't start.

## Considered Options

### For the agent-facing credential question

- **Reuse the runner's existing per-task embeddings credential (chosen).** The runner already embeds
  chunk-search queries client-side (`agent-clients/src/control_plane/search.rs`: *"the caller passes
  the already-embedded query [...] the vector MCP embeds the text with the runner's embeddings key"*).
  A new MCP tool does the same thing — zero new credentials.
- **Have control-plane embed on the agent's behalf too.** Rejected: it would mean two different code
  paths for the same operation (agent-embeds vs control-plane-embeds) depending on caller, for no
  benefit — the runner already has everything it needs.

### For the frontend-facing credential question

- **Give control-plane its own standing embeddings credential (chosen).** The only role with an HTTP
  surface a browser can reach. Same secret shape (`embeddings-base-url`/`model`/`api-key`) already used
  by the runner Job, mounted onto a *different, longer-lived* Deployment.
- **Spin up a Job per search request.** Rejected: not viable for an interactive UI — a real index Job
  in this project took 15+ seconds end-to-end for a small repo, including pod scheduling. A search box
  needs a synchronous embed on the request path.
- **Have the browser call the embeddings gateway directly.** Rejected outright — this is precisely the
  credential-in-the-browser outcome this ADR exists to avoid.

### For the rendering library

- **A generic, license-clean library (`@xyflow/react`, chosen) fed by our own JSON.** MIT-licensed,
  free for commercial use, and its default look/interaction model (pan/zoom/click, minimap, controls)
  is exactly what a bounded node/edge diagram needs. Consumes the same `{nodes, edges}` shape
  ADR-0113's endpoint already returns — no backend change for the swap.
- **Neo4j NVL.** Rejected — licensed for Aura/Enterprise-Subscription only (see Context). Its own data
  model (`{id, caption, ...}` nodes, `{id, from, to, caption}` relationships) would have mapped onto
  our schema almost directly if licensing allowed it; noted here so the fit isn't re-litigated later
  for the wrong reason.
- **Sigma.js + graphology.** Considered — its strength is WebGL rendering of large, effectively
  unbounded graphs. Not chosen because nothing we render is unbounded: `GRAPH_EDGE_LIMIT` and `hops`
  already cap every response server-side (ADR-0113), so raw large-N performance isn't the constraint;
  interaction polish is.

## Decision Outcome

### 1. Two new Neo4j indexes, no change to the existing schema

```cypher
-- Semantic signal. Verified working on this cluster's real neo4j:5.26-community.
CREATE VECTOR INDEX symbol_embedding_idx IF NOT EXISTS
FOR (s:Symbol) ON s.embedding
OPTIONS { indexConfig: {
  `vector.dimensions`: 4096,
  `vector.similarity_function`: 'cosine'
}};

-- Lexical signal — a real ranked/scored source for WRRF, unlike find_symbol's
-- boolean CONTAINS predicate, which has no rank to fuse.
CREATE FULLTEXT INDEX symbol_label_fulltext IF NOT EXISTS
FOR (s:Symbol) ON EACH [s.label, s.source_file];
```

`repo_id`, `commit`, `node_id`, `label`, `source_file`, `start_line` are untouched. `embedding` is a
new, optional property — nodes indexed before this ships simply lack it until re-indexed.

**Who writes it:** `agent-runner`'s indexer (`services/agent-runner/src/indexer/graph.rs`), reusing the
same `EmbeddingsClient` already injected for chunk embedding — not `lci-codegraph`, which stays a pure
structural walker (see Decision Drivers). One real open item: `lci-codegraph`'s `GraphNode` currently
exposes only `start_line`, not `end_line`, so slicing a symbol's full definition text needs either a
small additive PR to that external crate, or an in-tree correlation with `chunker.rs`'s
already-end-line-bearing chunks by `(file_path, start_line)` — a genuine choice, not settled by this
ADR, and worth a decision before implementation starts.

### 2. Hybrid search via Weighted Reciprocal Rank Fusion (WRRF)

Neo4j's documented hybrid-search pattern scores each source by *rank*, not raw score, and sums
contributions for a result that appears in multiple sources:

```cypher
CALL (query, queryVector) {
  CALL db.index.fulltext.queryNodes('symbol_label_fulltext', query, {limit: $sourceK})
  YIELD node AS s, score
  WITH collect(s) AS hits
  UNWIND CASE WHEN size(hits) = 0 THEN [] ELSE range(0, size(hits) - 1) END AS rankIndex
  RETURN hits[rankIndex] AS s, 'lexical' AS source, rankIndex + 1 AS sourceRank

  UNION ALL

  CALL db.index.vector.queryNodes('symbol_embedding_idx', $sourceK, queryVector)
  YIELD node AS s, score
  WITH collect(s) AS hits
  UNWIND CASE WHEN size(hits) = 0 THEN [] ELSE range(0, size(hits) - 1) END AS rankIndex
  RETURN hits[rankIndex] AS s, 'semantic' AS source, rankIndex + 1 AS sourceRank
}
WITH s, source, sourceRank, coalesce($sourceWeights[source], 1.0) AS weight
WITH s, sum(weight / ($rrfConstant + sourceRank)) AS wrrf
ORDER BY wrrf DESC
LIMIT $finalK
RETURN s.node_id AS node_id, s.label AS label, s.source_file AS source_file,
       s.start_line AS start_line, wrrf;
```

**Version note, confirmed against this deployment:** Neo4j's docs show two syntaxes — a newer
`SEARCH ... IN (VECTOR INDEX ...) SCORE AS score` clause, and the `db.index.vector.queryNodes()`
procedure form. This project's own live test against `neo4j:5.26-community` used the **procedure
form** successfully; that's what the query above uses, not the newer clause syntax.

**Structural signal, deliberately simplified:** the documented pattern's third source is a GDS FastRP
node-embedding index — real, but it needs the separate Graph Data Science plugin and is heavier than
day one needs. Instead, structural relevance is layered on *after* WRRF fusion, not as a third ranked
branch: for the top fused hits, run the existing `graph_neighborhood` (ADR-0113) to attach each hit's
immediate neighbors. FastRP-based structural ranking is a legitimate future upgrade if hop-distance
proves too coarse, not something this ADR commits to now.

New function, `services/control-plane/src/integrations/neo4j.rs`:

```rust
/// Hybrid symbol search: lexical (fulltext) + semantic (vector), fused by weighted reciprocal rank
/// (WRRF). Scoped by (repository_id, commit_sha) like every other query in this module. The caller
/// supplies the query embedding — this function never embeds anything itself, which is what lets both
/// the MCP tool (runner-embedded) and the admin endpoint (control-plane-embedded) share it.
pub async fn hybrid_symbol_search(
    graph: &Graph,
    repository_id: i64,
    commit_sha: &str,
    query_text: &str,
    query_embedding: &[f32],
    source_k: i64,
    final_k: i64,
    rrf_constant: f64,
) -> anyhow::Result<Vec<(SymbolHit, f64)>> {
    // WRRF Cypher above, parameterized by $repo/$commit/query/queryVector/sourceK/finalK/rrfConstant.
}
```

### 3. New MCP tool — agent path, existing tools untouched

`lightbridge_graph_semantic_search` (the name ADR-0090 originally proposed), registered as a new,
additive entry in the per-tier tool allowlist ([ADR-0062](0062-two-tier-review-fast-auto-deep-on-demand.md)),
next to — not instead of — `lightbridge_graph_find_symbol` and `lightbridge_graph_get_callers`, which
keep their exact current code and registration. The runner embeds the query with its existing per-task
credential and calls a new `POST /internal/tasks/{id}/graph/hybrid_search`, which wraps
`hybrid_symbol_search`. `mcp/tools.rs`'s existing `graph_search` function (today: `find_symbol` /
`get_callers` only) is left alone — the new tool is new code, not a third branch bolted onto that
dispatcher's exhaustive match.

### 4. New admin endpoint — frontend path

```
GET /admin/repositories/{id}/search?q=<text>
```

`repo:read`-gated (the `Caller` extractor, same as every other admin route), resolves
`latest_indexed_commit` the same way ADR-0113's endpoint does. The one new step: **control-plane embeds
`q` itself**, using a new standing `EMBEDDINGS_*` credential mounted onto its own Deployment (see
Considered Options) — then calls `hybrid_symbol_search` with the result. Response shape is deliberately
identical to ADR-0113's `GraphResponse` (`{ commit, nodes, edges }`), so the same frontend component
renders either a neighborhood browse or a search result, and a search hit can seed a follow-up
neighborhood fetch — closing the "which symbol seeds the initial view" question ADR-0113 left open.

### 5. Frontend rendering

`@xyflow/react` replaces the placeholder list rendering, consuming the same JSON either endpoint
returns. The browser calls only `apps/web` → control-plane's own admin API, exactly like every other
page in this app — it never sees an embeddings credential, a Neo4j credential, or any third-party API.

## Consequences

- **Good:** the new capability is provably additive — two new indexes, one new query function, one new
  MCP tool, one new endpoint, one new frontend component. Nothing existing changes behavior.
- **Good:** the credential split in §1/Considered Options means the agent path ships with zero new
  trust surface; only the frontend path needs a new grant, and it's scoped to exactly one new
  Deployment holding a credential of a kind this system already manages elsewhere.
- **Good:** WRRF is the actual documented Neo4j pattern, not a bespoke fusion scheme this project would
  have to justify and maintain alone.
- **Bad:** real new engineering, not a small addition — two indexes, a fused-ranking query, a new
  internal endpoint, a new admin endpoint, a new MCP tool, and index-time embedding added to the
  runner's indexer. ADR-0089's own accepted cost line still applies: roughly double the embedding-API
  calls at index time (symbols ≈ chunks in count).
- **Bad:** the structural signal is a deliberate simplification (hop-distance layered after fusion, not
  a true FastRP-ranked third source) — a real, disclosed scope reduction from Neo4j's full documented
  pattern, not the whole thing.
- **Neutral:** the `lci-codegraph` `end_line` question (§1) is explicitly left open by this ADR and
  needs a decision — either a small upstream PR or an in-tree correlation — before index-time symbol
  embedding can be implemented.

## Pros and Cons of the Options

### `@xyflow/react` (chosen)

- Good: MIT, free for commercial use, best-in-class default polish for a bounded node/edge diagram,
  consumes the existing JSON shape unchanged.
- Bad: none material for this use case.

### Neo4j NVL

- Good: purpose-built for exactly this (Neo4j Bloom-style graph visualization); data model would have
  mapped onto `SymbolHit`/`RelHit` almost directly.
- Bad: **license-restricted to Aura or Enterprise Subscription** — confirmed from the license page
  directly, the Subscription Agreement's anti-mixing clause, and npm's own non-standard-license flag.
  Not usable against this deployment's Community Edition. Hard reject, not a preference.

### Control-plane holding a standing embeddings credential (chosen, frontend path)

- Good: the only way to embed a live query synchronously without a credential ever reaching the
  browser or a Job spinning up per keystroke.
- Bad: genuinely new standing access to a paid external API from the process the public web UI talks
  to — real new trust surface, not free. Worth its own short follow-up ADR at implementation time,
  focused just on that grant.

## More Information

- [ADR-0089](0089-embeddings-on-the-code-graph.md) — the original symbol-embeddings proposal this ADR
  makes concrete.
- [ADR-0090](0090-hybrid-retrieval-tools.md) — the original hybrid-tool proposal; `lightbridge_graph_semantic_search`'s
  name is inherited from here.
- [ADR-0113](0113-native-code-graph-in-apps-web-not-grafana.md) — the native graph view this extends;
  its `GraphResponse` shape, `repo:read` gating, and `latest_indexed_commit` resolution are all reused
  unchanged.
- [ADR-0062](0062-two-tier-review-fast-auto-deep-on-demand.md) — the per-tier tool allowlist the new
  MCP tool registers into, additively.
- Neo4j hybrid search (WRRF pattern, fetched directly): `neo4j.com/developer/genai-ecosystem/hybrid-search/`
- Neo4j Visualization Library license (fetched directly): `neo4j.com/docs/reference/license/nvl`
- `services/agent-clients/src/control_plane/search.rs`, `services/agent-runner/src/indexer/graph.rs`,
  `services/control-plane/src/integrations/k8s.rs`, `services/control-plane/src/mcp/tools.rs`
