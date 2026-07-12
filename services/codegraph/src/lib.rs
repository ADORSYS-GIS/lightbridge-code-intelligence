//! # lci-codegraph
//!
//! In-house code-graph crate (ADR-0086, RFC-0007 slice 1). It owns **chunking**, the structural
//! **graph**, and **embedding-prep** over one tree-sitter parse — the extractor the agent-plane's
//! `index` mode consumes, replacing the Python **Graphify** dependency language-by-language.
//!
//! This crate is a pure extractor: it parses a checkout and returns [`Chunk`]s and a [`Graph`] of
//! payload-compatible nodes/edges. The **host** maps those onto the internal-API payloads
//! (`agent-clients::ChunkPayload` / `GraphNodePayload` / `GraphEdgePayload`) and submits them, exactly
//! as `index_checkout` does today — the crate holds no `kube`/`sqlx`/forge dependencies (ADR-0083).
//!
//! ## What slice 1 delivers (additive; Graphify stays the default)
//! - [`ignore_list`] — gitignore-style, operator-configurable ignore layer that **composes with** the
//!   repo `.gitignore`, replacing the old hardcoded dir set.
//! - [`pdf`] — bounded PDF text extraction (byte-capped before parse, panic-caught).
//! - [`graph`] — the structural call/reference graph with **cross-file resolution for Rust**.
//! - [`walk`] — the one-pass walk producing chunks + graph, honouring both ignore layers.
//! - a parity-harness scaffold (`tests/parity.rs`) that snapshots the graph against a golden.

pub mod chunk;
pub mod graph;
pub mod ignore_list;
pub mod language;
pub mod pdf;
pub mod ts;
pub mod tuning;
pub mod walk;

pub use chunk::{Chunk, chunk_file, chunk_text};
pub use graph::{Graph, GraphEdge, GraphNode};
pub use ignore_list::{DEFAULT_IGNORE_GLOBS, IgnoreConfig, IgnoreList};
pub use pdf::{PdfOutcome, extract_from_path as extract_pdf_from_path};
pub use tuning::IndexTuning;
pub use walk::{WalkOptions, WalkOutput, WalkStats, walk_checkout, walk_checkout_from_env};
