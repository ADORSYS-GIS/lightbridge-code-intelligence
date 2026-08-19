# ADR-0114: Hybrid graph + vector symbol search — Neo4j vector index and a new MCP tool

- **Status:** Accepted
- **Date:** 2026-08-18
- **Deciders:** @leghadjeu-christian
- **Builds on:** [ADR-0089](0089-embeddings-on-the-code-graph.md) (symbol embeddings on `:Symbol`),
  [ADR-0090](0090-hybrid-retrieval-tools.md) (hybrid retrieval tool)
- **Source of truth:** #615, Epic #493

## Context and Problem Statement

ADR-0089 and ADR-0090, written over a year earlier, proposed the missing piece for symbol-level
retrieval — embeddings on `:Symbol` and a hybrid semantic+exact+structural tool — but neither had a
single line of implementation. This ADR turns that proposal into a concrete, shipped design: a Neo4j
vector index on symbols, a Weighted Reciprocal Rank Fusion (WRRF) hybrid query, and a new MCP tool for
the review agent. It is scoped to the **agent-facing path only** — see [Scope](#scope).

Two facts, verified empirically against this cluster rather than assumed from documentation, made the
implementation buildable:

1. **Neo4j Community Edition supports vector indexes.** `CREATE VECTOR INDEX` and
   `db.index.vector.queryNodes` were tested directly against the real `neo4j:5.26-community` instance
   this project runs — real index, real cosine-similarity results, correctly ranked. There is no
   licensing blocker on the database capability itself.
2. **Neo4j documents a real hybrid-search pattern (WRRF)** — fusing ranked results from multiple
   sources by rank, not raw score — directly applicable here once symbols carry embeddings, rather than
   requiring an ad hoc union of two result sets.

### What this closes for the review agent

Before this work, the review agent had two disconnected retrieval tools:
`lightbridge_vector_semantic_search` (pgvector — finds similar *chunks* of text) and
`lightbridge_graph_find_symbol` (Neo4j — exact/substring *name* match on symbols). To go from "a
semantically relevant chunk" to "its place in the call graph," the model had to guess the symbol's
exact name from the chunk text and hope it matched a `find_symbol` query — a lossy, easy-to-fail
bridge, and a wasted turn when it didn't.

`lightbridge_graph_semantic_search` closes that gap: it finds symbols by meaning directly on the graph,
so the result is already a graph node (`node_id`) — immediately usable with `get_callers` or further
traversal, no guessing, no bridge step. Concretely, this helps the review agent:

- **Find related code it has no name for.** A diff introduces retry logic; if an equivalent function
  already exists elsewhere under a different name, exact-match search misses it — semantic search finds
  it. This is the case that matters most for catching **duplication and inconsistent patterns**, which
  requires finding the existing thing first.
- **Spend fewer turns per review.** One hybrid call replaces the chunk-search → guess-a-name →
  hope-`find_symbol`-hits chain.
- **Ground "nothing like this exists" claims in an actual search**, not just a name guess — relevant to
  this project's own refute-pass discipline (don't claim absence without a real search).

## Scope

This ADR covers **only** the agent/MCP-facing half of hybrid search:

- Two new Neo4j indexes on `:Symbol` (vector + fulltext).
- Index-time symbol embedding in `agent-runner`'s indexer.
- A WRRF hybrid-search query in `services/control-plane/src/integrations/neo4j.rs`.
- A new MCP tool, `lightbridge_graph_semantic_search`, additive alongside the existing graph tools.

A frontend-facing search endpoint and UI — letting a human search symbols from `apps/web`, which needs
its own standing embeddings credential on control-plane and a graph-rendering library decision — was
part of the original design exploration but is **explicitly out of scope for this ADR and this PR**.
The project's own prioritization put the agent-facing path first; the frontend half is deferred to a
future decision once this half has shipped and been used, not committed to here.

## Decision Drivers

- **`lightbridge_graph_find_symbol` and `lightbridge_graph_get_callers` do not change.** Whatever ships
  here is additive — a new tool alongside them, not a modification.
- **Reuse Neo4j's actual documented hybrid-search pattern** — Weighted Reciprocal Rank Fusion (WRRF) —
  not an ad hoc union of two result sets.
- **`lci-codegraph` (external crate, `vymalo/lci-codegraph`) stays embeddings-free.** It's a pure
  tree-sitter structural walker today, with no HTTP client and no credentials; that separation is worth
  keeping. Embedding orchestration belongs in `agent-runner`, exactly where chunk embedding already
  lives — `lci-codegraph`'s own crate never touches an embeddings client today, and shouldn't start.
- **No new credential, no new trust surface.** The runner already holds a per-task embeddings
  credential for chunk search; the new tool reuses it. Nothing new is granted to anything.

## Considered Options

### For the embedding-credential question

- **Reuse the runner's existing per-task embeddings credential (chosen).** The runner already embeds
  chunk-search queries client-side (`agent-clients/src/control_plane/search.rs`: *"the caller passes
  the already-embedded query [...] the vector MCP embeds the text with the runner's embeddings key"*).
  A new MCP tool does the same thing — zero new credentials.
- **Have control-plane embed on the agent's behalf too.** Rejected: it would mean two different code
  paths for the same operation (agent-embeds vs control-plane-embeds) depending on caller, for no
  benefit — the runner already has everything it needs.

### For where embeddings get attached to symbols

- **Attach embeddings inline, in the same batch that creates the `:Symbol` nodes (chosen).**
  `agent-runner`'s indexer correlates each `lci-codegraph`-produced symbol against the chunker's
  already-embedded chunks by `(file_path, start_line)`, then sends one combined upsert (structure +
  embedding) to control-plane. Simple: one write, no separate re-runnable step to operate.
- **A separate, re-runnable backfill pass** (`MATCH (s:Symbol) WHERE s.embedding IS NULL ...`, embed,
  patch back) — the pattern a [reviewed external write-up](https://www.linkedin.com/pulse/vector-indexing-plus-knowledge-graphs-neo4j-jeff-tallman-ayxve/)
  uses for its own embedding pipeline. Genuinely more resilient to a partial failure mid-index (nothing
  to re-walk, just re-run the backfill), but not needed for a first version and adds a second moving
  part. Noted here as a credible future hardening step, not adopted now.
  **Important divergence from that write-up, not adopted at all:** it embeds by calling
  `apoc.ml.vertexai.embedding()` **from inside Neo4j itself**, which means Neo4j holds the GCP
  credential and makes the outbound call. That breaks this project's credential boundary — embeddings
  credentials live only on `agent-runner` (or, if the frontend path is ever built, on control-plane),
  never on Neo4j, and APOC's `apoc.ml.*` procedures are pinned to specific vendor backends (OpenAI,
  VertexAI, Bedrock, Azure OpenAI), not this project's own Envoy-proxied embeddings gateway. Rejected
  outright.

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
new, optional property — nodes indexed before this ships simply lack it until re-indexed. Both indexes
are created idempotently at control-plane startup (`neo4j::ensure_indexes`, called once the Neo4j
connection is established in `main.rs`), so a fresh cluster gets them without a manual migration step.

**Who writes it:** `agent-runner`'s indexer (`services/agent-runner/src/indexer/graph.rs`), reusing the
same `EmbeddingsClient` already injected for chunk embedding — not `lci-codegraph`, which stays a pure
structural walker (see Decision Drivers). Symbol text for embedding is correlated in-tree against the
chunker's already-collected chunks by `(file_path, start_line)`, rather than requiring an upstream
change to `lci-codegraph` to expose `end_line`.

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
day one needs. Structural relevance is layered on *after* WRRF fusion instead, not as a third ranked
branch: the returned `node_id`s are graph nodes, so a caller can immediately follow up with the existing
`lightbridge_graph_get_callers` to pull each hit's neighbors. FastRP-based structural ranking is a
legitimate future upgrade if that two-step is too coarse, not something this ADR commits to now.

New function, `services/control-plane/src/integrations/neo4j.rs`:

```rust
/// Hybrid symbol search: lexical (fulltext) + semantic (vector), fused by weighted reciprocal rank
/// (WRRF). Scoped by (repository_id, commit_sha) like every other query in this module. The caller
/// supplies the query embedding — this function never embeds anything itself.
pub async fn hybrid_symbol_search(
    graph: &Graph,
    repository_id: i64,
    commit_sha: &str,
    query_text: &str,
    query_embedding: &[f32],
    source_k: i64,
    final_k: i64,
) -> anyhow::Result<Vec<SymbolHit>> {
    // WRRF Cypher above, parameterized by $repo/$commit/query/queryVector/sourceK/finalK.
    // rrfConstant fixed at Neo4j's documented default, 60.0.
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

## Consequences

- **Good:** the new capability is provably additive — two new indexes, one new query function, one new
  MCP tool. Nothing existing changes behavior.
- **Good:** zero new trust surface — the agent path reuses the runner's existing per-task embeddings
  credential; no new credential is granted to anything.
- **Good:** WRRF is the actual documented Neo4j pattern, not a bespoke fusion scheme this project would
  have to justify and maintain alone.
- **Good:** closes a real, concrete gap (see "What this closes for the review agent" above) rather than
  adding a tool speculatively — the review agent previously had no reliable path from "semantically
  similar" to "a graph node it can traverse."
- **Bad:** real new engineering — two indexes, a fused-ranking query, a new internal endpoint, a new MCP
  tool, and index-time embedding added to the runner's indexer. ADR-0089's own accepted cost line still
  applies: roughly double the embedding-API calls at index time (symbols ≈ chunks in count).
- **Bad:** the structural signal is a deliberate simplification (hop-distance follow-up, not a true
  FastRP-ranked third source) — a real, disclosed scope reduction from Neo4j's full documented pattern,
  not the whole thing.
- **Neutral:** the frontend-facing half (search endpoint, standing control-plane embeddings credential,
  graph-rendering library) is deferred — see [Scope](#scope) — and will need its own decision, informed
  by how this half performs in practice, before it's built.

## More Information

- [ADR-0089](0089-embeddings-on-the-code-graph.md) — the original symbol-embeddings proposal this ADR
  makes concrete.
- [ADR-0090](0090-hybrid-retrieval-tools.md) — the original hybrid-tool proposal; `lightbridge_graph_semantic_search`'s
  name is inherited from here.
- [ADR-0062](0062-two-tier-review-fast-auto-deep-on-demand.md) — the per-tier tool allowlist the new
  MCP tool registers into, additively.
- Neo4j hybrid search (WRRF pattern, fetched directly): `neo4j.com/developer/genai-ecosystem/hybrid-search/`
- External write-up reviewed for the embedding-insertion question (Considered Options above), agreed
  with on strategy, diverged from on credential placement: `linkedin.com/pulse/vector-indexing-plus-knowledge-graphs-neo4j-jeff-tallman-ayxve`
- `services/agent-clients/src/control_plane/search.rs`, `services/agent-runner/src/indexer/graph.rs`,
  `services/control-plane/src/integrations/neo4j.rs`, `services/control-plane/src/mcp/tools.rs`
