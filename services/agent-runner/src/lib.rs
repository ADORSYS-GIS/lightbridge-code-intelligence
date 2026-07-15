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
//!   and acts via mediated write tools the control plane flushes as one grouped review. SAST
//!   (`lci-agent-sast`, ADR-0061) is one such tool (`run_sast`, ADR-0073) — opengrep runs only when the
//!   agent calls it, and its findings ride the same review buffer.
//! - [`plane`] — the agent-plane `mode × host` matrix + routing guard (ADR-0085): pure data both
//!   binaries select on.
//! - [`run`] — the `run-once` host: [`run_once`] walks the pipeline above once and exits. Both the
//!   `agent-runner` and `agent-plane` binaries are thin shells over it.

pub mod bootstrap;
pub mod clone;
pub mod indexer;
pub mod plane;
pub mod review;
pub mod run;

// The `run-once` host entrypoint (ADR-0085), shared by the `agent-runner` and `agent-plane` binaries.
pub use run::run_once;
