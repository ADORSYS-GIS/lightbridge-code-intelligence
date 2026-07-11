//! Code review — the native in-process agent (ADR-0026 + ADR-0037).
//!
//! The runner drives its own agent loop against the eaig OpenAI-compatible **Chat Completions**
//! endpoint with function calling. The agent investigates with retrieval tools and **acts via mediated
//! write tools** (`add_review_comment` / `add_comment` / `finish`); the control plane buffers those and
//! flushes one grouped review on finalize (ADR-0037). The former OpenCode subprocess + the stdio MCP
//! servers it spawned were removed in #140 — this is the only review path.

pub mod instructions;
pub mod native;

pub use native::agent::{ReviewOutcome, run_native_agent};
