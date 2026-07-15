# ADR-0089: Embeddings on the Neo4j code graph — graph-native vector search

- **Status:** Proposed
- **Date:** 2026-07-13
- **Deciders:** @stephane-segning
- **Amends:** [ADR-0003](0003-dual-retrieval-neo4j-pgvector.md) (does NOT supersede — pgvector stays)
- **Builds on:** [ADR-0086](0086-in-house-code-graph-crate.md) (`lci-codegraph` owns the Neo4j write), [ADR-0018](0018-openai-compatible-embeddings.md) (eaig qwen3-embedding-8b, 4096-dim)

## Context and Problem Statement

[ADR-0003](0003-dual-retrieval-neo4j-pgvector.md) split retrieval into two **complementary** stores:
Neo4j answers *structure* (`:Symbol` nodes + `contains`/`method`/`calls` edges), pgvector answers
*semantics* (chunk embeddings in `code_chunks`). The deep-tier agent joins them itself — it runs the
`search` tool (pgvector), reads the hits, then issues `graph_find_symbol` / `graph_get_callers`
(Neo4j) to pull the structure around them.

Two things changed since ADR-0003:

1. **We now own the Neo4j write end-to-end.** [ADR-0086](0086-in-house-code-graph-crate.md)'s
   `lci-codegraph` produces `:Symbol` nodes with stable `(repo_id, commit, node_id)` identity and
   knows each symbol's source span, and the control plane already writes them
   ([`integrations/neo4j.rs`](../../services/control-plane/src/integrations/neo4j.rs)).
2. **The graph store can now hold vectors.** The deployed Neo4j is **5.26-community**, which has
   native HNSW vector indexes (in Community), whose **max dimension is 4096 — exactly the
   qwen3-embedding-8b width** we already produce.

So the semantic and the structural halves no longer *have* to live in different stores. If a
`:Symbol` carries an embedding, a single query can retrieve **semantically-similar symbols and their
call neighborhood at once** — retrieval that is semantic *and* structural, instead of two lookups the
model stitches together. The question is not "replace pgvector" (its exact, chunk-granular recall is
quality-critical and complementary) — it is: **should the graph store also carry embeddings, at what
granularity, and how does that coexist with pgvector?**

This is deliberately **additive**. pgvector's chunk index stays exactly as it is; this adds a new,
symbol-granular semantic lane on top of the graph. It amends ADR-0003's "two complementary stores"
into "Neo4j = structure + symbol-level semantics; pgvector = chunk-level semantics; both live."

## Decision Drivers

- **Graph-aware semantic retrieval** — the payoff: vector-search a symbol, expand its call graph, in
  one query. Strictly more than the current stitch-in-the-agent.
- **Symbol-granular semantic recall** (function/method level) *complements* pgvector's chunk-window
  recall — a different, useful lane, not a duplicate.
- **Low incremental cost** — we already own the Neo4j write ([ADR-0086](0086-in-house-code-graph-crate.md))
  and run the eaig embedder in the indexer; the delta is a node property + a vector index.
- **Do not trade proven recall/determinism.** pgvector runs *exact* search today; Neo4j's vector
  index is HNSW (*approximate*). The exact path must stay untouched.

## Considered Options

- **A — Symbol-level embeddings on `:Symbol` (chosen).** `lci-codegraph` embeds each symbol's def
  span through the same eaig qwen3-embedding-8b path it already runs, and the control plane stores it
  as `:Symbol.embedding` (4096-dim) behind a Neo4j cosine vector index. New capability: vector search
  over symbols, composable with graph traversal. Cost: +N embedding calls per index (N ≈ symbol
  count).
- **B — Reuse chunk embeddings, map to symbols.** No new embedding calls — attribute each `code_chunks`
  vector to the overlapping symbol by file+range. Cheaper, but chunk↔symbol is many-to-many (a chunk
  can contain several defs; a large def spans several chunks), so averaging/attribution is fuzzy and
  loses fidelity. Rejected as the default; can be a cost-saving fallback if index-time embedding cost
  bites.
- **C — `:Chunk` nodes with embeddings in Neo4j.** Mirror the chunk vectors onto new `:Chunk` nodes
  linked to `:Symbol` by range, giving Neo4j a full chunk-level semantic index. Enables chunk-level
  hybrid queries but **duplicates pgvector's data in a second, approximate store** — two semantic
  indexes to keep in sync for a granularity pgvector already serves *exactly*. Rejected.
- **D — Move all semantic search into Neo4j, drop pgvector.** One store. Rejected: trades pgvector's
  exact search for Neo4j HNSW (approximate → recall variance on the review-quality-critical path,
  worsening the [#285](https://github.com/vymalo/lightbridge-code-intelligence/issues/285)-class
  re-review non-determinism), and Neo4j-community is single-instance. The directive is to **add**
  hybrid tools and **keep** the existing ones, not consolidate.

## Decision Outcome

Chosen option: **A** — symbol-level embeddings on `:Symbol`, plus a Neo4j cosine vector index,
**additive** to the existing pgvector chunk index.

- `lci-codegraph` emits, per `:Symbol`, an `embedding: List<Float>` (4096-dim) produced by the **same**
  qwen3-embedding-8b eaig path and batch machinery it already uses for chunks (the
  `INDEX_EMBED_BATCH_SIZE` tunable and the 4096-dim guard apply unchanged). The control plane writes it
  onto the node in the existing `MERGE (:Symbol …)` upsert.
- **The write batches via `UNWIND` from day one, not as a later optimization.** A 4096-dim `float`
  array is a large per-row payload; upserting one `:Symbol` per statement (as the pre-embedding
  `upsert_graph` does today) would be a real latency/throughput regression once every node carries a
  vector. `upsert_graph` ([`integrations/neo4j.rs`](../../services/control-plane/src/integrations/neo4j.rs))
  takes an `UNWIND $rows AS row MERGE (s:Symbol {…}) SET s.embedding = row.embedding` shape, batched at
  the same `INDEX_EMBED_BATCH_SIZE` the embedder already chunks at — one round trip per batch, not per
  symbol.
- The control plane creates the Neo4j vector index idempotently at bootstrap / first index:
  ```cypher
  CREATE VECTOR INDEX symbol_embedding IF NOT EXISTS FOR (s:Symbol) ON (s.embedding)
  OPTIONS {indexConfig: {`vector.dimensions`: 4096, `vector.similarity_function`: 'cosine'}}
  ```
- pgvector's `code_chunks` exact chunk search is **unchanged**. ADR-0003 is amended, not superseded:
  Neo4j now carries *structure + symbol-level semantics*; pgvector carries *chunk-level semantics*.
- The tools that consume this land in a companion decision, [ADR-0090](0090-hybrid-retrieval-tools.md).

### Consequences

- **Good** — unlocks graph-native hybrid retrieval ([ADR-0090](0090-hybrid-retrieval-tools.md));
  symbol-granular semantic recall complements chunk recall; the exact pgvector path is untouched; the
  incremental write is one property + one index, reusing the embedder we already run.
- **Bad** — **+N embedding calls per index** (symbols ≈ chunks, so roughly **2× index-time embedding
  cost/latency**; watch eaig cost and the batch tunable — this is the main price, and Option B is the
  escape hatch if it bites); a **second, approximate** vector index to keep in sync with the graph
  write; Neo4j-community is a **single instance** and now carries semantic-search load too.
- **Neutral** — 4096 sits at Neo4j 5.26's **maximum** vector dimension: confirm the index accepts it
  at build time (it should); an embedder wider than 4096 later would force this lane onto pgvector or
  a dimensionality reduction. Recall becomes **approximate** for this lane (HNSW) — acceptable because
  it is a *new* lane, and the exact pgvector lane remains for when exactness matters.
