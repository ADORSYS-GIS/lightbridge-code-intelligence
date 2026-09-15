# ADR-0116: One walk, and a symbol's vector rides the chunk that is its body

- **Status:** Accepted
- **Date:** 2026-09-15
- **Deciders:** @leghadjeu-christian
- **Amends:** [ADR-0114](0114-hybrid-graph-vector-symbol-search.md) (§"Who writes it" and its
  `lci-codegraph`-stays-embeddings-free driver; the hybrid-search half stands unchanged),
  [ADR-0089](0089-embeddings-on-the-code-graph.md) (its **Option B** — reuse chunk vectors rather
  than embed a second time — is adopted here, fixing *which* chunk at the same time),
  [ADR-0010](0010-graphify-treesitter-indexing-baseline.md) (`agent-runner`'s own chunker retires)
- **Source of truth:** #654, #652, #651

## Context and Problem Statement

Indexing runs **two independent chunkers over the same checkout** and joins their outputs by a line
range.

`agent-runner`'s `indexer/chunker.rs` produces the chunks that become `code_chunks` rows.
`lci-codegraph`'s walk independently produces the structural graph — and its own chunks, which the
runner discards. To give a `:Symbol` a vector, `index_graph` then searches the *first* chunker's
output for a chunk whose `[start_line, end_line]` span contains the symbol's start line, and embeds
that chunk's text a **second** time (#652) through the same model that already embedded it.

That join is a guess, and measurement shows it is frequently wrong. Should the two halves of the
index keep coming from two parses joined by position, or from one parse that records the link?

## Evidence

Measured on a fresh clone of `cratestack/cratestack` @ `main`, by running each side's real code
(`agent-runner`'s `chunk_file`, and `lci-codegraph` at the pinned `b3062f86`).

### Coverage

| | count |
|---|---|
| graph nodes | 15,984 |
| file-level nodes (a file is not a symbol; no vector expected) | 2,122 |
| **real symbol nodes** | **13,862** |
| …in file types `agent-runner`'s chunker cannot read at all | 1,838 |
| …readable file, still no covering chunk | 951 |
| symbol nodes carrying **a** vector | 11,073 (79.9%) |

The 1,838 are structural, not incidental: `lci-codegraph` graphs 15 languages and `agent-runner`'s
chunker handles 7. Every Dart (1,407), `.cstack` (737) and Swift (24) symbol is permanently
un-embeddable while the two chunkers differ.

### The join picks the wrong chunk, systematically

```
of 11,169 hits, >1 chunk covers the symbol's line:  2,562  (22.9%)
  where .find()'s first match != the narrowest:     2,562  (100% of those)
```

100% is not noise. `collect_items` pushes a container's chunk and *then* recurses into its children,
so the enclosing `impl` always precedes its methods in the slice, and `.find()` takes the first
match:

```
ping() @ crates/cratestack-api/tests/async_fn_impls.rs:34
    picked   : impl      lines 32-45     ← the whole impl block's text
    narrowest: function 'ping' lines 33-44
```

Every method on one `impl` therefore receives an **identical** vector — the container's. Net, roughly
**8,511 of 13,862 symbol nodes (61.4%)** carry a vector that actually corresponds to that symbol.

### What that costs retrieval — measured against a live model

Sampled 38 methods drawn from the mis-pick population, spread across 10 `impl` blocks, and embedded
both variants plus a natural-language query per symbol (`qwen3-embedding-8b` via the configured
gateway, 1536-dim). Query = the humanised symbol name; task = rank the right symbol among the 38.

```
distinct stored texts — OLD 10, NEW 38

   OLD  top-1  7/38 (18.4%)   MRR 0.422   median rank 3
   NEW  top-1 29/38 (76.3%)   MRR 0.851   median rank 1
```

Under the current join, 38 distinct symbols collapse into **10 distinguishable vectors**, so
`lightbridge_graph_semantic_search` cannot tell them apart — which is the exact capability ADR-0114
built. The measurement is on the mis-pick population specifically (22.9% of matches), not a
whole-corpus average, and the local gateway is 1536-dim where prod is 4096; both are reasons to read
it as direction and magnitude, not as a precise prod number.

### What the link is worth

`lci-codegraph` already records, during the parse that emits the node, which chunk is a definition's
body:

```rust
fn link_chunk_node_ids(path: &str, file_symbols: &FileSymbols, chunks: &mut [Chunk]) {
    let nodes = file_symbols.nodes();
    for chunk in chunks {
        let key = chunk.symbol_name.as_deref().unwrap_or(chunk.chunk_type.as_str());
        let candidate = def_node_id(path, i64::from(chunk.start_line) + 1, key);
        if nodes.iter().any(|n| n.node_id == candidate) {
            chunk.node_id = Some(candidate);   // only when that node genuinely exists
        }
    }
}
```

Using it instead of the range join:

```
symbol nodes with an EXACT node_id chunk match: 13,633 / 13,862  (98.3%)
```

## Decision Drivers

- **A join that is right 61% of the time is worse than no join.** A wrong vector is not a smaller
  version of a right one — it makes two different symbols look identical to a vector query.
- **One parse, one source of truth.** The checkout is currently parsed twice by two chunkers whose
  disagreement *is* the bug. Removing the second is the fix; tuning the join is not.
- **Do not spend embedding budget on a partly-wrong index.** #651 is open because index jobs exhaust
  their deadline; paying that cost for a 61%-correct symbol index is the worst of both.
- **No new trust surface, no new credential.** The runner already holds the embeddings credential and
  already calls both internal endpoints.

## Considered Options

### A — One walk; the chunk carries `node_id` to its symbol (chosen)

`agent-runner` runs `lci_codegraph::walk_checkout_from_env` once and uses **both** halves of its
output. `ChunkPayload` gains `node_id`; the control plane attaches that chunk's vector to the
matching `:Symbol`. The structural submit carries no vectors at all. `agent-runner`'s own chunker is
deleted.

### B — Keep two chunkers, pick the narrowest covering chunk

Five lines: `filter(...).min_by_key(|c| c.end_line - c.start_line)` instead of `.find(...)`. Recovers
the 2,562 mis-picks (61.4% → 79.9% correct) and is worth having as a stopgap if A slips. Rejected as
the decision: it leaves 2,789 symbols with no vector at all, leaves the checkout parsed twice, and
keeps a positional guess where an exact identity is available.

### C — Fix the duplication only, leaving the join alone

Stop the second embedding call by reusing the vector the semantic pass already computed for the
chunk the range join selected (ADR-0089's Option B). Removes real waste — 25,413 texts sent to the
model per run on cratestack, of which 14,244 are distinct — and is output-equivalent, since both
passes embed the same string. Rejected **as the whole answer**: it makes the index cheaper to build
without making it correct, and the measurement below is what changed that calculus. Option A does the
same thing (one embed per text) as a consequence of having one chunker, so nothing is lost by going
further.

### D — Have `lci-codegraph` embed its own chunks

The crate ships an `embed` module with a graph-aware context header (container, callees, callers).
Attractive for retrieval quality, and a natural follow-up. Rejected **here**: it swaps an async client
that understands the gateway's rate-limit headers, attribution headers and private CA for a blocking
`ureq` that does not, and it changes what the vectors *mean* — which deserves its own evaluation
rather than riding along with a correctness fix.

### E — Teach `agent-runner`'s chunker the missing 8 languages

Closes the coverage gap without touching the join. Rejected: it doubles down on maintaining a second
chunker whose divergence from the graph pass is the root cause, and does nothing about the 22.9%
mis-pick rate.

## Decision Outcome

Chosen option: **A**.

**One walk.** `indexer::walk` calls `walk_checkout_from_env` once; `IndexOutput` carries the chunks
and the graph from the same parse. `services/agent-runner/src/indexer/chunker.rs` and `language.rs`
are deleted.

**Structure first, then vectors.** `index_graph` submits nodes and edges only:

```rust
.map(|n| GraphNodePayload {
    node_id: n.node_id.clone(),
    label: n.label.clone(),
    source_file: n.source_file.clone(),
    start_line: n.start_line,
})
```

Then `index_chunks` submits each chunk with the `node_id` the walk linked:

```rust
.map(|(c, embedding)| ChunkPayload {
    /* … */
    embedding,
    node_id: c.node_id.clone(),
})
```

**The control plane attaches by identity, with `MATCH` not `MERGE`:**

```cypher
UNWIND $rows AS r
MATCH (s:Symbol {repo_id: $repo, commit: $commit, node_id: r.id})
SET s.embedding = r.embedding
RETURN count(s) AS updated
```

`MATCH` is load-bearing: a vector whose symbol is absent — because the best-effort structural submit
failed — is dropped rather than creating a `:Symbol` carrying an embedding and no structural facts.
The attach runs **after** the pgvector write and is itself best-effort, so a graph outage cannot
reject a batch that already landed in the store review actually depends on.

### Consequences

- **Good** — symbol-vector correctness goes from ~61.4% to **98.3%** of real symbol nodes, and every
  symbol gets its *own* text rather than its container's. Measured retrieval on the affected
  population: top-1 18.4% → 76.3%.
- **Good** — the checkout is parsed **once**, not twice.
- **Good** — semantic coverage extends from 7 languages to 15 (Dart, Swift, Kotlin, TSX, JSON,
  Jinja2, Postgres, `.cstack`), which no amount of join-fixing would have reached.
- **Good** — the structural submit carries no vectors, so its payload drops from ~582 MB to ~8.8 MB
  on a repo this size. That is the `ingest_graph` 32 MiB body-limit failure in #651, fixed as a
  consequence rather than as separate work.
- **Good** — the O(n·m) correlation scan (157,954,980 comparisons) disappears entirely; there is
  nothing left to correlate.
- **Good** — each text is embedded exactly **once** per run, closing #652. Where the structural pass
  used to re-embed 11,169 chunks it had already embedded, there is now a single embed loop: 25,413
  texts → 17,097 on cratestack, and 5,083 embedding calls → 3,420 at `INDEX_EMBED_BATCH_SIZE=5`.
- **Good** — no schema migration. `node_id` is used only to route the vector to Neo4j; `code_chunks`
  is unchanged.
- **Bad** — **every repository must be re-indexed.** Chunk boundaries change, so existing snapshots
  are stale. Not a data migration (a new index writes a new `commit_sha` snapshot and the ADR-0052
  sweeper prunes the old one), but it is real operational work and it is not free while #651 stands.
- **Bad** — chunk count rises ~20% (14,244 → 17,097 on cratestack), because eight more languages are
  now indexed. Against today's *distinct*-text count (14,244) that is ~20% more embedding work, though
  against what today actually *sends* (25,413, because of the second pass) it is ~33% less.
- **Bad** — pgvector retrieval results shift, because chunk boundaries differ. Worth a before/after
  spot-check on a couple of repositories rather than assuming parity.
- **Neutral** — chunk-shape tuning moves from `INDEX_MAX_CHUNK_LINES` / `INDEX_WINDOW_SIZE` /
  `INDEX_WINDOW_STEP` to `lci-codegraph`'s `LCI_CODEGRAPH_*` equivalents, whose defaults are
  identical (150 / 100 / 50), so chunk shape is unchanged where nobody has tuned it. Verified
  against `ai-helm` and `ai-helm-values`: none of the three is set in any environment, and the only
  `INDEX_*` var set anywhere is `INDEX_EMBED_BATCH_SIZE: "5"` on the prod dispatcher, which keeps its
  name and meaning. `LCI_CODEGRAPH_IGNORE_GLOBS` is new capability — an operator ignore layer the old
  chunker had no equivalent for.
- **Bad** — `INDEX_MAX_CHUNK_BYTES` has **no** `LCI_CODEGRAPH_*` counterpart: the crate bounds chunk
  shape in lines only. The old chunker's `cap_chunk_bytes` pass was, in its own words, *"the single
  place that guarantees every chunk fits an embedding model's input"*, and it **split** an oversized
  chunk into line-bounded pieces, so every byte stayed searchable. Deleting the chunker deletes that.
  Two separable concerns were entangled in that one pass:
  - **Request safety** — never send the model more than it accepts. That belongs to the embeddings
    client, which is the only layer that knows the model's limit, and which now enforces it for
    *every* embed path (`EMBEDDINGS_MAX_INPUT_BYTES`, default 16,000) rather than just the indexer's.
    One oversized string fails the whole batched request, and `index_chunks` propagates that with
    `?`, so an unbounded input could fail a whole index run.
  - **Retrieval coverage** — every byte reachable by search. That belongs to the chunker, and is
    **not** restored here: content past the ceiling is stored in `code_chunks` but is not represented
    in its vector. Tracked as [lci-codegraph#55](https://github.com/ADORSYS-GIS/lci-codegraph/issues/55),
    to restore `max_chunk_bytes`/`cap_chunk_bytes` upstream where one chunker owns chunk bounds.
    Re-adding a split in `agent-runner` was rejected: it re-creates the second chunker this ADR
    removes. Measured on cratestack: 9 of 9,627 windows (0.09%) exceed 16 KB, largest 33 KB, all
    `CHANGELOG.md` slices or long-line design docs.

## Out of scope

- Option D — `lci-codegraph` embedding its own chunks with the graph-aware context header.
- The remaining #651 work: resumption, batch sizing, and concurrency in the embed loop. This change
  removes one of that issue's two hard failures (the body limit) but not the deadline pressure.

## More Information

- [ADR-0114](0114-hybrid-graph-vector-symbol-search.md) — the hybrid search this protects; its
  indexes, WRRF query and MCP tool are untouched.
- [ADR-0089](0089-embeddings-on-the-code-graph.md) — the original symbol-embeddings proposal. Its
  Option B (reuse chunk vectors) remains what happens; this ADR fixes *which* chunk.
- [ADR-0086](0086-in-house-code-graph-crate.md) — `lci-codegraph` as the sole structural engine;
  this completes that by making it the sole chunker too.
- #654 — the ticket this implements. #652 — the duplicate-embedding ticket it also closes.
  #651 — the indexing-deadline failure it partly relieves.
- `services/agent-runner/src/indexer/{mod,graph}.rs`,
  `services/agent-clients/src/control_plane/indexing.rs`,
  `services/control-plane/src/http/internal.rs`,
  `services/control-plane/src/integrations/neo4j.rs`.
