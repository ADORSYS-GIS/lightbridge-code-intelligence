//! Library surface of the agent runner, so integration tests (and any future in-process reuse) can
//! exercise the modules. The `agent-runner` binary (`main.rs`) is a thin orchestrator over these.
//!
//! # Module map
//!
//! Modules follow the per-task pipeline (`main.rs::run()` walks them in order):
//!
//! - [`bootstrap`] — load config ([`config`](bootstrap::config)); the shared `lci-agent-clients`
//!   crate talks to the control plane and is the only thing holding the runner bearer.
//! - [`clone`] — checkout the repo at the head SHA using the borrowed install token.
//! - [`indexer`] — tree-sitter chunking + structural-graph extraction, with the shared embeddings
//!   client (OpenAI-compatible vectors → control plane) feeding the semantic index.
//! - [`review`] — the native review agent loop (ADR-0026/0037): it investigates with retrieval tools
//!   and acts via mediated write tools the control plane flushes as one grouped review.
//! - [`sast`] — a deterministic opengrep pass over the PR's changed files (ADR-0061), whose findings
//!   ride the same review buffer; the agent is made aware of them but never gates them.

pub mod bootstrap;
pub mod clone;
pub mod indexer;
pub mod review;
pub mod sast;
