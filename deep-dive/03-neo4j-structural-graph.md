# 3. Neo4j: the structural code graph

Everything in this chapter lives in one file: `services/control-plane/src/integrations/neo4j.rs`
(1535 lines including tests). It is the control plane's only Cypher — the only place in the whole
backend that talks Bolt to Neo4j — and this chapter quotes most of its query text verbatim, because
with Cypher there's no substitute for reading the actual pattern.

## 3.1 Why the control plane, and not the agent, holds this connection

The module's own doc comment states the architecture in two sentences
(`services/control-plane/src/integrations/neo4j.rs:1-7`):

> "The agent runner builds the structural graph in-process with `lci-codegraph` (symbols +
> `contains`/`method`/`calls` edges) and POSTs nodes+edges to the internal API; this module writes
> them to Neo4j. The control plane owns the Neo4j credentials so the untrusted per-task Job never
> holds them (trust boundary, ADR-0002) — the same reason chunk ingestion routes through the control
> plane rather than direct DB access."

So the shape is: the agent runner (a separate, untrusted-by-design Kubernetes Job) parses a repo
checkout with the `lci-codegraph` crate, extracts symbols and edges, and `POST`s that payload to
`/internal/tasks/{id}/graph` — a route served by the `serve` role, authenticated with a per-task
runner token, not a Neo4j credential. Only the control plane process itself ever opens a Bolt
connection. This is the exact same pattern file `02` already covered: anything that could mutate a
durable store is mediated through the control plane, never handed directly to the Job that produced
the data.

## 3.2 Connecting, and the node/edge shape

`connect_from_env` (`neo4j.rs:41-54`) is the one place `NEO4J_URI`/`NEO4J_USER`/`NEO4J_PASSWORD` are
read; an unset `NEO4J_URI` returns `Ok(None)` rather than an error, and every route that needs the
graph fails closed with a 503 in that case (file `01`'s `AppState.neo4j: Option<Arc<neo4rs::Graph>>`).

Two plain Rust structs describe what the runner submits (`neo4j.rs:16-37`):

```rust
/// One graph node submitted by the runner (from `lci-codegraph`).
pub struct GraphNode {
    pub node_id: String,
    pub label: String,
    pub source_file: String,
    pub start_line: i64,
    pub embedding: Option<Vec<f32>>,
}

/// One directed edge (`contains` / `method` / `calls` / …).
pub struct GraphEdge {
    pub source: String,
    pub target: String,
    pub relation: String,
}
```

Note `GraphNode::embedding` is `Option` — most submissions leave it `None` (file `04` explains why:
a symbol's vector arrives on the *chunk* that is its body, not on the structural submit itself). It's
only there as a backward-compatible path for a pre-ADR-0116 runner.

## 3.3 The upsert — one `:Symbol` label, one `:REL` relationship type for everything

Cypher can't parameterize a node label or a relationship type — `MERGE (s:Symbol {...})` has to be
written with the literal label `Symbol` in the query text, you can't bind it to a parameter the way
you bind property values. So rather than one Neo4j label per symbol kind (`:Function`, `:Class`,
...), this module uses a single generic `:Symbol` label with a `label` *property* carrying the kind,
and a single generic `:REL` relationship type with a `relation` property carrying `contains` /
`method` / `calls` / etc. That design choice is what makes the upsert a single prepared statement
regardless of how many distinct symbol/edge kinds exist.

The node upsert, from `upsert_graph` (`neo4j.rs:112-124`):

```cypher
UNWIND $nodes AS n
MERGE (s:Symbol {repo_id: $repo, commit: $commit, node_id: n.id})
SET s.label = n.label, s.source_file = n.file, s.start_line = n.line,
    s.embedding = CASE WHEN size(n.embedding) > 0 THEN n.embedding ELSE s.embedding END
```

Read the `MERGE` key carefully: `{repo_id, commit, node_id}` — a symbol's *identity* in this graph is
the triple of which repository, which exact commit snapshot, and its own stable node id (a path+span
derived identifier `lci-codegraph` computes). Two different commits of the same repo produce two
completely separate sets of `:Symbol` nodes that happen to share a `repo_id` — there is no single
"the `foo()` function" node that gets updated across commits; re-indexing a new commit creates a
parallel snapshot rather than mutating the old one. That's deliberate: it's what lets an old snapshot
and a new one coexist long enough for in-flight reviews to keep reading the old one while a new index
lands.

The `embedding` `SET` clause is doing something specific worth unpacking: `n.embedding` arrives as an
*empty list* when the node has none (never `null` — Cypher's own type system reasons), and `CASE WHEN
size(n.embedding) > 0 THEN n.embedding ELSE s.embedding END` means "only overwrite if the incoming
value is non-empty; otherwise keep whatever this node already had." This is what makes a
structure-only re-index (no fresh embeddings this time) non-destructive to vectors a *previous*
submit already attached — covered more in §3.4.

The edge upsert, same function (`neo4j.rs:140-144`):

```cypher
UNWIND $edges AS e
MATCH (a:Symbol {repo_id: $repo, commit: $commit, node_id: e.src})
MATCH (b:Symbol {repo_id: $repo, commit: $commit, node_id: e.dst})
MERGE (a)-[r:REL {relation: e.rel}]->(b)
```

`MATCH`, not `MERGE`, on the endpoints — an edge whose source or target node doesn't already exist in
this snapshot simply matches nothing and silently writes no edge. That's a real failure mode the
tests exercise directly (§3.3.1 below): if a page of edges arrives before the page of nodes it
depends on, those edges vanish with no error.

Both statements are `UNWIND`-driven — one Bolt round trip per *batch*, not per element — "so the
write stays fast as the structural graph grows into the thousands of nodes/edges" (`neo4j.rs:62-63`).

### 3.3.1 The snapshot write lock — why a bare `MERGE` isn't enough

Before either upsert runs, `upsert_graph` takes a lock (`neo4j.rs:74-91`):

```cypher
MERGE (l:SnapshotWriteLock {repo_id: $repo, commit: $commit}) DELETE l
```

The comment above it explains the race this closes: "`MERGE` only guarantees a single node per key
when a uniqueness constraint backs it; without one, two transactions writing the same snapshot can
both find a symbol absent and both create it." Merging the lock node's key acquires its backing
unique-index entry — Neo4j holds that entry exclusively until the transaction ends — so a second
concurrent writer to the *same* `(repo_id, commit)` snapshot blocks here until the first transaction
commits, then proceeds against the already-written nodes instead of racing to create duplicates.
Deleting the node in the same statement means nothing is left behind once the lock is released; the
constraint that backs it is declared separately, idempotently, by `ensure_snapshot_write_lock`
(`neo4j.rs:422-435`), and every write retries that declaration if it hasn't succeeded yet — so a
startup-time failure to declare the constraint doesn't leave writes permanently unserialized.

A live test proves this directly (`neo4j.rs:1077-1166`,
`concurrent_writes_to_one_snapshot_create_each_symbol_once`): with the identity constraint
deliberately dropped so *only* the snapshot lock stands between four concurrent writers and a
duplicate, firing four `upsert_graph` calls at once for 1,500 identical nodes still produces exactly
1,500 symbol nodes, not more.

## 3.4 Attaching embeddings to existing symbols (ADR-0116)

`attach_symbol_embeddings` (`neo4j.rs:164-203`) is a separate, later write that links a chunk's vector
to the `:Symbol` node it's the body of:

```cypher
UNWIND $rows AS r
MATCH (s:Symbol {repo_id: $repo, commit: $commit, node_id: r.id})
SET s.embedding = r.embedding
RETURN count(s) AS updated
```

`MATCH`, not `MERGE`, again — deliberately, per the doc comment: "a vector whose symbol is not in the
graph — because the structural submit failed, or the snapshot predates this commit — is dropped
rather than creating a `:Symbol` carrying an embedding and nothing else." A live test
(`attach_symbol_embeddings_writes_to_matching_symbols_only`, `neo4j.rs:1384-1472`) proves three things
in sequence: a structure-only submit leaves a symbol with no embedding; attaching a vector for a
node id that doesn't exist updates nothing and creates nothing (`MATCH` found zero rows); and a
later structure-only re-upsert of the *same* symbol (going back through §3.3's `CASE WHEN
size(...)>0` guard) leaves the already-attached embedding untouched. That third assertion is the
direct payoff of the `CASE` expression in the node upsert — the two write paths were designed
together to not stomp on each other.

## 3.5 Reading the graph

### `find_symbol` — substring search, scoped to one snapshot

```cypher
MATCH (s:Symbol {repo_id: $repo, commit: $commit})
WHERE toLower(s.label) CONTAINS toLower($term)
   OR toLower(s.node_id) CONTAINS toLower($term)
   OR toLower(s.source_file) CONTAINS toLower($term)
RETURN s.node_id AS node_id, s.label AS label, s.source_file AS source_file,
       s.start_line AS start_line
LIMIT $limit
```
(`neo4j.rs:350-362`)

### `get_callers` — reverse `calls` traversal

```cypher
MATCH (caller:Symbol {repo_id: $repo, commit: $commit})
      -[:REL {relation: 'calls'}]->
      (target:Symbol {repo_id: $repo, commit: $commit, node_id: $id})
RETURN caller.node_id AS node_id, caller.label AS label,
       caller.source_file AS source_file, caller.start_line AS start_line
LIMIT $limit
```
(`neo4j.rs:387-397`)

Note the direction: the pattern walks *from* `caller` *to* `target` along a `calls` edge, with
`target` pinned to the requested `node_id` — this answers "who calls this symbol," the reverse of how
the edge was originally written (`a calls b` means `a` is the caller, `b` the callee; finding a
symbol's callers means finding every `a` with an outgoing `calls` edge *into* it).

### Every query in this module is scoped by `(repo_id, commit)`

This is worth stating as its own invariant, because it's what makes multi-tenancy and snapshot
isolation hold at the query level rather than relying on some outer access-control layer: every
single Cypher statement in this file that touches `:Symbol` nodes includes `repo_id: $repo, commit:
$commit` (or `{repo_id: $repo}` alone for a whole-repository delete) directly in the node pattern.
There is no query that reads "all symbols" unscoped. A caller — ultimately the internal API handling
a request for one specific task — supplies the scope; it is never left to the query to guess or the
caller to omit.

## 3.6 The indexes — vector, fulltext, and the identity constraint

`ensure_indexes` (`neo4j.rs:445-513`) is called once at startup (idempotent, safe to call on every
boot "mirroring how Postgres migrations already run unconditionally on connect"). Four DDL
statements, each via `create_index_idempotent` (`neo4j.rs:520-530`), which treats Neo4j's "this
schema object already exists" error as success rather than a failure — necessary because, unlike
Postgres's `sqlx::migrate!`, there's no single process guaranteed to run this exactly once; several
roles can open a Neo4j connection at startup and race each other to declare the same index.

**The vector index**, verbatim (`neo4j.rs:453-465`):

```cypher
CREATE VECTOR INDEX symbol_embedding_idx IF NOT EXISTS
FOR (s:Symbol) ON s.embedding
OPTIONS { indexConfig: {
  `vector.dimensions`: {dimension},
  `vector.similarity_function`: 'cosine'
}}
```

`{dimension}` is interpolated directly into the query string, not bound as a parameter — Cypher
schema DDL simply can't parameterize index options, the same limitation that forces the generic
`:Symbol`/`:REL` labeling in §3.3. The comment at `neo4j.rs:444` notes this is safe specifically
*because* the value "comes from trusted server-side config, never request input" — the dimension is
resolved once at startup from the embeddings config (default 4096, matching `qwen3-embedding-8b` —
see file `04`), not from anything a caller supplies per-request.

**The fulltext index** (`neo4j.rs:467-471`):

```cypher
CREATE FULLTEXT INDEX symbol_label_fulltext IF NOT EXISTS
FOR (s:Symbol) ON EACH [s.label, s.source_file]
```

**The snapshot index** (`neo4j.rs:478-481`), a plain composite index on `(repo_id, commit)` — backs
every snapshot-level read/delete that doesn't name a specific node.

**The identity constraint** (`neo4j.rs:499-512`):

```cypher
CREATE CONSTRAINT symbol_identity IF NOT EXISTS
FOR (s:Symbol) REQUIRE (s.repo_id, s.commit, s.node_id) IS UNIQUE
```

This is a `UNIQUE` **constraint**, not a bare index, chosen deliberately (per the comment) for three
reasons: it creates its own backing range index, it lets `MERGE` plan a unique-index seek instead of
a scan, and — most important semantically — the triple genuinely *is* unique: "a second node sharing
it would be a duplicate symbol." If this constraint fails to create (e.g. duplicate triples already
exist for some other reason), the function logs a warning and continues rather than propagating the
error — the comment explains the tradeoff: by that point in the function, the other three indexes are
already in place, and "an absent identity index costs write latency, not correctness" (because the
snapshot write lock in §3.3.1 already prevents the actual duplication; this constraint is a
performance seek-path, not the only thing standing between the data and a duplicate).

## 3.7 Hybrid search: the weighted reciprocal-rank-fusion query

This is the single most interesting query in the file, and ADR-0114's reason for existing. The module
comment introduces it precisely (`neo4j.rs:532-539`): "Hybrid symbol search: lexical (fulltext on
`label`/`source_file`) + semantic (vector on `embedding`), fused by weighted reciprocal rank (WRRF) —
Neo4j's documented hybrid-search pattern, not an ad hoc union."

**The problem it solves**: a fulltext search for "retry logic" finds symbols whose *name* or *file
path* literally contains those words; a vector search against an embedding of the same query finds
symbols that are *semantically* close even if they're named `backoff_loop` or `attempt_again`. Either
alone misses things the other would catch. Taking the union naively would just concatenate two
differently-scaled ranking signals with no principled way to interleave them. **Reciprocal rank
fusion** sidesteps the scaling problem entirely by discarding each branch's raw score and using only
its *rank* (1st place, 2nd place, ...) — a rank is always comparable across branches even when the
underlying scores (Lucene relevance vs. cosine similarity) live on totally different scales.

The full query, verbatim (`neo4j.rs:558-577`):

```cypher
CALL () {
  CALL db.index.fulltext.queryNodes('symbol_label_fulltext', $query, {limit: $sourceK})
  YIELD node AS s
  WHERE s.repo_id = $repo AND s.commit = $commit
  WITH collect(s) AS hits
  UNWIND CASE WHEN size(hits) = 0 THEN [] ELSE range(0, size(hits) - 1) END AS rankIndex
  RETURN hits[rankIndex] AS s, rankIndex + 1 AS sourceRank
  UNION ALL
  CALL db.index.vector.queryNodes('symbol_embedding_idx', $sourceK, $queryVector)
  YIELD node AS s
  WHERE s.repo_id = $repo AND s.commit = $commit
  WITH collect(s) AS hits
  UNWIND CASE WHEN size(hits) = 0 THEN [] ELSE range(0, size(hits) - 1) END AS rankIndex
  RETURN hits[rankIndex] AS s, rankIndex + 1 AS sourceRank
}
WITH s, sum(1.0 / ($rrfConstant + sourceRank)) AS wrrf
ORDER BY wrrf DESC
LIMIT $finalK
RETURN s.node_id AS node_id, s.label AS label, s.source_file AS source_file,
       s.start_line AS start_line
```

Read it in three passes:

1. **Each branch independently produces a ranked list.** `db.index.fulltext.queryNodes(...)` and
   `db.index.vector.queryNodes(...)` are Neo4j's built-in index procedures — the first runs a Lucene
   query against the fulltext index, the second a nearest-neighbor search against the vector index.
   Each `YIELD`s matching nodes already in the index's own best-first order. The `WHERE s.repo_id = …
   AND s.commit = …` filter is the same per-snapshot scoping every other query in this file applies —
   note it runs *after* the index procedure call, as a plain filter, because neither
   `queryNodes` procedure accepts a scope parameter itself.
2. **Rank, not score, is what gets carried forward.** `collect(s)` materializes each branch's filtered
   hit list in order, then `UNWIND range(0, size(hits)-1)` turns "the Nth element of this list" into
   an explicit `rankIndex` (0-based), reported as `sourceRank = rankIndex + 1` (1-based rank — "1st
   place," "2nd place," ...). The `CASE WHEN size(hits) = 0 THEN [] ...` guard matters mechanically:
   `range(0, -1)` in Cypher does *not* produce an empty list the way you might expect from a
   zero-length range — so an empty branch is special-cased to unwind over a literal empty list instead
   of whatever `range(0, -1)` would actually yield.
3. **Fusion**: `UNION ALL` concatenates both branches' `(s, sourceRank)` rows (a symbol that matched
   both branches appears twice, once per branch, each with its own rank in that branch); the outer
   `WITH s, sum(1.0 / ($rrfConstant + sourceRank)) AS wrrf` then groups by symbol and sums
   `1/(constant + rank)` across however many branches that symbol appeared in. `RRF_CONSTANT = 60.0`
   (`neo4j.rs:553`) is called out as "Neo4j's documented default" — it's the smoothing term that keeps
   a rank-1 hit from dominating with a near-infinite score (`1/(60+1)` vs. a hypothetical `1/1`) and
   controls how steeply the fused score falls off between adjacent ranks. A symbol that ranked well in
   *both* branches accumulates a higher `wrrf` than one that ranked well in only one — which is exactly
   the "structural agreement between two independent signals is stronger evidence than either alone"
   intuition RRF formalizes.

`CALL () { ... }` (the outer subquery call with an empty, explicit variable-scope clause) is Neo4j's
newer subquery syntax; the function's own doc comment flags the deliberate API choice at
`neo4j.rs:541-542`: "Uses the `db.index.vector.queryNodes()` procedure form rather than the newer
`SEARCH ... IN (VECTOR INDEX...)` clause syntax" — worth knowing if you go looking for this pattern in
Neo4j's current docs and find a newer-looking clause form; this codebase deliberately didn't adopt it
(yet).

### Escaping free text before it reaches Lucene

One more detail that would silently break the query above in production: `$query` is a natural
language string (something like "how does retry logic with backoff work"), but
`db.index.fulltext.queryNodes` parses it as a **Lucene** query, where characters like `(`, `)`, `:`,
and `"` are syntactically meaningful. `escape_lucene_query` (`neo4j.rs:604-617`) is called on the raw
text before it's bound as the `$query` parameter:

```rust
fn escape_lucene_query(text: &str) -> String {
    const SPECIAL: &[char] = &[
        '\\', '+', '-', '!', '(', ')', ':', '^', '[', ']', '"', '{', '}', '~', '*', '?', '|', '&',
        '/',
    ];
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if SPECIAL.contains(&c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}
```

Its own unit test (`neo4j.rs:800-814`) makes the failure mode concrete: `"retry logic (with
backoff)"` becomes `` retry logic \(with backoff\) `` — without the escape, the unbalanced-looking
parenthesis structure "throws a Lucene parse error that fails the entire call, not just the fulltext
branch" (doc comment, `neo4j.rs:599-603`). A natural-language query containing a colon ("how does
auth: refresh work?") or a quote mark would fail the same way unescaped.

## 3.8 Neighborhood and overview reads — the admin console's graph tab

Two more read queries worth knowing, both used to render the admin console's interactive graph view:

- **`graph_neighborhood`** (`neo4j.rs:668-709`) — a seeded BFS: one symbol plus everything within
  `hops` steps (clamped `1..=3`), via `OPTIONAL MATCH (seed)-[:REL*1..{hops}]-(other:Symbol {...})`.
  `hops` is interpolated into the query text, not bound — the module comment explains why this one
  specifically can't be a parameter: "Neo4j's `*min..max` relationship range can't be parameterized,"
  and clamping it server-side to a small integer before interpolation keeps it safe (never request
  text, always a small bounded int).
- **`graph_overview`** (`neo4j.rs:716-749`) — an *unseeded* slice: the `limit` highest-degree symbols
  in a snapshot, used "when the frontend has no node selected yet (first load of the graph tab), so
  there's always something to render instead of an empty canvas."

Both delegate their edge-fetching to a shared helper, **`edges_among`** (`neo4j.rs:755-789`), which
finds only the `:REL` edges whose *both* endpoints are already in a given node set — "so a search
result set that happens to be structurally connected shows that connection instead of rendering as
isolated dots." This is the same function hybrid-search results could be passed through to show
structural relationships among semantic hits, though the search function itself (§3.7) returns bare
`SymbolHit`s and leaves that composition to its caller.

## 3.9 Deleting and pruning a snapshot

Deletes share one helper, `delete_symbols_in_batches` (`neo4j.rs:218-247`), built around Neo4j's
`CALL ... IN TRANSACTIONS` construct:

```cypher
{selector} CALL (s) { DETACH DELETE s } IN TRANSACTIONS OF 1000 ROWS
```

The reason this matters, stated directly in the comment above `DELETE_BATCH_ROWS`
(`neo4j.rs:205-210`): "Neo4j holds a transaction's whole state on the heap until it commits, so a
delete sized by its input grows with the snapshot it removes." Batching in fixed-size chunks of 1,000
rows, each its own auto-committing transaction, keeps memory bounded regardless of how large a
repository's snapshot is — and, as a direct consequence, means a delete that fails partway through
leaves the already-committed batches deleted rather than rolling the whole thing back. A live test
(`a_delete_commits_each_batch_on_its_own`, `neo4j.rs:1009-1055`) constructs exactly this scenario — a
selector engineered to divide-by-zero on its very last matched row — and confirms the two full
batches before the failure stay committed.

Three callers build on this: `delete_commit_graph` (one snapshot), `delete_repo_graph` (every
snapshot of a repo, used when a repo is removed/denied), and `prune_graph` (every snapshot *except*
the ones in a `keep` list, the Neo4j half of ADR-0052's index-snapshot pruning — see file `04` for
its pgvector counterpart). `prune_graph` explicitly no-ops on an empty `keep` list rather than
treating "nothing to keep" as "delete everything" — "never wipe the whole graph here."

## 3.10 What to read next

- `04-pgvector-and-dual-retrieval.md` — the semantic half of ADR-0003's "dual retrieval": why a vector
  store exists *alongside* this graph rather than instead of it, and the exact SQL behind
  `search_code_chunks`.
- `05-glossary.md` — snapshot, RRF, hybrid search, upsert, and the rest of this chapter's vocabulary.
