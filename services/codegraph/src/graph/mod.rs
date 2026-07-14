//! Structural code graph (ADR-0086 §1). Built directly on tree-sitter — the same grammars the chunker
//! uses — as **edges on top of the per-file symbols** (definitions + their call sites).
//!
//! Output is payload-compatible with the graph payload the retired Graphify `graph.json` parse
//! produced (schema history, ADR-0019 → ADR-0086): [`GraphNode`] mirrors
//! `agent-clients::GraphNodePayload` (`node_id`, `label`, `source_file`, `start_line`) and
//! [`GraphEdge`] mirrors `GraphEdgePayload` (`source`, `target`, `relation`). The control plane's
//! generic `:Symbol` + `[:REL {relation}]` Neo4j write and the `graph_find_symbol` /
//! `graph_get_callers` retrieval tools are unchanged behind the seam.
//!
//! Languages with a real extractor today: **Rust** (ADR-0086 "Rust language first"), **Python**,
//! **TypeScript/JavaScript** (incl. JSX/TSX), and **Java**. Every language emits the SAME node/edge
//! vocabulary — the cross-file resolver ([`resolve::resolve`]/[`resolve::pick`]) is language-agnostic
//! and shared verbatim. Definitions and call references are identified by each grammar's bundled
//! `tags.scm` query ([`crate::tags`]); Rust keeps its own node-kind extractor for a byte-stable
//! golden. See [`emit::Classifier`]. Relations emitted:
//! - `contains` — file → top-level def, and container def (mod/struct/trait/enum/class) → nested def.
//! - `method` — a type container (impl/trait/struct/enum/class/interface) → a callable it defines.
//!   This is a specialisation of `contains` kept separate for parity with Graphify (which emits
//!   `method`).
//! - `calls` — caller def → callee def, with **cross-file** resolution (a call in file A resolved to a
//!   definition in file B). `graph_get_callers` traverses this relation, so the name must match.
//!
//! Line numbers are **1-based** and callable labels carry a `()` suffix (`add` → `add()`), both for
//! parity with the Graphify `graph.json` schema this crate replaced.
//!
//! ## Module layout
//! The module is split by concern, each a single-responsibility slice of the pipeline:
//! - [`emit`] — the tree-sitter DFS that emits def nodes + `contains`/`method` edges and records call
//!   sites ([`emit::extract_file`], the language [`emit::Classifier`] dispatch).
//! - [`callee`] — parses a call site's callee reference (bare name + optional type qualifier) out of
//!   the AST, for both the Rust `call_expression` navigation and the tags-captured callee name node.
//! - [`resolve`] — the cross-file name resolver ([`resolve::resolve`]/[`resolve::pick`]) that turns
//!   recorded call sites into `calls` edges.

use serde::Serialize;

mod callee;
mod emit;
mod resolve;
#[cfg(test)]
mod tests;

pub use emit::extract_file;
pub use resolve::resolve;

/// One graph node. Field set mirrors `agent-clients::GraphNodePayload` exactly.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphNode {
    pub node_id: String,
    pub label: String,
    pub source_file: String,
    pub start_line: i64,
}

/// One directed edge. Field set mirrors `agent-clients::GraphEdgePayload` exactly.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphEdge {
    pub source: String,
    pub target: String,
    pub relation: String,
}

/// A resolved structural graph, canonicalised (nodes + edges sorted, deduped) so it is stable to
/// snapshot as a golden and stable to submit.
#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
pub struct Graph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

/// The per-file structural facts: the file's nodes (file node + defs), its intra-file `contains` /
/// `method` edges, and the unresolved call sites for the cross-file pass.
#[derive(Debug, Default)]
pub struct FileSymbols {
    nodes: Vec<GraphNode>,
    contains: Vec<GraphEdge>,
    /// Call sites attributed to their enclosing caller, resolved to `calls` edges once every file is
    /// known.
    calls: Vec<CallSite>,
    /// Callable defs (functions/methods) in this file, for same-file-first resolution.
    callables: Vec<Callable>,
}

/// A callable definition (`function`/`method`) recorded for call resolution.
#[derive(Debug, Clone)]
struct Callable {
    name: String,
    node_id: String,
    /// Enclosing type/class name (Rust `impl S` → `S`; a Python/TS/Java `class C` → `C`), used
    /// **only** as a tiebreaker to disambiguate several same-named callables. `None` for free
    /// functions.
    scope: Option<String>,
}

/// An unresolved call site: the enclosing caller def id, the bare callee name, and — when the call is
/// qualified by a receiver/path (`A::new`, `Foo.bar`) — that qualifier, used only to break same-name
/// ambiguity.
#[derive(Debug, Clone)]
struct CallSite {
    caller: String,
    name: String,
    qualifier: Option<String>,
}
