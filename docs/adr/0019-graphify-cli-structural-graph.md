# ADR-0019 — Graphify (standalone CLI) as the structural-graph extractor

| Field      | Value |
|------------|-------|
| Status     | **Superseded by [ADR-0086](0086-in-house-code-graph-crate.md)** (was: Accepted). Graphify has been fully removed — the structural graph is now built in-process by the in-house `lci-codegraph` crate; no Python/Graphify subprocess or venv remains in the runner image. |
| Date       | 2026-06-19 (superseded 2026-07-13) |
| Deciders   | @ssegning |
| Epic       | #5 (indexer + agent, slice 3) |
| Refines    | [ADR-0010](0010-graphify-treesitter-indexing-baseline.md), builds on [ADR-0003](0003-dual-retrieval-neo4j-pgvector.md) |

## Context

Slice 3 builds the **structural** half of dual retrieval (ADR-0003): a Neo4j graph of symbols and
their `contains` / `method` / `calls` relationships. ADR-0010 named "Graphify + tree-sitter" as the
indexing baseline but didn't pin *how* Graphify is integrated. Two facts forced a concrete decision:

1. **Slice 2 already shipped our own tree-sitter chunker** for the pgvector (semantic) path — we did
   not use Graphify there. For slice 3 we either build a *second* hand-rolled tree-sitter pass
   (symbols + call graph) or lean on Graphify for the graph.
2. **What Graphify actually is** (verified by installing `graphifyy==0.8.44` and running it): a
   Python CLI that extracts an AST→graph over **36 languages**, fully **headless with no API key**
   (`graphify extract <dir> --no-cluster`), emitting `graphify-out/graph.json` (nodes with
   `source_file` + start line; edges with `relation` ∈ {`contains`, `method`, `calls`, …}). It has
   **no embeddings / pgvector** capability. It *can* push the graph to Neo4j itself
   (`graphify export neo4j --push bolt://… --user … --password …` / `NEO4J_PASSWORD`), but using that
   would require the **runner** to hold Neo4j credentials — which we deliberately avoid (see Decision).

## Decision

**Use Graphify as the structural-graph extractor, bundled into the agent-runner image; the runner
spawns it and the control plane owns the Neo4j write.**

- The runner image now bundles **Python + `graphifyy`** alongside the Rust binary (one image, built
  in stages so compilers don't ship to runtime).
- The runner spawns `graphify extract <checkout> --no-cluster` (no LLM — AST only), parses
  `graph.json`, and POSTs nodes+edges to **`POST /internal/tasks/{id}/graph`**.
- The **control plane** writes to Neo4j over Bolt (`neo4rs`), scoping every node/edge by
  `(repository_id, commit_sha)`. The untrusted per-task Job never holds Neo4j credentials — same
  trust boundary as chunk ingestion (ADR-0002).
- **Graphify does not replace the slice-2 chunker.** It has no embeddings; the semantic (pgvector)
  path stays with our tree-sitter chunker. The two indexers map cleanly onto dual retrieval:
  tree-sitter chunker → pgvector (semantic), Graphify → Neo4j (structural).

We **do not** use Graphify's own `export neo4j --push`, even though it exists, for two reasons:

1. **Trust boundary (ADR-0002).** The runner is an untrusted per-task Job (it clones arbitrary
   repos). Our datastores — Postgres/pgvector *and* Neo4j — are written **only** by the control
   plane; the Job never holds Postgres or Neo4j credentials. `index_graph` is the graph-side twin of
   the chunk path (`index_checkout`): the runner produces data, the control plane writes the store.
   A direct push would put `NEO4J_PASSWORD` in the Job — the exact long-lived-secret-in-an-untrusted-
   Job anti-pattern we are hardening elsewhere (cf. `AGENT_RUNNER_TOKEN`).
2. **Graphify stays swappable.** Owning the ingestion and the Neo4j schema (scoped by
   `(repository_id, commit_sha)`, synthetic nodes filtered) keeps Graphify behind a thin seam —
   "runner emits graph data → control plane writes Neo4j." Replacing or dropping Graphify later (e.g.
   for our own extractor) never touches the control-plane write or the graph schema.

## Consequences

**Good**
- 36-language structural coverage for free, vs the 4 languages our chunker handles — and we avoid
  hand-rolling/maintaining a second tree-sitter symbol+call extractor.
- Headless, offline, deterministic AST extraction; no API key, no paid calls (the runner explicitly
  strips `*_API_KEY` env before spawning so a stray key can't trigger Graphify's optional LLM pass).
- Neo4j credentials stay in the control plane.

**Trade-offs**
- The runner image is larger (Python runtime + 36 grammar wheels). Accepted — the goal is broad
  language support in one image.
- We depend on Graphify's `graph.json` schema; pinned to `0.8.44` and guarded by a parser unit test
  on a captured sample. We could shed code by letting Graphify push to Neo4j itself, but we keep the
  write in the control plane regardless (trust boundary; keeps Graphify swappable — see Decision).
- Graph edges are AST-approximate (no semantic resolver). Acceptable as the baseline (ADR-0010);
  Graphify's `--mode deep` LLM inference is deliberately **off**.

## Alternatives rejected

- **Hand-roll a second tree-sitter pass for the graph** — duplicates Graphify for fewer languages
  and more maintenance.
- **Have Graphify also feed pgvector** — impossible; it produces no embeddings and its nodes carry
  no end-range/body to embed.
- **Let the runner push to Neo4j directly** (Graphify's `export neo4j --push`) — simpler, but puts
  Neo4j credentials in the untrusted Job and couples us to Graphify's schema. Rejected on both counts
  (see Decision).
