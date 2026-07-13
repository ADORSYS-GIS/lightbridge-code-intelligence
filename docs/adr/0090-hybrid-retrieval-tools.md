# ADR-0090: Hybrid retrieval tools — semantic + structural code search

- **Status:** Proposed
- **Date:** 2026-07-13
- **Deciders:** @stephane-segning
- **Builds on:** [ADR-0089](0089-embeddings-on-the-code-graph.md) (symbol embeddings on `:Symbol`), [ADR-0066](0066-deep-tier-external-knowledge-tools.md) (control-plane-mediated tools), [ADR-0062](0062-two-tier-review-fast-auto-deep-on-demand.md) (per-tier tool allowlist)
- **Complements (does NOT replace):** the existing `search`, `lightbridge_graph_find_symbol`, `lightbridge_graph_get_callers` tools

## Context and Problem Statement

The deep-tier agent has three separate retrieval tools it stitches together by hand:

- **`search`** — pgvector *chunk* semantic search. The runner embeds the query with its embeddings
  client, then `POST /internal/tasks/{id}/search` returns the nearest `code_chunks`
  ([`http/internal.rs`](../../services/control-plane/src/http/internal.rs) `search`,
  [`agent-clients/src/control_plane.rs`](../../services/agent-clients/src/control_plane.rs)).
- **`lightbridge_graph_find_symbol` / `lightbridge_graph_get_callers`** — Neo4j *structure*
  ([`review-agent/src/tools/graph.rs`](../../services/review-agent/src/tools/graph.rs)).

To answer "what code is semantically like X, and what calls it?" the model must run `search`, read the
hits, guess the symbol names, then issue graph lookups — several turns, and the join happens in the
model's head where it can drift. [ADR-0089](0089-embeddings-on-the-code-graph.md) puts an embedding on
each `:Symbol` behind a Neo4j vector index. That makes it possible to do the **semantic hit and its
structural neighborhood in one mediated call** — fewer turns, and the structure travels *with* the
semantic match instead of being re-derived.

The goal (operator directive): **add** hybrid tools that combine embedding similarity with the graph —
"semantic *and* exact match" — for more efficient retrieval, **while keeping the existing tools**. This
is additive: the new tool goes into the same per-tier allowlist alongside the ones already there; none
are removed.

## Decision Drivers

- **Fewer turns / tighter grounding** — one call returns semantically-relevant symbols *with* their
  call neighborhood, instead of N round-trips joined by the model.
- **"Semantic and exact match"** — blend fuzzy vector similarity with *exact* symbol-name / qualified
  match, so a precise name (`processPayment`) is never missed while similar behavior is still surfaced.
- **Additive, not a swap** — keep `search` (chunk-level, exact pgvector) and the two `graph_*` tools;
  the hybrid tool is a new lane in the same [ADR-0062](0062-two-tier-review-fast-auto-deep-on-demand.md) allowlist.
- **Same trust boundary** — mediated through the control-plane internal API like every other tool
  ([ADR-0037](0037-agent-acts-via-mediated-tools.md)/[ADR-0066](0066-deep-tier-external-knowledge-tools.md)): Neo4j
  creds stay in the trust boundary, the query is embedded runner-side, results are untrusted quoted
  context ([ADR-0036](0036-auto-read-agent-instruction-files.md) posture).

## Considered Options

- **One combined tool `lightbridge_graph_semantic_search` (chosen).** Parameters:
  `query` (text), `k` (top-K), `expand_hops` (optional graph expansion depth), `relation` (optional
  edge filter: `calls`/`method`/`contains`), and an `exact` flag/behaviour that also unions **exact
  `:Symbol.label` name matches** into the result and ranks them first. Flow: runner embeds `query` →
  `POST /internal/tasks/{id}/graph_search {vector, k, expand_hops, relation, exact_terms}` →
  control-plane runs `db.index.vector.queryNodes('symbol_embedding', k, $vec)`, unions exact-name
  matches, and (if `expand_hops>0`) traverses the neighborhood → returns symbols
  `{node_id, label, source_file, start_line, score}` plus the expanded edges. One tool, both recall
  modes (semantic + exact) + optional structure.
- **Two tools (a pure `graph_vector_search` and a separate `graph_expand`).** More composable but
  reintroduces the multi-turn stitch this ADR exists to remove. Rejected — the point is the single
  call.
- **Fold it into the existing `search`.** Overloading `search` (pgvector, chunk) to also hit Neo4j
  would blur two stores/granularities behind one name and break the ADR-0089 "keep pgvector's lane
  intact" boundary. Rejected — a distinct tool keeps each lane legible.

## Decision Outcome

Add **`lightbridge_graph_semantic_search`** — a single control-plane-mediated tool that does
**semantic (vector) + exact (name) + optional structural (graph-expansion)** retrieval over the
[ADR-0089](0089-embeddings-on-the-code-graph.md) `:Symbol` embeddings, via a new
`POST /internal/tasks/{id}/graph_search` endpoint (query embedded runner-side, same pattern as
`search`; Neo4j creds + Cypher in the control plane; results untrusted).

- It is registered in the per-tier `review.<tier>.tools` allowlist
  ([ADR-0062](0062-two-tier-review-fast-auto-deep-on-demand.md)) **alongside** the retained `search`,
  `lightbridge_graph_find_symbol`, and `lightbridge_graph_get_callers` — **none are removed**. Deep
  tier gets it by default; fast tier opts in by config.
- Ships **after / with** [ADR-0089](0089-embeddings-on-the-code-graph.md) (needs the symbol embeddings
  + vector index to exist).

### Consequences

- **Good** — one-call semantic+structural retrieval → fewer turns and findings grounded in real
  structure; "semantic and exact" in a single tool; strictly additive (the existing tools and their
  exact pgvector lane stay); reuses the existing mediation + allowlist machinery, so no new trust
  surface.
- **Bad** — one more tool competing for the model's tool-choice and the prompt-tool budget (governed by
  [ADR-0070](0070-window-proportional-prompt-budgets.md) window-proportional injection + the ADR-0062
  allowlist); Neo4j vector search is **approximate** ([ADR-0089](0089-embeddings-on-the-code-graph.md));
  an overlapping tool set can dilute tool choice — **measure** whether it displaces or complements
  `search`/`graph_*` (the ADR-0069 review-run telemetry already records offered/used tools).
- **Neutral** — a candidate for the fast tier only after measuring its turn/recall payoff there; the
  `exact` blend's ranking (exact-first vs interleaved) is a tuning knob to settle during
  implementation, not a decision this ADR locks.
