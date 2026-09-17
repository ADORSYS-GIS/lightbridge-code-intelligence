# ADR-0117: The structural graph is written in bounded pages against an indexed identity

- **Status:** Accepted
- **Date:** 2026-09-17
- **Deciders:** @leghadjeu-christian
- **Amends:** [ADR-0086](0086-in-house-code-graph-crate.md) (its "runner extracts, control plane owns
  the Neo4j write" contract gains a paging protocol and a snapshot discard; the engine choice is
  untouched), [ADR-0114](0114-hybrid-graph-vector-symbol-search.md) (its `ensure_indexes` bootstrap
  gains a third index — the hybrid-search query and MCP tool are unchanged)
- **Source of truth:** #656

## Context and Problem Statement

The structural half of an index run leaves the runner as **one request**: `submit_graph` posts the
whole repository's nodes and edges to `POST /internal/tasks/{id}/graph`, and the control plane writes
them in a single Neo4j transaction.

That shape ties two independent costs to one variable — repository size:

- **Request size** scales with the graph, against a fixed `DefaultBodyLimit` (32 MiB).
- **Request duration** scales with the graph, against a fixed client timeout
  (`DEFAULT_REQUEST_TIMEOUT_SECS`, 180 s).

A third cost is independent of the request shape but compounds with it: every node `MERGE` and both
endpoint `MATCH`es of every edge address a `:Symbol` by the triple `(repo_id, commit, node_id)`, and
nothing in the schema indexes that triple. Each lookup is a label scan.

So the largest indexable repository is not a property of the platform's capacity — it is whatever
happens to fit inside two constants, and it shrinks as more repositories are indexed. What should
bound a structural submit?

## Evidence

### Identity lookups are scans

Profiled on Neo4j 5.26.29 Community seeded to 60,018 `:Symbol` nodes, running the statements
`upsert_graph` actually issues with a single row each — so these are the cost of **one** lookup, not
of a batch. The only variable between the two runs is whether `(repo_id, commit, node_id)` carries
the composite `IS UNIQUE` constraint.

The node upsert without it resolves the node by reading the whole label and discarding almost all of
it:

```text
| Operator         |    Rows | DB Hits | Details
| +SetProperties   |       1 |       2 |
| +Merge           |       1 |       0 | CREATE (s:Symbol {repo_id: …, commit: …, node_id: n.id})
| +Filter          |       1 |  60,496 | (s.repo_id = … AND s.commit = … AND s.node_id = n.id)
| +NodeByLabelScan |  60,018 |  60,019 | s:Symbol

Total database accesses: 120,517
```

With the constraint the scan and the filter are gone entirely — the planner seeks straight to the
node, and `Rows` at the leaf drops from 60,018 to 1:

```text
| Operator                      | Rows | Details
| +SetProperties                |    1 |
| +Merge                        |    1 | CREATE (s:Symbol {repo_id: …, commit: …, node_id: n.id})
| +NodeUniqueIndexSeek(Locking) |    1 | UNIQUE s:Symbol(repo_id, commit, node_id)

Total database accesses: 3
```

The edge write matches **both** endpoints, so it pays the lookup twice and the two plans diverge
further:

| statement `upsert_graph` issues | without the constraint | with it |
|---|---|---|
| node `MERGE` (one node) | 120,517 | **3** |
| edge `MATCH` + `MATCH` + `MERGE` (one edge) | 241,037 | **9** |

The unindexed cost is a function of *every* `:Symbol` in the database — all repositories, all
retained commits — not of the repository being written. A submit performing ~21,000 such lookups
therefore gets slower every time an unrelated repository is indexed, which is the shape of a write
path that worked until it didn't.

### The index applies to all three properties or to none

Verified on a controlled 20,000-node corpus with the same constraint shape, because the composite
behaviour decides which queries actually benefit:

```
repo_id + commit + node_id   NodeUniqueIndexSeek            2 db hits
repo_id + commit             NodeByLabelScan + Filter  22,000 db hits
repo_id                      NodeByLabelScan + Filter  20,000 db hits
commit + node_id             NodeByLabelScan + Filter  20,051 db hits
```

There is no partial or prefix use. This matters because the write path is precisely the all-three
case — the node `MERGE`, both endpoint `MATCH`es, and the ADR-0116 embedding attach all supply the
full triple. The queries that supply two (`list_symbols`, `prune_graph`, the caller side of
`get_callers`) keep scanning, but they run once per user request rather than tens of thousands of
times per index run, so the seek covers the hot path exactly.

### The single request is a size *and* a duration ceiling

ADR-0116 removed vectors from the structural payload, which dropped it from ~582 MB to ~8.8 MB on a
large repository and cleared the 32 MiB limit. The duration ceiling underneath it then became
reachable: a submit observed in prod on `ADORSYS-GIS/CoopData` (5,154 nodes, 7,893 edges) reached
Neo4j and timed out client-side at exactly 180 s, logging
`structural graph indexing failed (non-fatal)`. The task still reported `succeeded` with a semantic
index and no structural one.

That observation is a symptom, not the decision driver. The driver is that both ceilings are fixed
constants sitting in front of an unbounded input.

## Decision Drivers

- **A request's cost should be a function of a configured unit of work, not of repository size.** A
  fixed timeout in front of an unbounded payload is a size limit expressed in seconds, and one that
  nobody can read off the configuration.
- **Identity lookups should seek, not scan.** The triple is how every symbol read and write addresses
  a node; an unindexed primary access path is a defect in the schema regardless of what it currently
  costs.
- **A snapshot is whole or it is absent.** A partial graph is worse than no graph: a missing edge is
  indistinguishable from a symbol that genuinely has no callers, so a subset reads as a complete
  answer and is silently wrong.
- **Operational knobs over code changes.** The page size is the kind of value an operator needs to
  move under a slow or contended database without shipping a release.

## Considered Options

### A — Bounded pages, an indexed identity, and an all-or-nothing snapshot (chosen)

The runner slices the graph into pages of a configured size and submits each as its own request;
`ensure_indexes` declares the identity constraint; a page sequence that fails discards the snapshot.

### B — Raise the timeout

One line, and it defers rather than removes the ceiling: the next repository is larger. It also
lengthens the window in which a single stalled request holds a Job open against its
`activeDeadlineSeconds`, and it leaves the scan cost — which is the reason the duration is what it is
— entirely in place.

### C — Declare the constraint, keep the single request

Genuinely large: the seek alone takes a node upsert from 120,517 db accesses to 3, which is most of
the observed duration. Rejected **as the whole answer** because it fixes the constant and
leaves the shape: duration still scales with repository size against a fixed timeout, and the 32 MiB
body limit is still a size ceiling one growth spurt away. It makes the current corpus fit; it does
not make fit a property of the design.

### D — Stream the graph over one long-lived request

Chunked transfer or a WebSocket would bound memory without bounding per-request duration, which is
the constant actually being hit. It also replaces a plain REST endpoint (ADR-0092's bearer-auth
surface, a `DefaultBodyLimit` per route) with a streaming protocol that needs its own framing,
resumption and error semantics — a new transport for the same problem pages solve with a `for` loop.

### E — Hold every page open in one Neo4j transaction

Preserves the all-or-nothing property at the database rather than by compensation, so no discard is
needed. Rejected: it requires a transaction that outlives a single HTTP request, which means
server-side transaction state keyed by task, a lifetime to manage, and a lock footprint held for the
duration of the whole submit. That is a substantial amount of machinery to avoid a `DELETE`.

## Decision Outcome

Chosen option: **A**.

**Pages are the unit of submission.** `submit_graph_paged` sends all node pages, then all edge pages:

```rust
for (page, chunk) in nodes.chunks(page_size).enumerate() {
    self.submit_graph(task_id, GraphBatch {
        commit_sha: commit_sha.to_string(),
        nodes: chunk.to_vec(),
        edges: Vec::new(),
    })
    .await
    .with_context(|| format!("submitting graph node page {page}"))?;
}
```

Nodes-before-edges is load-bearing, not incidental ordering: an edge is written by `MATCH`ing both
endpoints, and a `MATCH` that finds nothing is a silent no-op rather than an error, so an edge whose
target has not yet been sent would be dropped without a trace.

The size is `GRAPH_SUBMIT_PAGE_SIZE`, defaulting to `DEFAULT_GRAPH_PAGE_SIZE` (2,000) and forwarded
to the Job like the other indexing knobs. `ingest_graph`'s early return widens to
`nodes.is_empty() && edges.is_empty()`, because an edge-only batch is now a normal shape.

Within a page nothing changes: each request is still one transaction with one `UNWIND`-driven
statement per side, so a page is one Bolt round trip carrying 2,000 rows — not 2,000 round trips.

**Identity is indexed.** `ensure_indexes` declares, after the vector and fulltext indexes:

```cypher
CREATE CONSTRAINT symbol_identity IF NOT EXISTS
FOR (s:Symbol) REQUIRE (s.repo_id, s.commit, s.node_id) IS UNIQUE
```

A uniqueness constraint rather than a bare index: it creates its own backing range index, it lets
`MERGE` plan a unique-index seek, and the triple genuinely is unique — a second node sharing it would
be a duplicate symbol, so the constraint states an invariant the write path already assumes.

It is declared **last and non-fatally**. Creation is rejected outright if duplicate triples already
exist, and propagating that would take the vector and fulltext indexes down with it — an absent
identity index costs write latency, not correctness, so it warns and steps over.

**A failed sequence leaves nothing behind.** Pages commit individually, so `index_graph` compensates:

```rust
if let Err(error) = client
    .submit_graph_paged(context.task_id, &commit_sha, &nodes, &edges, page_size)
    .await
{
    if let Err(discard) = client.discard_graph(context.task_id).await {
        tracing::warn!(
            error = %format!("{discard:#}"),
            "discarding the partial graph failed; the snapshot may hold an incomplete graph"
        );
    }
    return Err(error).context("submitting codegraph structural graph");
}
```

`DELETE /internal/tasks/{id}/graph` resolves the task to its `(repository_id, commit_sha)` and
`DETACH DELETE`s that snapshot. The commit returns to "not indexed", which every reader already
handles.

### Consequences

- **Good** — a request's duration is bounded by `page_size`, a constant, instead of by repository
  size. Repository growth adds requests rather than seconds to one request, so the 180 s timeout is
  measured against a fixed unit of work and stops being an unwritten size limit.
- **Good** — per-lookup cost drops from a full label scan to a unique-index seek (120,517 → 3 db
  accesses for a node upsert, 241,037 → 9 for an edge write), and stops growing with the number of
  repositories indexed. This is the larger share of the observed improvement; paging is what keeps
  it bounded as repositories grow.
- **Good** — the 32 MiB body limit is no longer reachable by an ordinary repository, since a page's
  size is configured rather than emergent.
- **Good** — `graph skipped` in the task summary now carries the cause. A repository with no symbols
  and one whose graph never landed were previously indistinguishable to an operator.
- **Good** — the page size is an operator knob (`GRAPH_SUBMIT_PAGE_SIZE`), so a slow or contended
  Neo4j is a values change, not a release.
- **Bad** — a submit is no longer atomic at the transport. All-or-nothing is now maintained by
  compensation, which can itself fail; when it does, the snapshot keeps an incomplete graph and only
  a warning records it. The failure mode is narrow (the discard is a single `DETACH DELETE` against a
  database the submit was just talking to) but it is real, and it is the price of not holding a
  transaction open across requests.
- **Bad** — a paged submit is not idempotent as a whole. A retry re-`MERGE`s pages that already
  landed, which is harmless for nodes and edges but means the work is redone rather than resumed.
- **Neutral** — more requests per run (a 5,154-node, 7,893-edge graph becomes 7 requests at the
  default page size, from 1). Each carries the same bearer token and hits the same route; the added
  cost is per-request overhead, not per-row work.
- **Neutral** — the constraint is declared at startup and is idempotent, so no migration step exists.
  On a database that somehow holds duplicate triples it will not be created, and the warning
  `symbol identity constraint not created; symbol lookups will scan the label` is the only signal —
  worth confirming its absence after rollout rather than assuming the index landed.
- **Neutral** — existing snapshots are unaffected. This changes how a graph is written, not what is
  written, so no re-index is required.

## Out of scope

- Resumption. A failed sequence restarts from the first page rather than continuing from the last one
  that landed; checkpointing the structural pass is separate work.
- Concurrency. Pages are submitted sequentially. Overlapping them would trade Neo4j lock contention
  for wall-clock time and needs its own measurement.
- The two-property read queries (`list_symbols`, `prune_graph`, the caller side of `get_callers`),
  which still scan the label. They run once per user request and are not on the indexing path.

## More Information

- [ADR-0086](0086-in-house-code-graph-crate.md) — `lci-codegraph` as the sole structural engine, and
  the runner-extracts/control-plane-writes split this refines.
- [ADR-0116](0116-one-walk-node-id-symbol-embeddings.md) — removed vectors from the structural
  payload, which cleared the size ceiling this addresses the duration ceiling behind.
- [ADR-0114](0114-hybrid-graph-vector-symbol-search.md) — the `ensure_indexes` bootstrap the identity
  constraint joins.
- [ADR-0092](0092-per-task-runner-tokens.md) — the per-task token every page carries.
- #656 — the ticket this implements.
- `services/agent-clients/src/control_plane/indexing.rs`,
  `services/agent-runner/src/indexer/graph.rs`,
  `services/control-plane/src/http/internal.rs`,
  `services/control-plane/src/integrations/neo4j.rs`.
