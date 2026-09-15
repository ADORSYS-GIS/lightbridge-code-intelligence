# ADR-0116: Symbol embeddings reuse the chunk pass's vectors

- **Status:** Accepted
- **Date:** 2026-09-15
- **Deciders:** @leghadjeu-christian
- **Amends:** [ADR-0114](0114-hybrid-graph-vector-symbol-search.md) (§"Who writes it" and its cost
  line — the hybrid-search half is untouched and stands), [ADR-0089](0089-embeddings-on-the-code-graph.md)
  (adopts its **Option B**, which it named as the escape hatch for exactly this cost)
- **Source of truth:** #652, #651

## Context and Problem Statement

Indexing embeds every chunk once for pgvector, then embeds a second time to give each `:Symbol` node
a vector. The two passes send **the same strings** to the same model: the structural pass resolves a
symbol to the chunk covering its start line and embeds that chunk's `content` — the exact text the
semantic pass embedded moments earlier.

Should the structural pass keep calling the embeddings endpoint for text whose vector is already in
memory?

## Context that changed since ADR-0114

ADR-0114 accepted the doubling knowingly, restating ADR-0089's cost line: *"roughly double the
embedding-API calls at index time (symbols ≈ chunks in count)"*. Two things have since made that
price concrete rather than theoretical:

1. **It is measurable and large.** On `cratestack/cratestack` (2,214 indexable files) the semantic
   pass embeds 14,244 chunks and the structural pass re-embeds 11,169 of them — 25,413 texts sent to
   the model, of which 14,244 are distinct. At the deployed `INDEX_EMBED_BATCH_SIZE=5` that is 5,083
   sequential embedding calls where 2,849 would do. Index jobs on this repository do not finish
   inside `activeDeadlineSeconds` (#651); this pass is a measured 28% of their round trips.
2. **The fidelity argument for paying it no longer holds.** ADR-0089 rejected Option B as the
   default because chunk↔symbol attribution is "fuzzy and loses fidelity" — a chunk can hold several
   definitions, a large definition can span several chunks. But the code ADR-0114 shipped **already
   embeds chunk text**, not the symbol's own definition span, and takes the first range match. The
   implementation therefore already has Option B's fidelity characteristics while paying Option A's
   cost. Reuse settles the cost without moving the fidelity.

A third fact is worth recording because it retires one of ADR-0114's decision drivers. That ADR
argued *"`lci-codegraph` … stays embeddings-free. It's a pure tree-sitter structural walker today,
with no HTTP client and no credentials"*. The pinned revision (`b3062f86`) now ships an `embed`
module and carries `Chunk::node_id`, linking a chunk to the graph node it is the body of during the
same parse. That premise is simply no longer true — but this decision does not depend on it, and
deliberately does not act on it (see [Out of scope](#out-of-scope)).

## Decision Drivers

- **A vector that has been computed should not be computed again.** Nothing about the second call is
  informative; it is deterministic recomputation of a known result.
- **No change to what is stored.** `:Symbol.embedding` must hold the same value for the same set of
  nodes, so the ADR-0114 hybrid query and the `lightbridge_graph_semantic_search` tool behave
  identically before and after.
- **Independently shippable.** #651 needs several changes; this one must not be entangled with them,
  and must stand on its own merits if the others slip.
- **Smaller surface, not larger.** A change that removes a network dependency from a code path is
  worth more than one that adds a configuration knob to tune it.

## Considered Options

### A — Reuse the chunk pass's vectors (chosen)

`index_checkout` keeps each chunk's position and vector; `index_graph` attaches the vector of the
chunk covering a symbol instead of embedding that chunk's text again. The correlation rule is
untouched. The structural pass loses its `EmbeddingsClient` parameter entirely.

### B — Keep the second pass, batch it harder

Raise `INDEX_EMBED_BATCH_SIZE` so the second pass costs fewer round trips. Rejected as a decision,
though it remains a worthwhile tuning change under #651: it reduces the number of calls that carry
redundant work without removing the redundancy, and the batch size is bounded by the gateway's
response-size cap, which is why it is currently 5.

### C — Embed the symbol's own definition span instead of the chunk's text

The thing ADR-0089's Option A actually described, and the only version of the second pass that would
produce a genuinely different vector. Rejected: it requires `lci-codegraph` to expose a definition's
end line (which ADR-0114's own doc comment records as the reason the chunk-range correlation exists),
it would double index-time embedding for a retrieval improvement nobody has measured, and it moves in
precisely the wrong direction while #651 is open.

### D — Move embedding into `lci-codegraph`

Let the crate embed its own chunks with the graph-aware context header it now builds, and let the
host submit the result. Genuinely attractive — one pass, exact `node_id` linkage rather than a line
range, and a context header (container, callees, callers) that would improve retrieval. Rejected
**for this decision only**, on scope: it changes chunk boundaries, so it needs a full re-index of
every repository, and it swaps an async client that understands the gateway's rate-limit headers,
attribution headers and private CA for a blocking `ureq` that does not. It deserves its own ADR and
its own migration, not a rider on a duplication fix.

## Decision Outcome

Chosen option: **A** — the structural pass reads vectors, it does not request them.

`index_checkout` returns `Vec<EmbeddedChunk>` — a chunk's `file_path`, `start_line`, `end_line` and
`embedding`. Chunk text is deliberately not carried: with no second embedding call there is nothing
left in the structural pass that needs it.

```rust
pub struct EmbeddedChunk {
    pub file_path: String,
    pub start_line: i32,
    pub end_line: i32,
    pub embedding: Vec<f32>,
}
```

`index_graph` builds its node payloads in one pass with no `await` inside it:

```rust
let nodes: Vec<GraphNodePayload> = out
    .graph
    .nodes
    .iter()
    .map(|n| GraphNodePayload {
        node_id: n.node_id.clone(),
        label: n.label.clone(),
        source_file: n.source_file.clone(),
        start_line: n.start_line,
        embedding: embedding_for(chunks, &n.source_file, n.start_line),
    })
    .collect();
```

What does **not** change:

- The correlation rule (`chunk_contains_symbol`) — same range containment, same first match.
- The set of nodes that carry an embedding, and the set that do not.
- The `GraphBatch` payload shape, the `ingest_graph` endpoint, and the `UNWIND … MERGE (:Symbol …)`
  upsert, whose `size(n.embedding) > 0` sentinel still means a structure-only re-index leaves an
  existing vector alone.
- Everything in ADR-0114 downstream of the write: both Neo4j indexes, the WRRF query, and
  `lightbridge_graph_semantic_search`.

### Consequences

- **Good** — index-time embedding drops to one call per distinct text. On cratestack: 25,413 texts
  → 14,244 (−44%), and 5,083 embedding calls → 2,849 at the deployed batch size.
- **Good** — `index_graph` makes no embeddings call, so an embeddings outage or rate-limit stall can
  no longer be introduced by the structural pass. It still makes exactly one outbound call, the
  `submit_graph` POST carrying the nodes, edges and their vectors — `:Symbol.embedding` is written
  for the same nodes as before. It takes one fewer parameter.
- **Good** — ADR-0089's accepted "~2× index-time embedding cost" is retired rather than carried
  forward, using the escape hatch that ADR itself specified.
- **Neutral** — memory. The run holds 14,244 × 4096 × 4 B ≈ 233 MB of vectors, against a runner
  limit of 4Gi. Roughly where it already was: the structural pass previously accumulated
  11,169 × 4096 ≈ 183 MB into its own `nodes` vector, and chunk text is no longer retained.
- **Neutral** — the equivalence rests on the embeddings endpoint being deterministic for identical
  input. True of `qwen3-embedding-8b` through the eaig gateway, but not a guarantee the
  OpenAI-compatible API makes. A non-deterministic endpoint would change `:Symbol.embedding` values
  between runs today anyway, in a way nothing depends on.
- **Bad** — the graph submit still carries every vector inline, so it still exceeds `ingest_graph`'s
  32 MiB body limit on a repository this size (#651). This decision removes the cost of *producing*
  those vectors, not the cost of *shipping* them.

## Out of scope

Tracked in #651, deliberately not bundled here:

- The `ingest_graph` body-limit failure on large repositories.
- The O(n·m) correlation scan (`chunks.iter().find(…)` per node) — 157,954,980 comparisons on
  cratestack, and a `HashMap` keyed by file path away from O(nodes + chunks).
- Resumption, batch sizing, and concurrency in the semantic pass.

Option D above — `lci-codegraph` owning chunking and embedding end-to-end — needs its own decision.

## More Information

- [ADR-0089](0089-embeddings-on-the-code-graph.md) — symbol embeddings on `:Symbol`; **Option B**,
  adopted here, is its own named fallback "if index-time embedding cost bites".
- [ADR-0114](0114-hybrid-graph-vector-symbol-search.md) — the shipped hybrid search this amends in
  one place and otherwise leaves intact.
- [ADR-0086](0086-in-house-code-graph-crate.md) — `lci-codegraph` as the sole structural engine.
- [ADR-0018](0018-openai-compatible-embeddings.md) — the eaig embeddings path and its 4096-dim model.
- #652 — the ticket this implements. #651 — the indexing-deadline failure it contributes to.
- `services/agent-runner/src/indexer/mod.rs`, `services/agent-runner/src/indexer/graph.rs`,
  `services/control-plane/src/integrations/neo4j.rs`.
