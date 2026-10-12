# Graph-submit timeout, the missing Neo4j index, and what #651 actually is

> Written 16 Sep 2026, from a real prod run: task `7e41350b-4998-4cdd-96b5-55b9cae87ad9`,
> `ADORSYS-GIS/CoopData` @ `develop`, on a build of
> [PR #655](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/pull/655).
>
> Related: [#651](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/651) ·
> [#654](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/654) ·
> [lci-codegraph#55](https://github.com/ADORSYS-GIS/lci-codegraph/issues/55)

---

## 1. What the CoopData run actually shows

Three lines carry the whole story:

```text
12:24:32  INFO  codegraph: index complete  files_chunked=788  chunks=11284
                                           graph_nodes=5154  graph_edges=7893
12:27:32  WARN  structural graph indexing failed (non-fatal)
                error="submitting codegraph structural graph: submitting graph:
                       error sending request for url (…/graph): operation timed out"
12:57:55  INFO  task succeeded  summary="indexed ADORSYS-GIS/CoopData at develop
                                         — 11284 chunks, graph skipped; review disabled"
```

`12:24:32 → 12:27:32` is **exactly 180 seconds**, which is `DEFAULT_REQUEST_TIMEOUT_SECS`:

```rust
request_timeout_secs: parse_env_u64("CONTROL_PLANE_REQUEST_TIMEOUT_SECS")
    .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS),   // 180
```

`CONTROL_PLANE_REQUEST_TIMEOUT_SECS` is set in neither `ai-helm` nor `ai-helm-values`, so the default
applies. The runner gave up; Neo4j may well have finished the write afterwards, unobserved.

### What it is not

- **Not a 413.** The payload is structure-only since #655 — 5,154 nodes + 7,893 edges with no vectors,
  a few MB. It reached Neo4j and was simply slow.
- **Not "Neo4j unconfigured".** `NEO4J_URI = bolt://lightbridge-ci-neo4j:7687` is set on the control
  plane, and prod `/metrics` shows **no** non-2xx on `/graph` at all.

### The honest read on #655

The PR did what it claimed — it turned a **guaranteed** 413 rejection into a request that actually
reaches the database. It also, unintentionally, **traded a size failure for a duration failure**,
because `submit_graph` is still one unbounded request. On the old code this same submit would have
carried ~5,154 embeddings (~260 MB) and been rejected outright, so this is progress, not a
regression — but it is an incomplete fix and I did not see it coming.

### The silent part

The task reported `succeeded`. By design — the graph is best-effort — but the consequence is that
**CoopData now has a semantic index and no structural index**. `graph_find_symbol`, `get_callers` and
`graph_semantic_search` all return nothing for it, and the review agent's own prompt instructs it to
read an empty result as *"could not verify"* rather than *"absent"*. So the degradation is invisible
at the task level and only shows up as weaker reviews.

---

## 2. Why the write is slow — the missing index

`ensure_indexes` creates exactly two indexes:

```rust
pub async fn ensure_indexes(graph: &Graph, dimension: i64) -> anyhow::Result<()> {
    create_index_idempotent(graph, &format!(
        "CREATE VECTOR INDEX symbol_embedding_idx IF NOT EXISTS \
         FOR (s:Symbol) ON s.embedding \
         OPTIONS {{ indexConfig: {{ \
           `vector.dimensions`: {dimension}, \
           `vector.similarity_function`: 'cosine' \
         }}}}"), "symbol_embedding_idx").await?;
    create_index_idempotent(graph,
        "CREATE FULLTEXT INDEX symbol_label_fulltext IF NOT EXISTS \
         FOR (s:Symbol) ON EACH [s.label, s.source_file]",
        "symbol_label_fulltext").await?;
    Ok(())
}
```

A vector index and a fulltext index. **Nothing indexes the identity properties every write and read
matches on.** Verified by grepping the whole repo — every other `CREATE INDEX` is a Postgres
migration.

Now look at what `upsert_graph` does on every submit:

```cypher
UNWIND $nodes AS n
MERGE (s:Symbol {repo_id: $repo, commit: $commit, node_id: n.id})
SET s.label = n.label, s.source_file = n.file, s.start_line = n.line,
    s.embedding = CASE WHEN size(n.embedding) > 0 THEN n.embedding ELSE s.embedding END
```

```cypher
UNWIND $edges AS e
MATCH (a:Symbol {repo_id: $repo, commit: $commit, node_id: e.src})
MATCH (b:Symbol {repo_id: $repo, commit: $commit, node_id: e.dst})
MERGE (a)-[r:REL {relation: e.rel}]->(b)
```

Without an index on `(repo_id, commit, node_id)`, **each of those `MERGE`/`MATCH` clauses is a label
scan over every `:Symbol` node in the database** — not just this repo's, not just this commit's.
Every repo, every retained commit snapshot.

For CoopData that is:

```
 5,154  node MERGEs
+7,893  edge rows × 2 MATCH each = 15,786
───────
20,940  lookups, each scanning the whole :Symbol population
```

And the population is cumulative. Cost per index run is roughly
`O(nodes_this_repo × symbols_everywhere)` — it degrades as the platform indexes more repos, which is
exactly the shape of "it used to work and now it doesn't".

Prod `/metrics` for the two graph POSTs that *did* complete:

```
http_request_duration_seconds_sum{…"/graph"}   98.06
http_request_duration_seconds_count{…"/graph"}     2      →  ~49 s each
```

49 s for a *successful* submit is not a healthy number; 180 s is a short hop from it.

### Which index — a uniqueness constraint, not a bare index

```cypher
CREATE CONSTRAINT symbol_identity IF NOT EXISTS
FOR (s:Symbol) REQUIRE (s.repo_id, s.commit, s.node_id) IS UNIQUE
```

Neo4j uses a composite index when the query supplies **all** its properties with equality — which is
precisely the shape of every `MERGE`/`MATCH` above, and of `find_symbol`, `get_callers`,
`symbol_embedding`, `prune_graph` and the new `attach_symbol_embeddings`. Property order matters for
partial matches, so lead with the most selective prefix that queries actually use: every one of them
supplies `repo_id` and `commit` together.

A constraint rather than a plain `CREATE INDEX` because it delivers **both** things at once: it
creates its own backing range index, *and* it enforces the invariant `MERGE` already assumes.

```
name              type     properties                          owningConstraint
symbol_identity   RANGE    ["repo_id","commit","node_id"]      symbol_identity
```

Measured on `neo4j:5` Community 5.26.29 with 60,018 `:Symbol` nodes across 3 repos — the same lookup
`upsert_graph` performs for every node and twice per edge:

```
PROFILE MATCH (s:Symbol {repo_id: 2, commit: 'demo-commit', node_id: 'src/f19999.rs#…'}) RETURN s.label

  without the constraint :  DbHits = 145,029    NodeByLabelScan
  with the constraint    :  DbHits =       3    NodeUniqueIndexSeek
```

**48,000× fewer database hits for one lookup.** 145,029 is ~60,018 nodes × the three properties
checked on each — the whole label scanned to find one row. `NodeUniqueIndexSeek` is also the
strongest operator the planner has for this shape, which a bare index would not give.

The uniqueness half is real too — a duplicate is now rejected rather than silently created:

```
Node(20018) already exists with label `Symbol` and properties `repo_id` = 2,
`commit` = 'demo-commit', `node_id` = 'src/f1.rs#1:sym1'
```

> **Correction.** An earlier draft of this document claimed composite uniqueness constraints are
> Neo4j Enterprise and recommended a bare range index instead. That was wrong, and tested wrong:
> composite `IS UNIQUE` creates fine on Community. What *is* Enterprise-only is `IS NODE KEY`
> (uniqueness **plus** a not-null guarantee on all three properties), which errors with
> *"Node Key constraint requires Neo4j Enterprise Edition"*. We do not want NODE KEY anyway — it
> would reject any `:Symbol` missing one of the three, a stricter contract than the code promises.

This is a **one-line, no-migration change** that likely buys more than everything else in this
document combined.

**One precondition.** Unlike a plain index, constraint creation *fails* if duplicate rows already
exist for a triple. Every write goes through `MERGE` on that exact key, so duplicates should be
impossible — but check before applying to prod, and fall back to `CREATE INDEX … ON (…)` if any turn
up:

```cypher
MATCH (s:Symbol)
WITH s.repo_id AS r, s.commit AS c, s.node_id AS n, count(*) AS dupes
WHERE dupes > 1
RETURN r, c, n, dupes LIMIT 10
```

---

## 3. Why paging is still needed

The index makes each lookup cheap. It does not bound the *request*.

One submit still carries the whole graph in one transaction, so its duration scales with repo size
against a fixed 180 s client timeout. A repo 5× CoopData's size re-crosses the line even with a fast
index. Paging makes duration scale with **batch size**, which is a constant we choose, rather than
with repo size, which we do not.

It also fixes a second-order problem: the whole graph currently lands in **one Neo4j transaction**,
so a failure anywhere rolls back everything, and the transaction holds locks for its entire duration.

### The ordering constraint that makes this non-trivial

Edges `MATCH` both endpoints. If edges are submitted before all their nodes exist, those edges are
silently dropped — `MATCH` that finds nothing is a no-op, not an error. So:

> **All node pages must land before any edge page.**

This is the same ordering rule ADR-0116 already relies on for `attach_symbol_embeddings`, just one
level down.

---

## 4. Implementation plan

### 4.1 — Neo4j identity index *(do this first, alone)*

**`services/control-plane/src/integrations/neo4j.rs`**

```rust
pub async fn ensure_indexes(graph: &Graph, dimension: i64) -> anyhow::Result<()> {
    // Every symbol read and write matches on the full identity triple — the graph upsert's MERGE,
    // the edge MATCHes, find_symbol, get_callers, symbol_embedding, prune_graph, and the
    // chunk-side embedding attach (ADR-0116). Unindexed, each is a label scan over every :Symbol in
    // the database, so a single repo's cost grows with every other repo indexed. A composite range
    // index applies when a query supplies all three with equality, which all of them do.
    //
    // A uniqueness constraint rather than a bare index: it creates its own backing range index and
    // additionally enforces the invariant MERGE already assumes. Measured on Community 5.26 with
    // 60k symbols, one lookup goes from 145,029 db hits (NodeByLabelScan) to 3 (NodeUniqueIndexSeek).
    create_index_idempotent(
        graph,
        "CREATE CONSTRAINT symbol_identity IF NOT EXISTS \
         FOR (s:Symbol) REQUIRE (s.repo_id, s.commit, s.node_id) IS UNIQUE",
        "symbol_identity",
    )
    .await?;
    // … existing vector + fulltext indexes unchanged
}
```

`create_index_idempotent` already swallows the "already exists" race, so this is safe under the
multi-role startup it is called from.

**Verification** — run before and after against the live instance:

```cypher
PROFILE MATCH (s:Symbol {repo_id: 123, commit: 'abc', node_id: 'src/a.rs#1:f'}) RETURN s
```

Look for `NodeIndexSeek` in the plan where `NodeByLabelScan` is today, and compare `db hits`. That is
the proof, not the wall-clock number.

**Backfill:** none. Neo4j populates a new index over existing data on creation; it is online and
idempotent.

---

### 4.2 — Page the graph submit

**Step 1 — split the payload at the client.**
`services/agent-clients/src/control_plane/indexing.rs`:

```rust
/// Nodes or edges per `submit_graph` request. Bounds one request's duration by a constant we
/// choose rather than by repo size, which we do not — a whole-graph submit scales with the
/// repository and eventually outlives `CONTROL_PLANE_REQUEST_TIMEOUT_SECS`.
/// `GRAPH_SUBMIT_PAGE_SIZE` (default 2,000).
pub const DEFAULT_GRAPH_PAGE_SIZE: usize = 2_000;
```

```rust
/// Submit a structural graph, paged. **Every node page is sent before any edge page**: an edge
/// `MATCH`es both endpoints, and a `MATCH` that finds nothing is a silent no-op, so an edge landing
/// ahead of its nodes is dropped without error.
pub async fn submit_graph_paged(
    &self,
    task_id: Uuid,
    commit_sha: &str,
    nodes: &[GraphNodePayload],
    edges: &[GraphEdgePayload],
    page_size: usize,
) -> anyhow::Result<()> {
    let page_size = page_size.max(1);

    for (i, page) in nodes.chunks(page_size).enumerate() {
        self.submit_graph(task_id, GraphBatch {
            commit_sha: commit_sha.to_string(),
            nodes: page.to_vec(),
            edges: Vec::new(),
        })
        .await
        .with_context(|| format!("submitting graph node page {i}"))?;
    }

    for (i, page) in edges.chunks(page_size).enumerate() {
        self.submit_graph(task_id, GraphBatch {
            commit_sha: commit_sha.to_string(),
            nodes: Vec::new(),
            edges: page.to_vec(),
        })
        .await
        .with_context(|| format!("submitting graph edge page {i}"))?;
    }
    Ok(())
}
```

**Step 2 — the handler must accept an edges-only page.** This is the one real behaviour change on the
server, and it is easy to miss:

```rust
if batch.nodes.is_empty() {
    return StatusCode::NO_CONTENT.into_response();   // ← drops every edges-only page today
}
```

That early return must become `if batch.nodes.is_empty() && batch.edges.is_empty()`. Without it,
paging appears to work and silently indexes **zero edges** — the worst possible failure, since the
graph looks populated.

**Step 3 — `upsert_graph` already handles it.** Its two blocks are independently guarded
(`if !nodes.is_empty()`, `if !edges.is_empty()`), so a nodes-only or edges-only batch is already
valid. No change.

**Step 4 — call site**, `services/agent-runner/src/indexer/graph.rs`, replacing the single
`submit_graph`.

**Step 5 — forward `GRAPH_SUBMIT_PAGE_SIZE`** in `k8s.rs` alongside the existing index knobs.

**Tests**

| test | asserts |
|---|---|
| unit | node pages all precede edge pages, and page count is `ceil(n / size)` for each |
| unit | a graph smaller than one page still sends exactly one request per non-empty kind |
| wire-contract | an edges-only body is well-formed and accepted (`nodes: []`) |
| live Neo4j | paged submit of a graph spanning ≥3 pages produces the same node/edge counts as one unpaged submit — the regression guard for the dropped-edge failure |

That last one is the load-bearing test. Everything else is arithmetic.

---

### 4.3 — Raise `CONTROL_PLANE_REQUEST_TIMEOUT_SECS`? No

Tempting, and wrong as a fix: it moves the wall without changing the slope, and it makes every *other*
control-plane call wait longer to fail. With 4.1 and 4.2 in place the value stops mattering. Leave it.

---

## 5. What #651 is, and what has changed since it was written

### The claim

Index jobs on large repositories run until `activeDeadlineSeconds` (3600) expires, get requeued by the
reaper, and repeat until `MAX_ATTEMPTS` is exhausted. The cause is **strictly sequential HTTP round
trips** — every embed and every submit is a `for … await` with no concurrency — multiplied by the
deployed `INDEX_EMBED_BATCH_SIZE=5`, which is 6.4× the code default of 32.

Its three aggravating factors:

1. **No resumption** — every retry restarts at chunk 0, so five attempts re-embed the same corpus five
   times.
2. **O(n·m) node↔chunk correlation** — 157,954,980 comparisons on cratestack.
3. **The chunker ignored `.gitignore` and double-emitted** container + child chunks, inflating the
   count everything else multiplies.

### What #655 already settled

| #651 item | status |
|---|---|
| (2) O(n·m) correlation | **gone** — one walk, `node_id` linkage, zero comparisons |
| (3) `.gitignore` + double-emission | **gone** — `lci-codegraph` honours `.gitignore` and an operator glob layer |
| duplicate embed pass (#652) | **gone** — one chunker means one embed per text |
| (1) no resumption | **still open** |
| sequential round trips | **still open** |

### The number #651 said was missing — now measured

> *"the actual per-call embedding latency was not measured … That single number decides whether
> reducing round trips is sufficient on its own."*

The CoopData run supplies it. Chunks ran `12:27:32 → 12:57:55`:

```
1,823 s  for 11,284 chunks  =  2,257 batches at INDEX_EMBED_BATCH_SIZE=5
                            →  0.808 s per batch (embed + submit)
```

Extrapolating to cratestack's 17,097 chunks:

```
3,420 batches × 0.808 s ≈ 2,763 s  ≈ 46 min      against a 60 min cap
```

**So cratestack now looks feasible rather than hopeless** — but with ~14 minutes of headroom, which a
slow gateway hour would erase. This revises my earlier "probably not" upward, on one repo's data.

### The correction that matters for #651's plan

#651 and I both assumed **raising the batch size is the big lever**. The measurement says otherwise.

If a batch of 5 takes 0.808 s and the submit half is ~0.046 s, the embed call is ~0.76 s for 5 texts.
Embedding is dominated by **per-text GPU work**, not per-request overhead, so batching does not divide
the cost — it only amortises the fixed part:

```
batch  5 :  3,420 iterations × 0.81 s  ≈  2,763 s
batch 32 :    535 iterations × ~3.8 s  ≈  2,049 s      (~26% saved, not 6×)
```

A single data point cannot decompose fixed overhead from per-text cost. **Measure `L` at two batch
sizes** (5 and 32 on the same repo) and solve `L(b) = overhead + b × per_text` before tuning
anything. If `overhead` turns out small, batching is nearly worthless and **bounded concurrency is the
only real lever** — with the caveat that a GPU-bound gateway may just queue parallel requests, in
which case neither helps and the answer is a faster or scaled-out embedder.

That is the honest state: #651's diagnosis was right about the shape, and its proposed remedy needs
re-grounding on the data that now exists.

---

## 6. Recommended sequence

Ordered by value per unit of risk.

| # | change | effort | risk | why here |
|---|---|---|---|---|
| 1 | **Neo4j `symbol_identity` constraint** | one line | low | Fixes the *cause* of the 180 s timeout (145,029 → 3 db hits per lookup) and speeds every symbol read. No migration, no backfill, online — but fails if duplicate triples already exist, so check first. |
| 2 | **Page `submit_graph`** | small | low | Bounds request duration by a constant. Must ship with the `nodes.is_empty()` handler fix or it silently drops all edges. |
| 3 | **Measure `L(5)` and `L(32)`** | minutes | none | Decides whether (4) is worth anything. Do not tune before this. |
| 4 | **Batch size and/or bounded concurrency** | medium | medium | Rate-limit pressure; `EmbeddingsClient` retries 429 up to 8 s and treats the gateway budget as shared. Bounded (≈8), never unbounded. |
| 5 | **Resume from stored chunk keys** | medium | low | Turns a timeout from "5 wasted hours" into "finishes on attempt 2". `upsert_code_chunks` is already idempotent on `(repository_id, commit_sha, file_path, start_line, end_line)`, so this is a read-and-skip, not new storage. |

Items 1 and 2 belong together in one PR — they are two halves of "the graph submit must survive a
large repo", and shipping 2 without 1 leaves each page slower than it needs to be.

Items 3–5 are #651's remaining scope.

### One thing worth fixing regardless

A graph submit that fails leaves the repo with a semantic index and no structural one, while the task
reports `succeeded`. Best-effort is the right *policy*, but it should be **visible**: a counter, or
the summary saying `graph skipped (timeout)` rather than the current bare `graph skipped`, so an
operator can tell a repo missing its graph from one that never had symbols.

---

## 7. What I could not verify

- **The live Neo4j index list.** `kubectl exec` into `lightbridge-ci-neo4j-0` is blocked for this
  account (`cannot create resource "pods/exec"`). The claim that `symbol_identity_idx` is absent rests
  on the source — `ensure_indexes` is the only code that creates Neo4j indexes, and it creates two.
  If someone added one by hand, that changes the diagnosis. The db-hit numbers above come from a
  **local** `neo4j:5` Community 5.26.29 instance seeded to a realistic size, not from prod. Confirm
  prod's state with:

  ```cypher
  SHOW INDEXES YIELD name, type, labelsOrTypes, properties
  ```

- **The decomposition of embedding latency.** One batch size measured, two needed.

- **Whether CoopData's graph write eventually succeeded** after the client gave up. Neo4j may hold the
  data even though the runner recorded `graph skipped`. Worth a `MATCH (s:Symbol {repo_id: …})
  RETURN count(s)` before assuming the repo has no graph at all.
