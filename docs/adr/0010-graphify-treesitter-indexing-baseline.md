# ADR-0010: Graphify + tree-sitter as the indexing baseline

- **Status:** Accepted — but the **Graphify (structural-graph) half is Superseded by [ADR-0086](0086-in-house-code-graph-crate.md)**. The tree-sitter semantic-chunking half stands; the structural graph is now built in-process by the in-house `lci-codegraph` crate (Graphify removed).
- **Date:** 2026-06-18 (graph half superseded 2026-07-13)

## Context and Problem Statement

The indexing pipeline must turn a repository into a structural graph (Neo4j) and a semantic chunk
store (pgvector). It needs syntax-aware parsing across many languages, stable source ranges for
chunk boundaries, and a path to richer, language-specific enrichment.

## Decision Drivers

- Multi-language, syntax-aware parsing
- Stable symbol extraction and chunk boundaries
- A baseline that works before deep per-language tooling exists
- Room to layer language enrichers on top

## Considered Options

- **Graphify + tree-sitter baseline, with language enrichers** — tree-sitter for syntax-aware
  parsing and stable ranges; Graphify for multi-modal graph extraction and Neo4j push; enrichers
  (rust-analyzer, tsserver, gopls, etc.) layered for deeper resolution.
- **LSP-only** — accurate but heavy, slow to stand up across all languages, and brittle as a
  baseline.
- **Regex / heuristic parsing** — cheap but inaccurate and unstable.

## Decision Outcome

Chosen option: **Graphify + tree-sitter as the baseline**, with language enrichers added
incrementally. Tree-sitter provides syntax-aware parsing and stable ranges; enrichers improve
symbol/dependency resolution where available.

### Consequences

- Good, because we get a working multi-language baseline without full LSP coverage.
- Good, because chunk boundaries and symbol ranges are stable and syntax-aware.
- Bad, because tree-sitter is syntactic, not semantic — graph edges may be approximate until
  enrichers fill gaps.
- Neutral, because enrichers can be added per language over time (see
  [indexing and storage](../indexing-and-storage.md)).
