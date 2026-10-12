# PRs #657 and #658 — making an index run survive its own failures

> [#657](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/pull/657) · closes
> [#656](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/656) ·
> `fix/graph-submit-paging-and-identity-index`
>
> [#658](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/pull/658) · closes
> [#651](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/651) ·
> `fix/index-resume-and-transient-body`
>
> Both branched from `adorsys/main` at `f3bd0df`. Independent — they touch `indexing.rs` and
> `internal.rs` in different functions and can merge in either order.

---

## Why two PRs

Indexing writes two stores. Each had its own way of losing work, and they fail differently enough
that bundling them would have made both harder to review.

| | #657 — the graph half | #658 — the chunk half |
|---|---|---|
| what fails | one `submit_graph` request times out | one embeddings request drops |
| how it presents | task says `succeeded`, repo has **no** structural index | task fails, retry re-embeds everything |
| observed on | CoopData, then cratestack | latent — nothing has triggered it since #655 |
| root causes | no index on the identity triple; one unbounded request | no checkpoint; a truncated body classed non-retryable |

---

# PR #657 — index symbol identity, and submit the graph in pages

## The failure

```text
12:24:32  INFO  codegraph: index complete  graph_nodes=5154  graph_edges=7893
12:27:32  WARN  structural graph indexing failed (non-fatal)
                error="submitting codegraph structural graph: submitting graph:
                       error sending request for url (…/graph): operation timed out"
12:57:55  INFO  task succeeded  summary="… 11284 chunks, graph skipped; review disabled"
```

`12:24:32 → 12:27:32` is exactly `DEFAULT_REQUEST_TIMEOUT_SECS` (180), set in neither `ai-helm` nor
`ai-helm-values`. Not a 413 — prod `/metrics` shows no non-2xx on `/graph`. Not Neo4j being
unconfigured — `NEO4J_URI` is set. It reached the database and was too slow. cratestack then did the
same thing.

The task reported `succeeded` both times. That is the design (the graph is best-effort) but the
consequence is a repository with a semantic index and **no** structural one, where
`graph_find_symbol`, `get_callers` and `graph_semantic_search` all return nothing — and the review
agent's prompt tells it to read an empty result as *"could not verify"*, not *"absent"*.

## Change 1 — the identity index

`ensure_indexes` created a vector index and a fulltext index. Nothing indexed the triple that every
read and write addresses a node by:

```cypher
MERGE (s:Symbol {repo_id: $repo, commit: $commit, node_id: n.id})
```

```cypher
MATCH (a:Symbol {repo_id: $repo, commit: $commit, node_id: e.src})
MATCH (b:Symbol {repo_id: $repo, commit: $commit, node_id: e.dst})
```

Without an index, each of those scans **every** `:Symbol` in the database — all repositories, all
retained commit snapshots. Measured on `neo4j:5` Community 5.26.29 seeded with 60,018 symbols:

```text
PROFILE MATCH (s:Symbol {repo_id: 2, commit: 'demo-commit', node_id: 'src/f19999.rs#…'}) RETURN s.label

  without :  DbHits = 145,029    NodeByLabelScan
  with    :  DbHits =       3    NodeUniqueIndexSeek
```

CoopData performs 5,154 node `MERGE`s + 7,893 edges × 2 `MATCH` = **20,940 such lookups per submit**.
Cost per submit is roughly `O(nodes_this_repo × symbols_everywhere)` — it degrades as the platform
indexes more repositories, which is the shape of "it used to work and now it doesn't".

```rust
// Every symbol read and write addresses a node by its full identity triple — the graph upsert's
// MERGE, both endpoint MATCHes on each edge, `find_symbol`, `get_callers`, `symbol_embedding`,
// `prune_graph`, and the chunk-side embedding attach. A composite index applies when a query
// supplies all three with equality, which all of them do; without one each lookup scans every
// `:Symbol` in the database, so one repository's write cost grows with every other repository
// indexed.
//
// A uniqueness constraint rather than a bare index: it creates its own backing range index, it
// lets MERGE plan a unique-index seek, and the triple genuinely is unique — a second node
// sharing it would be a duplicate symbol. (`IS NODE KEY` additionally requires all three to be
// present, and is Enterprise-only.)
if let Err(error) = create_index_idempotent(
    graph,
    "CREATE CONSTRAINT symbol_identity IF NOT EXISTS \
     FOR (s:Symbol) REQUIRE (s.repo_id, s.commit, s.node_id) IS UNIQUE",
    "symbol_identity",
)
.await
{
    tracing::warn!(?error, "symbol identity constraint not created; symbol lookups will scan the label");
}
```

**Two placement decisions worth noticing.** It is declared *after* the vector and fulltext indexes,
and its failure is warned rather than propagated. A constraint — unlike an index — is rejected
outright if duplicate triples already exist, and `ensure_indexes` short-circuits on `?`. Declaring it
first would have meant a database holding duplicates loses the other two indexes as well.

**On the constraint vs a plain index.** A constraint brings its own backing range index *and*
enforces the invariant `MERGE` already assumes:

```text
name              type     properties                          owningConstraint
symbol_identity   RANGE    ["repo_id","commit","node_id"]      symbol_identity
```

`IS NODE KEY` — uniqueness plus a not-null guarantee — is Enterprise-only and not wanted anyway: it
would reject any `:Symbol` missing one of the three, stricter than the code promises. Composite
`IS UNIQUE` works on Community, tested directly.

## Change 2 — paging the submit

The submit travelled as one request and one Neo4j transaction, so duration scaled with repository
size against a fixed client timeout.

```rust
/// Submit a structural graph as a sequence of bounded pages.
///
/// Every node page is sent before any edge page: an edge is written by matching both of its
/// endpoints, and a match that finds nothing is a silent no-op, so an edge that arrives ahead of
/// its endpoints is dropped without error.
pub async fn submit_graph_paged(
    &self,
    task_id: Uuid,
    commit_sha: &str,
    nodes: &[GraphNodePayload],
    edges: &[GraphEdgePayload],
    page_size: usize,
) -> anyhow::Result<()> {
    let page_size = page_size.max(1);

    for (page, chunk) in nodes.chunks(page_size).enumerate() {
        self.submit_graph(task_id, GraphBatch {
            commit_sha: commit_sha.to_string(),
            nodes: chunk.to_vec(),
            edges: Vec::new(),
        })
        .await
        .with_context(|| format!("submitting graph node page {page}"))?;
    }

    for (page, chunk) in edges.chunks(page_size).enumerate() { /* … edges … */ }
    Ok(())
}
```

**The trap this creates on the server.** `ingest_graph` returned early on `nodes.is_empty()`. Paging
sends edge-only pages, so without this change every edge would be silently dropped while the graph
looked populated:

```rust
// A batch carries nodes, edges, or both: the runner pages a large graph into node-only requests
// followed by edge-only ones, so an edge-only batch is a normal shape, not an empty submit.
if batch.nodes.is_empty() && batch.edges.is_empty() {
    return StatusCode::NO_CONTENT.into_response();
}
```

`upsert_graph` needed no change — its two blocks are already independently guarded
(`if !nodes.is_empty()`, `if !edges.is_empty()`).

## Change 3 — discarding a partial graph *(added in review)*

Paging introduced a regression I did not see, and @stephane-segning did. `upsert_graph` opens and
commits a transaction **per call**, so per page:

```rust
let mut txn = graph.start_txn().await?;   // line 71
…
txn.commit().await?;                      // line 133
```

Before this PR a failed submit meant *zero* graph — clean, self-evidently absent. With paging, a
failure at page k leaves pages `0..k-1` permanently committed and pages `k..N` never attempted. If
that happens mid-edges, the graph has every node and some edges, and `get_callers` will confidently
answer "no callers" for a function whose caller's edge simply never arrived. **A partial graph is
worse than no graph, because nothing marks it partial.**

The fix restores all-or-nothing at the task level:

```rust
if let Err(error) = client
    .submit_graph_paged(context.task_id, &commit_sha, &nodes, &edges, page_size)
    .await
{
    // Pages commit individually, so a sequence that stops partway leaves the ones that already
    // landed. Discarding the snapshot leaves the commit un-indexed, which readers handle, rather
    // than a subset that reads as a complete graph — a missing edge is indistinguishable from a
    // symbol that genuinely has no callers.
    if let Err(discard) = client.discard_graph(context.task_id).await {
        tracing::warn!(
            error = %format!("{discard:#}"),
            "discarding the partial graph failed; the snapshot may hold an incomplete graph"
        );
    }
    return Err(error).context("submitting codegraph structural graph");
}
```

backed by a commit-scoped delete and a `DELETE /internal/tasks/{id}/graph` endpoint:

```rust
pub async fn delete_commit_graph(
    graph: &Graph,
    repository_id: i64,
    commit_sha: &str,
) -> anyhow::Result<u64> {
    let mut result = graph
        .execute(
            query(
                "MATCH (s:Symbol {repo_id: $repo, commit: $commit}) \
                 WITH s, count(s) AS _c \
                 DETACH DELETE s \
                 RETURN count(_c) AS deleted",
            )
            .param("repo", repository_id)
            .param("commit", commit_sha),
        )
        .await?;
    …
}
```

Scoped to one commit, so other snapshots of the same repository survive — asserted by the live test.

## Change 4 — the skip reason *(corrected in review)*

`perform_indexing` previously produced the bare string `graph skipped`, which reads identically
whether the submit failed or the repository genuinely has no symbols. My first attempt used
`error.root_cause()`:

```rust
format!("graph skipped ({})", error.root_cause())   // ← discards every layer of context
```

@stephane-segning pointed out that `root_cause()` walks to the *innermost* error, stripping exactly
the context the change exists to surface — `submit_graph_paged` labels each page
(`"submitting graph edge page 3"`) and `index_graph` adds its own. The summary would have said
`graph skipped (operation timed out)` with no indication of which page, or whether it died mid-nodes
or mid-edges. The warn log on the line above already uses `{error:#}` for that reason:

```rust
tracing::warn!(error = %detail, "structural graph indexing failed (non-fatal)");
// Carry the cause into the task summary: a repository with no symbols and one whose
// graph never landed are otherwise indistinguishable to an operator reading it.
format!("graph skipped ({detail})")
```

## Tests

| test | asserts |
|---|---|
| `a_paged_graph_write_matches_an_unpaged_one` *(live Neo4j)* | a 25-node/24-edge graph written as 7-per-page pages yields identical node and edge counts to one written whole |
| `discarding_a_snapshot_clears_that_commit_and_leaves_others` *(live Neo4j)* | a partial snapshot is fully removed; a different commit of the same repository is untouched |
| `submit_graph_paged_sends_every_node_page_before_any_edge_page` | page kinds arrive `["nodes","nodes","nodes","edges","edges"]` for 5 nodes + 3 edges at 2/page |
| `submit_graph_paged_stops_at_the_first_failing_page` | a rejected page aborts the sequence, names the page, and sends exactly one request |
| `discard_graph_deletes_the_task_snapshot` | the client issues the `DELETE` with its bearer token |
| `submit_graph_paged_with_nothing_to_send_makes_no_request` | an empty graph costs no round trip |

---

# PR #658 — resume a partially indexed snapshot, retry a truncated response

## Two ways one request loses half an hour

A cratestack index is ~3,400 sequential batches over ~31 minutes, and the run kept no record of its
progress:

```rust
let embeddings = embedder.embed(&texts).await
    .with_context(|| format!("embedding batch {batch_idx}"))?;   // ← batch 3,400 of 3,421
```

That `?` ends the task. The reaper requeues it, a fresh Job starts, and the loop begins again at
`batch_idx = 0` — re-embedding 17,104 chunks already sitting in `code_chunks`, up to
`MAX_ATTEMPTS = 5` times.

And one class of failure made that likelier than it should have been.

## Change 1 — a truncated body is transport, not corruption

```rust
let mut data = response.json::<EmbedResponse>().await.map_err(|e| EmbedError {
    // A malformed 2xx body is not a transport problem — don't retry it.
    transient: false,
    …
```

A connection dropping **mid-body** arrives after a clean `200`, so it never reaches the send-error
branch (`is_timeout() || is_connect()`). It lands here, marked non-retryable, and kills the run.

This is not theoretical. Sweeping the embeddings gateway at prod's shape, 8 trials per size:

```text
 batch      ok  fail%  median s   resp MB   s/text
     5    3/3      0%      1.98      0.26    0.396
    16    8/8      0%      3.10      0.84    0.205
    32    8/8      0%      4.07      1.67    0.183
    48    8/8      0%      6.23      2.50    0.122
    64    7/8     12%      5.09      3.34   IncompleteRead
   128    0/3    100%         -      6.7+   IncompleteRead
```

No 4xx at any size — the ceiling is the connection dropping between 2.50 MB and 3.34 MB, and at 64
it was *intermittent*, which is the worst kind.

The fix separates reading the body from parsing it:

```rust
// Reading the body and parsing it are separated so the two failures stay distinguishable:
// a body that stops arriving mid-stream is a transport failure worth retrying, while one
// that arrives whole and does not parse is deterministic. Decoding straight into the type
// collapses both into one decode error.
let body = response.bytes().await.map_err(|e| EmbedError {
    error: anyhow::Error::new(e).context("reading embeddings response"),
    transient: true,
    retry_after: None,
})?;

let mut data = serde_json::from_slice::<EmbedResponse>(&body).map_err(|e| EmbedError {
    error: anyhow::Error::new(e).context("parsing embeddings response"),
    transient: false,
    retry_after: None,
})?;
```

**My first attempt was wrong and the test caught it.** I tried `transient = e.is_body()`, which never
fired — the failure surfaces as a *decode* error, and matching on `is_decode()` would have swept in
genuinely malformed JSON too. Splitting the steps is what actually distinguishes them.

The test uses a raw `TcpListener`, because `wiremock` always sends a well-formed response:

```rust
if attempt == 0 {
    // Content-Length outruns the bytes written, then the socket closes.
    let _ = stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
          Content-Length: 4096\r\n\r\n{\"data\":[{\"embed",
    );
} else {
    // … a proper response
}
```

`embed` must reach the second connection.

## Change 2 — resume from what is already stored

Storage is already idempotent, keyed on `(repository_id, commit_sha, file_path, start_line, end_line)`,
so resume is a read-and-skip rather than new storage. The runner has no database access, so the set
comes over the internal API:

```rust
/// The `(file_path, start_line, end_line)` of every chunk already stored for one snapshot.
///
/// Together with `repository_id` and `commit_sha` these are the conflict target `upsert_code_chunks`
/// writes against, so a chunk whose key appears here is already present with its embedding and does
/// not need to be embedded again.
pub async fn indexed_chunk_keys(
    pool: &PgPool,
    repository_id: i64,
    commit_sha: &str,
) -> Result<Vec<(String, i32, i32)>, sqlx::Error> { … }
```

served by `GET /internal/tasks/{id}/chunks/indexed`, which resolves the commit the same way the
runner stamps it (`head_sha`, falling back to `default_branch`).

In the runner:

```rust
// Chunks already stored for this snapshot keep their embedding, so an index re-running over a
// commit it has partially indexed embeds only the gap. A lookup that fails is treated as an
// empty set: re-embedding is wasteful, not wrong, and is preferable to failing the run.
let already_indexed = match client.indexed_chunk_keys(context.task_id).await {
    Ok(keys) => keys,
    Err(error) => {
        tracing::warn!(error = %format!("{error:#}"), "indexed-chunk lookup failed; embedding every chunk");
        HashSet::new()
    }
};
let pending = pending_chunks(&out.chunks, &already_indexed);
```

## Change 3 — testing the predicate that actually matters *(added in review)*

@stephane-segning noticed that the load-bearing line had **no test that would fail if it were
wrong**. The three tests in the PR covered the SQL projection and the HTTP parsing — the invariant —
but nothing drove the filter itself. Inverting the `!` would pass every one of them while a resumed
run embedded only what was already stored, skipped everything missing, and reported success.

The predicate is now its own function, and addressed directly:

```rust
/// The chunks of `chunks` that `already_indexed` does not cover, in their original order.
#[must_use]
fn pending_chunks<'a>(
    chunks: &'a [Chunk],
    already_indexed: &HashSet<(String, i32, i32)>,
) -> Vec<&'a Chunk> {
    chunks
        .iter()
        .filter(|c| !already_indexed.contains(&(c.file_path.clone(), c.start_line, c.end_line)))
        .collect()
}
```

Four tests, asserting *which* chunks survive rather than how many:

```rust
let pending = pending_chunks(&chunks, &already);
let paths: Vec<&str> = pending.iter().map(|c| c.file_path.as_str()).collect();
assert_eq!(
    paths,
    vec!["a.rs", "c.rs"],
    "the stored chunk is skipped and the rest are kept, in order"
);
```

plus one that pins every component of the key — a chunk sharing a file with a stored one, or sharing
its start line, is still a different chunk.

**Verified by mutation.** Applying the reviewer's exact suggestion (drop the `!`):

```text
with the predicate INVERTED:
  pending_chunks_keeps_exactly_what_is_not_yet_stored ... FAILED
  pending_chunks_is_empty_when_every_chunk_is_stored ... FAILED
  pending_chunks_matches_on_the_whole_position_not_part_of_it ... FAILED
restored:
  test result: ok. 5 passed
```

---

# Review findings — all five were correct

| # | PR | reviewer | finding | status |
|---|---|---|---|---|
| 1 | 657 | lightbridge-assistant (P2) | `submit_graph` lost its doc comment to the new method | fixed |
| 2 | 657 | stephane-segning (P1) | a mid-sequence page failure strands a partial graph | fixed — discard on failure |
| 3 | 657 | stephane-segning (P2) | `root_cause()` discards the context the summary needs | fixed — `{error:#}` |
| 4 | 658 | lightbridge-assistant (P2) | `submit_chunks` lost its doc comment the same way | fixed |
| 5 | 658 | stephane-segning (P1) | the resume predicate had no test that could fail | fixed — extracted + 4 tests |

**Findings 1 and 4 are the same mistake, made twice.** Both PRs inserted a new method using the
existing method's signature as an anchor, which placed the new code *between* that method's doc
comment and the method itself. The doc silently transferred to the new function and the old one was
left bare. It compiles, rustdoc renders it, and nothing catches it — worth remembering as a hazard of
anchor-based insertion.

**Finding 2 is the most valuable.** It identifies a regression *this PR introduced*: trading one
atomic write for N non-atomic ones, in a system where a partial graph is indistinguishable from a
complete one. It also names precisely why the existing test did not catch it —
`submit_graph_paged_sends_every_node_page_before_any_edge_page` asserts happy-path ordering, not what
state a mid-sequence failure leaves behind. That gap is now covered from both ends: a client test
that the sequence stops at the first failure, and a live Neo4j test that the discard clears exactly
the right snapshot.

**Finding 5 is the same class of criticism applied to #658** — tests that prove the invariant rather
than the code's use of it. Its suggested mutation (invert the predicate) is now a test that fails.

---

# Verification

```bash
docker compose up -d postgres neo4j
docker compose ps --format '{{.Service}}: {{.Status}}'
DATABASE_URL=postgres://lightbridge:lightbridge@localhost:5432/lightbridge cargo test --workspace
cargo test -p control-plane --bin control-plane integrations::neo4j -- --ignored --test-threads=1
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Container state is quoted with every result deliberately. Earlier in this work a suite reported
294 passed / 180 failed, and the Neo4j tests reported 3 failures, purely because those containers had
stopped — the numbers mean nothing without evidence both were healthy.

---

# What neither PR does

- **Raise `CONTROL_PLANE_REQUEST_TIMEOUT_SECS`.** It moves the wall without changing the slope.
- **Raise `INDEX_EMBED_BATCH_SIZE`** — config, not code:
  [ai-helm-values#446](https://github.com/ADORSYS-GIS/ai-helm-values/pull/446) takes it 5 → 16, which
  moves a cratestack index from ~81% of its deadline to ~41%.
- **Bounded concurrency.** Still available, but with cratestack at 52% of its deadline it is no
  longer motivated. Measure before building.
- **Restore per-chunk byte capping in the chunker** —
  [lci-codegraph#55](https://github.com/ADORSYS-GIS/lci-codegraph/issues/55).
