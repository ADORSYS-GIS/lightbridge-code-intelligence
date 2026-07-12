//! Lightbridge agent runner.
//!
//! The per-task Kubernetes Job the dispatcher launches (ADR-0004). It holds no GitHub App key: it
//! reads its task id + control-plane callback wiring from env, fetches the task context (repo
//! coordinates + a short-lived installation token) from the control plane, clones the repo at the
//! relevant commit, runs the task, and reports a terminal status back. The control plane owns the
//! trust boundary — it mints the token and validates findings + writes to GitHub (ADR-0002, ADR-0022).
//!
//! The lifecycle: clone → semantic index (tree-sitter → pgvector, slice 2) → structural index
//! (Graphify → Neo4j, slice 3) → review (the native agent loop, ADR-0026/0037, which acts via mediated
//! write tools the control plane flushes) → report. Indexing is required; the structural graph and the
//! review are best-effort and non-fatal.
//!
//! This binary is the historical entrypoint. The orchestration now lives in [`agent_runner::run_once`]
//! (the ADR-0085 `run-once` host), shared with the new `agent-plane` binary (`bin/agent_plane.rs`).
//! This binary passes `None` for the mode, so the runner infers index-vs-review from the task exactly
//! as it always has — its behaviour is byte-identical to before the agent-plane split.

// Global allocator — static-musl images (ADR-0080). The runner is allocation-heavy (clone walk,
// tree-sitter parse, embedding batches); mimalloc avoids musl malloc's multithreaded regression.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    // `None` mode → infer index-vs-review from the task's `command`, today's behaviour unchanged.
    agent_runner::run_once(None).await
}
