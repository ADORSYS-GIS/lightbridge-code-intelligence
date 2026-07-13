# ADR-0086: In-house code-graph crate — retire Graphify

- **Status:** Accepted — Implemented (Graphify fully removed; `lci-codegraph` is the sole graph engine)
- **Date:** 2026-07-12 (implemented 2026-07-13)
- **Deciders:** @stephane-segning
- **Supersedes:** [ADR-0019](0019-graphify-cli-structural-graph.md); the graph half of [ADR-0010](0010-graphify-treesitter-indexing-baseline.md)

## Context and Problem Statement

Structural code-graph extraction today runs through **Graphify** — a standalone, Python-based,
multi-language (36-grammar) AST→graph extractor bundled into the runner image
([ADR-0019](0019-graphify-cli-structural-graph.md), the graph half of the
[ADR-0010](0010-graphify-treesitter-indexing-baseline.md) indexing baseline). The indexer spawns it
as `graphify update … --no-cluster`, parses the `graph.json` it writes, and hands the code
nodes+edges to the control plane (which owns the Neo4j write) — see
[`indexer/graph.rs`](../../services/agent-runner/src/indexer/graph.rs). The graph it produces is what
powers the deep-tier retrieval tools `lightbridge_graph_find_symbol` and
`lightbridge_graph_get_callers` ([`review-agent/src/tools/graph.rs`](../../services/review-agent/src/tools/graph.rs)).

Graphify is the **only** reason a Python runtime lives in the indexer image, and it is the driver of
that image's ~4Gi index-Job memory footprint. That footprint already forced a split: the *review*
runner was carved out to a slim, no-Python image (PR [#207](https://github.com/vymalo/lightbridge-code-intelligence/pull/207))
precisely so review pods would not carry the Graphify weight. The result is two runner images that
must be built, scanned, and kept in lockstep. Meanwhile the semantic (pgvector) path is *already*
in-house Rust: [`indexer/chunker.rs`](../../services/agent-runner/src/indexer/chunker.rs) uses
tree-sitter to extract the same class of symbols Graphify does (functions, methods, structs, enums,
traits, classes, modules) before embedding via the OpenAI-compatible eaig gateway
([`agent-clients/src/embeddings.rs`](../../services/agent-clients/src/embeddings.rs), qwen3-embedding-8b,
4096-dim). The structural half is the last thing pinning us to an external runtime.

This is a **control-plane v2** decision ([RFC-0007](../rfc/0007-control-plane-v2-planes.md)):
the new **agent-plane** ([ADR-0085](0085-agent-execution-plane.md)) wants to run its `index` mode
from **one lean binary**, and a Python-shaped dependency is exactly what makes that impossible. The
question: do we keep driving graph extraction out to an external tool, or own it?

## Decision Drivers

- **Collapse the runner images.** With no Python in the indexer, the review and index images have no
  reason to differ — the [#207](https://github.com/vymalo/lightbridge-code-intelligence/pull/207)
  split dissolves and the single agent-plane binary ([ADR-0085](0085-agent-execution-plane.md))
  becomes viable. This is the load-bearing driver.
- **Right-size the index Job.** A Rust extractor is bounded and profileable; the ~4Gi footprint is a
  Python/Graphify artifact, not an intrinsic cost of graph extraction.
- **Own a core competency.** Code-graph extraction *is* the product for a code-intelligence tool.
  An external dependency we cannot tune per-language is a strategic liability, not just an image bloat.
- **One coherent indexing crate.** Chunking, graph edges, and embedding-prep are three views of the
  same parsed tree. Today the graph lives in an external process while chunking is Rust — the tree
  is parsed twice, by two toolchains. Owning both lets us parse once.
- **Reuse what already exists.** The tree-sitter grammars and symbol extraction in `chunker.rs` are
  already in the binary; the graph is *edges built on top of the per-file symbols the chunker already
  finds*, not a new parser stack.

## Considered Options

- **Option A — keep Graphify.** Zero migration cost and a mature 36-grammar extractor. Rejected: it
  hard-pins a Python runtime into the image, drives the ~4Gi index-Job footprint, keeps the
  [#207](https://github.com/vymalo/lightbridge-code-intelligence/pull/207) image split alive, and
  **structurally blocks the single agent-plane binary** ([ADR-0085](0085-agent-execution-plane.md)) —
  the whole point of control-plane v2's indexer. It is also an external dependency we cannot tune.
- **Option B — swap Graphify for a different external graph tool** (e.g. another CLI/service).
  Rejected on mechanism: it is the *same class* of dependency — a separate runtime, its own image
  weight and process-boundary marshalling (spawn → temp `graph.json` → parse) — with *less* control
  than Graphify, which we at least already understand. Trading one external process for another does
  not collapse the images or unblock the single binary.
- **Option C — build an in-house Rust crate `lci-codegraph` (chosen).** Owns chunking + graph +
  embedding-prep as one crate on the shared tree-sitter parse, under the `lci-*` prefix
  ([ADR-0083](0083-platform-crate-architecture-and-cratestack-data-layer.md)).

## Decision Outcome

Chosen option: **Option C — an in-house Rust crate, proposed name `lci-codegraph`.** It absorbs the
existing `indexer/chunker.rs` + `indexer/mod.rs` logic and adds the structural graph Graphify does
today, so the indexer becomes one Rust crate with no external process and no Python. The control
plane's Neo4j-write contract is unchanged: the crate emits the same node/edge payload shape the
`graph.json` parse produces today (`GraphNodePayload`/`GraphEdgePayload`), so the retrieval tools and
graph store are untouched behind the seam.

### Required capabilities (all decided)

**1. Structural code graph via tree-sitter directly.** Reuse the grammars and the
`interesting_node` symbol extraction already in `chunker.rs`. On top of the per-file symbols the
chunker finds, the crate builds a **structurally functioning graph**: symbols (functions, methods,
structs, enums, traits, classes, modules), their **definitions**, their **references**, and
**call/reference edges** (caller→callee) with **cross-file symbol resolution** — resolving a
reference in file A to a definition in file B. This is exactly what `graph_find_symbol` (name →
node) and `graph_get_callers` (node → reverse-call-graph) consume, so the tools keep working against
an in-house-built graph. Cross-file resolution is the hard, high-value part and the primary parity
target against Graphify (see Risk register).

**2. Embeddings for semantic search (already ours; now co-located).** The crate owns chunking and
embedding *preparation*, feeding the existing OpenAI-compatible embeddings path unchanged
(qwen3-embedding-8b, 4096-dim, via the internal eaig gateway). Chunking, graph, and embedding-prep
become **one crate over one parse of the tree** rather than logic scattered across `chunker.rs`,
`graph.rs` (external), and `mod.rs`. Batching/round-trip tuning ([`IndexTuning`](../../services/agent-runner/src/indexer/mod.rs),
`INDEX_EMBED_BATCH_SIZE` et al., and the gateway large-response cap it exists to work around) carries
over verbatim.

**3. PDF text extraction.** Repos carry documentation as PDFs; those should be chunked, embedded, and
searchable like any other doc. The crate extracts text from PDF files during the walk and feeds it to
the same windowed-chunk → embed path text files already take. The concrete Rust PDF-text crate
(`pdf-extract`, `lopdf`, …) is deliberately **not** fixed here — see Unresolved questions — because
PDF parsing over untrusted repo input is a crash/OOM surface that must be bounded, not chosen casually.

**4. Configurable ignore-list.** Skip paths like `target/`, `node_modules/`, `.git/`, `dist/`,
`build/`, `vendor/`, `.venv/`, `__pycache__/` even when erroneously committed. Today this is a
hardcoded `matches!` set in [`indexer/mod.rs`](../../services/agent-runner/src/indexer/mod.rs); the
crate replaces it with **gitignore-style glob semantics, operator-configurable, with those names as
built-in defaults** (the "make everything configurable" requirement). Crucially it **composes with,
not replaces, the repo's own `.gitignore`** — the operator list is an *additional* filter for junk
that slipped past the repo's ignore rules, not a substitute for honouring them. What was skipped is
logged so a misconfiguration hiding real files is diagnosable rather than silent.

### Where it sits

`lci-codegraph` is a shared `lci-*` library crate consumed by the agent-plane's `index` mode
([ADR-0085](0085-agent-execution-plane.md)); it holds no `kube`/`sqlx`/forge dependencies and submits
through the internal API exactly as `index_checkout` does today. It slots into the
[RFC-0002](../rfc/0002-incremental-layered-indexing.md) incremental-indexing model unchanged — it is
the *extractor* the layered/snapshot machinery drives, so snapshot reuse and pruning are orthogonal
to this decision.

### Migration shape

Language-by-language, behind the existing seam. Graphify keeps running until a language reaches
parity; the crate emits the same payloads, so the control plane, Neo4j store, and retrieval tools do
not know which extractor produced a given graph. Parity is enforced by golden tests on a fixture repo
plus a **side-by-side comparison against Graphify's `graph.json`** during migration (Risk R1). Only
when every actively-indexed language is at parity does Graphify — and with it Python and the
[#207](https://github.com/vymalo/lightbridge-code-intelligence/pull/207) image split — get deleted.

## Consequences

- **Good:** removing Python/Graphify collapses the index Job's footprint — a right-sizeable Rust
  binary, no 4Gi ghost — and **dissolves the [#207](https://github.com/vymalo/lightbridge-code-intelligence/pull/207)
  image split**. With no Python anywhere, the `review` and `index` runners collapse into one lean
  binary, which is exactly what makes the single agent-plane binary
  ([ADR-0085](0085-agent-execution-plane.md)) viable.
- **Good:** graph extraction becomes a core competency we own — per-language tuning, resolution
  heuristics, and edge kinds are ours to improve, with no external release cadence or process boundary.
- **Good:** the tree is parsed once. Chunking, graph, and embedding-prep share one tree-sitter pass
  in one crate instead of a Rust chunker plus a spawned Python process re-parsing the same files.
- **Good:** PDFs in repos become searchable — a capability Graphify's code-only path
  (`file_type == "code"`) discards today.
- **Bad:** **parity/quality risk is real.** A hand-built extractor may resolve symbols or call edges
  less accurately than Graphify's mature 36-grammar engine, especially cross-file. We start behind on
  language coverage and must earn parity language-by-language (R1). This is the central cost of the
  decision.
- **Bad:** new attack surface — PDF parsing over untrusted repo input (R2). Bounded, but nonzero.
- **Neutral:** the built-in ignore defaults must stay conservative; an over-broad operator glob can
  silently hide real files, so skips are logged (R3).
- **Neutral:** language coverage is now *our* backlog. Graphify's 36 grammars set the bar; we ship the
  languages that matter first and accept a narrower initial set. The windowed-text fallback in
  `chunker.rs` already keeps unsupported languages *searchable* (semantically) even before they have a
  structural graph, so the degradation is graph-only, not index-wide.

### Risk register

| # | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| R1 | **Parity/quality:** in-house resolution produces fewer/wrong symbols or call edges vs. Graphify, degrading `graph_find_symbol`/`graph_get_callers` retrieval | High | High (silent review-quality regression — worse context, not a crash) | Golden tests on a fixture repo; **side-by-side `graph.json` diff against Graphify during migration**; migrate language-by-language and keep Graphify until a language is at parity; text-fallback keeps unsupported langs semantically searchable |
| R2 | PDF parsing on untrusted repo input crashes/OOMs the index Job or is exploited via a crafted PDF | Medium | Medium | **Cap input bytes read at the I/O level (a `MAX_FILE_BYTES`-style ceiling before the parser ever sees the file)**, then bound parse time + memory + output size per file; catch/skip on parse failure and log; runs in the index Job, which is the sandboxed one; pick a maintained crate (Unresolved) and treat it as untrusted-input code |
| R3 | Ignore-list misconfiguration hides real files from the index (operator glob too broad, or `.gitignore` composition wrong) | Medium | Medium | Conservative built-in defaults; **compose with, don't replace** repo `.gitignore`; log every skipped path/dir count so omissions are diagnosable, not silent |
| R4 | Language backlog: a language Graphify covered has no in-house grammar yet at cutover | Medium | Low | Don't delete Graphify until every actively-indexed language is at parity; text-fallback keeps such files in the semantic index meanwhile |
| R5 | Cross-file resolution is genuinely hard (build systems, re-exports, dynamic dispatch) and stalls | Medium | Medium | Ship intra-file edges first (already implied by the chunker's per-file symbols), then cross-file per language; measure against the Graphify baseline so "good enough" is data, not vibes |

## Alternatives considered

- **Option A — keep Graphify.** The status quo works and is mature, but the Python runtime, the ~4Gi
  footprint, the [#207](https://github.com/vymalo/lightbridge-code-intelligence/pull/207) image split,
  and the block on the single agent-plane binary are exactly the costs control-plane v2 exists to
  remove. Rejected.
- **Option B — a different external graph tool.** Same dependency class (separate runtime, image
  weight, spawn+marshal boundary), less control than the tool we already run. Rejected on mechanism —
  it solves none of the drivers.
- **Keep the graph external but the chunker in-house (today's split), permanently.** This *is* the
  current state; it keeps two toolchains parsing the same tree and keeps Python in the image. Rejected
  as a destination, not a step.

## Unresolved questions

- **Which Rust PDF-text crate** (`pdf-extract`, `lopdf`, or another): trade extraction fidelity
  against maintenance status and untrusted-input robustness. Decide with a small spike over a corpus
  of real repo PDFs, measuring crash rate and memory ceiling, not just extraction quality.
- **Initial language set** at Graphify cutover, and the parity bar (precision/recall thresholds on the
  fixture `graph.json` diff) that lets a language graduate off Graphify.
- **Cross-file resolution depth per language** — how far to chase re-exports, aliases, and build-graph
  boundaries before the marginal edge stops paying for itself.

## More Information

- [RFC-0007](../rfc/0007-control-plane-v2-planes.md) — control-plane v2 planes; this crate is the
  indexer that lets the agent-plane's `index` mode ship as one binary.
- [ADR-0085](0085-agent-execution-plane.md) — the agent execution plane that consumes `lci-codegraph`
  in `index` mode; the single-binary goal this ADR unblocks.
- [ADR-0083](0083-platform-crate-architecture-and-cratestack-data-layer.md) — the `lci-*` crate prefix
  and workspace architecture this crate joins.
- [RFC-0002](../rfc/0002-incremental-layered-indexing.md) — incremental/layered indexing; the
  extractor this crate replaces plugs into that machinery unchanged.
- What is being retired / reused: [ADR-0019](0019-graphify-cli-structural-graph.md) (Graphify),
  [ADR-0010](0010-graphify-treesitter-indexing-baseline.md) (the baseline),
  [`indexer/graph.rs`](../../services/agent-runner/src/indexer/graph.rs) (Graphify driver),
  [`indexer/chunker.rs`](../../services/agent-runner/src/indexer/chunker.rs) +
  [`indexer/mod.rs`](../../services/agent-runner/src/indexer/mod.rs) (the tree-sitter chunker + walk +
  ignore set this crate absorbs), [`agent-clients/src/embeddings.rs`](../../services/agent-clients/src/embeddings.rs)
  (the embeddings path it keeps feeding), and the consumers
  [`review-agent/src/tools/graph.rs`](../../services/review-agent/src/tools/graph.rs)
  (`graph_find_symbol` / `graph_get_callers`).
- The image split this dissolves: PR [#207](https://github.com/vymalo/lightbridge-code-intelligence/pull/207).
