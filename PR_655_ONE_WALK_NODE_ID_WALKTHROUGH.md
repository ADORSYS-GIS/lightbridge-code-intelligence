# PR #655 — One walk, and a symbol's vector rides its own chunk

> Walkthrough of [PR #655](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/pull/655)
> · closes [#654](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/654),
> [#652](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/652)
> · refs [#651](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/651)
> · [ADR-0116](docs/adr/0116-one-walk-node-id-symbol-embeddings.md)
> · supersedes [#653](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/pull/653)
>
> 17 files · +795 / −845

---

## The one-paragraph version

Indexing parsed the checkout **twice**, with two different chunkers, and joined their outputs by a
line range. That join gave the **wrong vector to ~38% of symbol nodes** — systematically, because
every method on an `impl` received the enclosing block's vector. This PR deletes the second chunker,
runs one walk, and lets each chunk carry the `node_id` of the definition it is the body of. The join
disappears; identity replaces position.

---

## 1. What was actually broken

Two chunkers, and nothing connected them:

| | `agent-runner`'s `chunker.rs` | `lci-codegraph`'s walk |
|---|---|---|
| produced | 14,244 chunks | 17,097 chunks **with `node_id`** |
| languages | 7 | 15 |
| used for | `code_chunks` rows (pgvector) | `out.graph` only |
| its chunks | embedded and stored | **discarded** |

Verified by counting field accesses in the old `index_graph`:

```
out.graph.nodes -> 1
out.graph.edges -> 1
out.chunks      -> 0     # never read
```

So the pipeline computed the exact chunk↔symbol link it needed, in `out.chunks`, and threw it away —
then reconstructed a worse version by searching the *other* chunker's output by position.

### The join, and why it failed 100% of the time it mattered

```rust
// OLD — indexer/graph.rs
if let Some(chunk) = chunks
    .iter()
    .find(|c| chunk_contains_symbol(c, &n.source_file, n.start_line))
{
    embeddable.push((nodes.len(), chunk.content.as_str()));
}
```

`chunk_contains_symbol` is range containment, and `.find()` takes the **first** match. `collect_items`
pushes a container's chunk and *then* recurses into its children, so the enclosing `impl` always
precedes its methods in the slice:

```
of 11,169 hits, >1 chunk covers the symbol's line:  2,562  (22.9%)
  where .find()'s first match != the narrowest:     2,562  (100% of those)

ping() @ crates/cratestack-api/tests/async_fn_impls.rs:34
    picked   : impl      lines 32-45     ← the whole impl block's text
    narrowest: function 'ping' lines 33-44
```

**Every method on one `impl` got an identical vector.** Measured against a live
`qwen3-embedding-8b` — 38 methods from 10 `impl` blocks, query = humanised symbol name, rank the
right symbol among the 38:

```
distinct stored texts — OLD 10, NEW 38

   OLD  top-1  7/38 (18.4%)   MRR 0.422   median rank 3
   NEW  top-1 29/38 (76.3%)   MRR 0.851   median rank 1
```

That is the capability [ADR-0114](docs/adr/0114-hybrid-graph-vector-symbol-search.md) built
`lightbridge_graph_semantic_search` to provide, and the join was quietly defeating it.

---

## 2. Old flow

```mermaid
flowchart TD
    CO["Repo checkout"]

    subgraph P1["index_checkout — semantic"]
        C1["agent-runner chunker<br/>7 languages"]
        C1 --> CH["14,244 chunks"]
        CH --> E1{{"embed(chunk.content)"}}
        E1 --> S1["submit_chunks → pgvector"]
    end

    subgraph P2["index_graph — structural"]
        C2["lci-codegraph walk<br/>15 languages"]
        C2 --> DIS["17,097 chunks<br/>WITH node_id — DISCARDED"]
        C2 --> GN["15,984 nodes / 28,813 edges"]
        GN --> CORR["chunks.iter().find(range)<br/>158M comparisons<br/>22.9% pick the wrong chunk"]
        CORR --> E2{{"embed(chunk.content) AGAIN"}}
        E2 --> S2["submit_graph<br/>~582 MB · 32 MiB limit"]
    end

    CO --> C1
    CO --> C2
    S2 --> NEO[("Neo4j :Symbol")]
    S1 --> PG[("pgvector")]

    style DIS fill:#fbf1e7,stroke:#a9591f,color:#111
    style CORR fill:#fbecee,stroke:#a92b36,color:#111
    style E2 fill:#fbecee,stroke:#a92b36,color:#111
    style S2 fill:#fbecee,stroke:#a92b36,color:#111
```

## 3. New flow

```mermaid
flowchart TD
    CO["Repo checkout"]
    W["indexer::walk — ONE parse<br/>lci-codegraph, 15 languages"]
    CO --> W
    W --> OUT["IndexOutput<br/>17,097 chunks (13,633 with node_id)<br/>15,984 nodes / 28,813 edges"]

    OUT --> G["① index_graph<br/>structure only, no vectors<br/>~8.8 MB"]
    G --> NEO[("Neo4j :Symbol<br/>MERGE by node_id")]

    OUT --> IC["② index_chunks<br/>embed once, carry node_id"]
    IC --> S1["submit_chunks"]
    S1 --> PG[("pgvector code_chunks")]
    S1 --> ATT["attach_symbol_embeddings<br/>MATCH by node_id"]
    ATT --> NEO

    style W fill:#e6f3f1,stroke:#0e7d73,color:#111
    style ATT fill:#e6f3f1,stroke:#0e7d73,color:#111
```

The ordering is load-bearing: **① before ②**, because the attach uses `MATCH`, not `MERGE`.

---

## 4. The implementation, file by file

### `indexer/mod.rs` — the walk becomes its own step

`index_checkout` is gone. In its place, three smaller things: a walk, a structural submit, a chunk
submit.

```rust
/// Walk the checkout once, producing the chunks and the structural graph from a single parse.
///
/// Tree-sitter parsing is synchronous CPU work, so it runs on a blocking thread rather than stalling
/// the async runtime.
pub async fn walk(checkout: &Path) -> anyhow::Result<IndexOutput> {
    let checkout = checkout.to_path_buf();
    tokio::task::spawn_blocking(move || {
        lci_codegraph::walk_checkout_from_env(&checkout, /* build_graph */ true)
    })
    .await
    .context("codegraph walk task panicked")?
    .context("codegraph walk failed")
}
```

Then `index_chunks` embeds from `out.chunks` — the output that used to be thrown away — and passes
`node_id` straight through:

```rust
let payloads: Vec<ChunkPayload> = batch_chunks
    .iter()
    .zip(embeddings)
    .map(|(c, embedding)| ChunkPayload {
        file_path: c.file_path.clone(),
        language: c.language.clone(),
        chunk_type: c.chunk_type.clone(),
        symbol_name: c.symbol_name.clone(),
        start_line: c.start_line,
        end_line: c.end_line,
        content: c.content.clone(),
        embedding,
        node_id: c.node_id.clone(),   // ← the link, recorded during the parse
    })
    .collect();
```

There is now **one** embed call per text, by construction — not as a fix, but because there is only
one chunker. That is why this PR subsumes #652.

A log line worth having, since it makes the coverage visible per run:

```rust
let linked = out.chunks.iter().filter(|c| c.node_id.is_some()).count();
tracing::info!(
    chunk_count = total,
    linked_to_a_symbol = linked,
    embed_batch_size = batch_size,
    "walk complete; embedding in batches"
);
```

`IndexTuning`'s five fields collapse to one function, because chunk *shape* is now the crate's
business:

```rust
/// Chunks embedded and submitted per round-trip. …
/// Chunk shape itself — window size, line and byte ceilings — is `lci-codegraph`'s to tune, under
/// its own `LCI_CODEGRAPH_*` variables.
const DEFAULT_EMBED_BATCH_SIZE: usize = 32;

pub fn embed_batch_size() -> usize {
    std::env::var("INDEX_EMBED_BATCH_SIZE")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map(|n| n.max(1))
        .unwrap_or(DEFAULT_EMBED_BATCH_SIZE)
}
```

### `indexer/graph.rs` — structure only

157 lines become 79. The whole correlate-and-embed apparatus is deleted: `embeddable`, the
`(usize, &str)` index tuples, the back-patching loop, `chunk_contains_symbol`, and the `embedder`
parameter.

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
    })
    .collect();
```

No `embedding` field exists to populate. The doc comment carries the ordering contract:

```rust
/// A symbol's `:Symbol.embedding` arrives separately, on the chunk that is its body, keyed by
/// `node_id` (ADR-0116). Run this **before** [`super::index_chunks`] so a symbol exists by the time
/// its vector is offered; a vector whose symbol is absent is dropped rather than creating a node
/// with no structural facts.
```

### `chunker.rs` and `language.rs` — deleted

427 lines. Every symptom traced back to their existing alongside a second chunker.

### `run.rs` — the order made explicit

```rust
// One `lci-codegraph` walk produces both halves of the index from a single parse (ADR-0086):
// the semantic chunks and the structural graph.
let out = indexer::walk(checkout).await?;

// Submitted first so each `:Symbol` exists before `index_chunks` offers its vector (ADR-0116).
// Best-effort: the graph store may be unconfigured (control plane returns 503). A graph failure
// is logged, not fatal — the task still succeeds with the semantic index alone, and the symbols
// that failed to land simply carry no vector.
let graph_result = indexer::graph::index_graph(context, &out, client).await;
let graph = match graph_result { /* … */ };

// ── Semantic index: chunks → pgvector, and each linked chunk's vector → its `:Symbol` ──
let chunk_count = indexer::index_chunks(context, &out, client, embedder).await?;
```

Note the failure asymmetry is preserved and now reads correctly: the graph is best-effort (`match`),
the chunk submit is fatal (`?`).

### `agent-clients` — the wire contract

```rust
pub struct ChunkPayload {
    /* … */
    pub embedding: Vec<f32>,
    /// The graph node this chunk is the body of, when `lci-codegraph` linked one during the parse
    /// that produced both. The control plane attaches this chunk's vector to that `:Symbol`
    /// (ADR-0116). `None` for a chunk that is not a definition — a windowed slice, a text file, or a
    /// definition the graph pass did not emit a node for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
}
```

And `GraphNodePayload` loses `embedding` entirely — the type now makes "the structural submit carries
no vectors" un-violatable rather than merely true.

### `control-plane/http/internal.rs` — routing the vector to two stores

```rust
// A chunk that is a definition's body carries that definition's `node_id`, so its vector serves
// both stores: the pgvector row below, and the matching `:Symbol` (ADR-0116).
let symbol_vectors: Vec<(String, Vec<f32>)> = batch
    .chunks
    .iter()
    .filter_map(|c| c.node_id.clone().map(|node_id| (node_id, c.embedding.clone())))
    .collect();
```

The handler's control flow changed shape: the pgvector write now `return`s early on failure instead
of being the tail expression, so the attach can follow it.

```rust
// Best-effort, and deliberately after the pgvector write: semantic search is the load-bearing
// store, so an unconfigured or failing graph must not reject a batch that already landed there.
if let Some(neo4j) = state.neo4j.as_ref()
    && !symbol_vectors.is_empty()
{
    match crate::integrations::neo4j::attach_symbol_embeddings(/* … */).await {
        Ok(updated) => tracing::info!(offered = symbol_vectors.len(), updated, "symbol embeddings attached"),
        Err(error) => tracing::warn!(%error, "attaching symbol embeddings failed (non-fatal)"),
    }
}
```

Logging `offered` next to `updated` is deliberate — a gap between them is exactly the symptom of a
structural submit that did not land.

### `control-plane/integrations/neo4j.rs` — `MATCH`, not `MERGE`

```cypher
UNWIND $rows AS r
MATCH (s:Symbol {repo_id: $repo, commit: $commit, node_id: r.id})
SET s.embedding = r.embedding
RETURN count(s) AS updated
```

```rust
/// `MATCH`, not `MERGE`: a vector whose symbol is not in the graph — because the structural submit
/// failed, or the snapshot predates this commit — is dropped rather than creating a `:Symbol`
/// carrying an embedding and nothing else.
```

`GraphNodeInput.embedding` is **kept** server-side with `#[serde(default)]`, so a pre-ADR-0116 runner
still works during a rollout, and `None` already means "leave any existing vector alone".

### `k8s.rs` — the forwarded knobs follow the chunker

```rust
for key in [
    "INDEX_EMBED_BATCH_SIZE",
    "INDEX_MAX_EMBED_BYTES",
    "LCI_CODEGRAPH_MAX_CHUNK_LINES",
    "LCI_CODEGRAPH_WINDOW_SIZE",
    "LCI_CODEGRAPH_WINDOW_STEP",
    "LCI_CODEGRAPH_IGNORE_GLOBS",
] {
```

Verified against `ai-helm` and `ai-helm-values` rather than assumed: the **only** `INDEX_*` variable
set anywhere is `INDEX_EMBED_BATCH_SIZE: "5"`, on the prod **dispatcher** — which is the right place,
since the dispatcher is what builds the Job manifest. The retired chunk-shape knobs appear in neither
repo, and the crate's defaults match them one-for-one (150 / 100 / 50), so chunk shape is unchanged
where nobody tuned it. `LCI_CODEGRAPH_IGNORE_GLOBS` is new capability: an operator ignore layer,
composed with the repo's own `.gitignore`, that `agent-runner`'s chunker had no equivalent for.

### The guard I nearly dropped — `INDEX_MAX_EMBED_BYTES`

`INDEX_MAX_CHUNK_BYTES` has **no** `LCI_CODEGRAPH_*` counterpart. The crate's `IndexTuning` has four
fields and bounds chunks in *lines* only. The old chunker's `cap_chunk_bytes` pass said why that
matters:

```rust
// Neither strategy above bounds a chunk's byte size on its own (a tree-sitter symbol with no
// splittable children, or a windowed slice of long unwrapped lines, can both come out huge) —
// this pass is the single place that guarantees every chunk fits an embedding model's input.
```

Deleting the chunker deletes that guarantee. Measured on cratestack: **9 of 9,627 windows (0.09%)
exceed 16 KB, the largest 33 KB** — `CHANGELOG.md` slices and long-line design docs. Small, but a
repo with minified JS or large JSON would be far worse.

So the ceiling is re-established host-side, bounding the text **sent to the model** rather than
splitting the chunk — splitting would break the 1:1 chunk↔`node_id` mapping this whole PR rests on:

```rust
/// The text to embed for `content`: the whole thing, or its first `max_bytes` bytes cut on a `char`
/// boundary so the slice can never split a multi-byte character.
fn embed_text(content: &str, max_bytes: usize) -> &str {
    if content.len() <= max_bytes {
        return content;
    }
    let mut end = max_bytes;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    &content[..end]
}
```

Stored `content` is never truncated — only the embed input. Truncations are counted and logged
(`truncated_for_embedding`) so the rate is visible per run.

The `open`-mode assertion widened to match:

```rust
assert!(
    !env.iter()
        .any(|e| e.name.starts_with("INDEX_") || e.name.starts_with("LCI_CODEGRAPH_")),
    "open must NOT carry index knobs"
);
```

---

## 5. Old vs new, side by side

| | Old | New |
|---|---|---|
| parses of the checkout | 2 | **1** |
| chunkers | 2, joined by line range | 1 |
| chunk→symbol link | `.find()` on range, first match | `Chunk::node_id`, recorded at parse |
| languages chunked | 7 | **15** |
| real symbol nodes | 13,862 | 13,862 |
| carrying **a** vector | 11,073 (79.9%) | **13,633 (98.3%)** |
| carrying the **right** vector | ~8,511 (61.4%) | **13,633 (98.3%)** |
| texts sent to the model | 25,413 | **17,097** |
| embed passes | 2 | **1** |
| node↔chunk comparisons | 157,954,980 | **0** |
| graph submit payload | ~582 MB (32 MiB limit) | **~8.8 MB** |
| retrieval top-1 (mis-pick population) | 18.4% | **76.3%** |

---

## 6. Verification

```bash
cargo build --workspace
DATABASE_URL=postgres://lightbridge:lightbridge@localhost:5432/lightbridge cargo test --workspace
docker compose up -d neo4j
cargo test -p control-plane --bin control-plane integrations::neo4j -- --ignored --test-threads=1
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

```text
$ DATABASE_URL=... cargo test --workspace
... 935 tests total, 0 failed

$ cargo test -p control-plane --bin control-plane integrations::neo4j -- --ignored --test-threads=1
running 3 tests
test integrations::neo4j::tests::attach_symbol_embeddings_writes_to_matching_symbols_only ... ok
test integrations::neo4j::tests::prune_graph_keeps_only_the_keep_set ... ok
test integrations::neo4j::tests::upsert_graph_round_trips_against_live_neo4j ... ok
test result: ok. 3 passed; 0 failed
```

**The attach path is verified against a real Neo4j, not mocked.** The new test asserts three things
the design depends on:

```rust
// 1. structure lands first, carrying no vectors
upsert_graph(&graph, repo, commit, &nodes, &[]).await?;
assert_eq!(symbol_embedding(…, "src/auth.rs#40:validate").await?, None);

// 2. one known symbol, one that is not in the graph
let rows = vec![
    ("src/auth.rs#40:validate".to_string(), vec![0.25f32; 8]),
    ("src/auth.rs#99:ghost".to_string(),    vec![0.75f32; 8]),
];
let updated = attach_symbol_embeddings(&graph, repo, commit, &rows).await?;
assert_eq!(updated, 1, "only the symbol that exists is updated");

// 3. MATCH not MERGE — the unknown node_id created nothing
assert_eq!(row.get::<i64>("n").unwrap(), 2,
           "a vector for an absent symbol must not create one");

// 4. a structure-only re-index does not wipe the attached vector
upsert_graph(&graph, repo, commit, &nodes, &[]).await?;
assert!(symbol_embedding(…).await?.is_some());
```

Two wire-contract tests pin the payload shape **both ways** — `node_id` present for a definition
chunk, and *absent from the JSON* (not `null`) for a windowed one, so the control plane's `Option`
default applies:

```rust
.and(body_json(serde_json::json!({
    "commit_sha": "abc123",
    "chunks": [{
        /* … */
        "embedding": [0.0, 0.0, 0.0, 0.0],
        "node_id": "src/main.rs#1:main",
    }],
})))
```

---

## 7. Risks, stated plainly

- **Every repository must be re-indexed.** Chunk boundaries change. Not a data migration — a new
  index writes a new `commit_sha` snapshot and the ADR-0052 sweeper prunes the old — but real
  operational work, and reviews keep reusing the stale base index (ADR-0025) until it runs.
- **pgvector retrieval results shift** with the new boundaries. This is *not* a behaviour-neutral
  change on the semantic side; worth a before/after spot-check on two repos.
- **Chunk count rises ~20%** (14,244 → 17,097), because eight more languages are indexed. Embedding
  work rises against today's *distinct*-text count, but falls ~33% against what today actually sends.
- **Ordering dependency** — the attach needs the structural submit to have landed. Enforced by call
  order in `run.rs` plus doc comments. Whether that should be structural is the first thing worth
  arguing about in review.
- **Re-index sequencing against #651.** Large repos currently cannot finish an index inside
  `activeDeadlineSeconds`. This PR removes the body-limit failure but not the deadline one.

**The fallback if the re-index cost is judged too high right now** is Option B in ADR-0116 — five
lines, no re-index, 61.4% → 79.9%:

```rust
chunks
    .iter()
    .filter(|c| chunk_contains_symbol(c, source_file, start_line))
    .min_by_key(|c| c.end_line - c.start_line)
    .map(|c| c.embedding.clone())
```

It leaves 2,789 symbols with no vector and the checkout parsed twice, so it is a stopgap, not the
answer.

---

## 8. Measurement caveats

Stated so nobody over-reads the numbers:

- **cratestack is Rust-heavy**, so 98.3% may not generalise. Worth re-running the harness on a
  TypeScript-dominant repo.
- **The retrieval test used a 1536-dim local gateway** where prod is 4096.
- **It sampled the mis-pick population specifically** (22.9% of matches), not the whole corpus — so
  18.4% → 76.3% is the improvement *on the symbols that were wrong*, not a corpus-wide average.

Read them as direction and magnitude, not as precise prod numbers.
