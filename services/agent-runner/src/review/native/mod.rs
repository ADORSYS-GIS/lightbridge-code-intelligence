//! Native Rust review agent (ADR-0026 + ADR-0037) — the only review path (#140 removed OpenCode).
//!
//! The runner drives its own agent loop against the eaig OpenAI-compatible **Chat Completions**
//! endpoint with function calling. The agent investigates with retrieval tools and **acts via mediated
//! write tools** (`add_review_comment` / `add_comment` / `finish`); the control plane buffers those and
//! flushes one grouped review on finalize, so results come from validated tool calls rather than
//! scraped free text. This module holds:
//!
//! - [`chat`] — the Chat Completions client (request/response types + tool-call protocol).
//! - [`tools`] — the agent's tool surface (retrieval + mediated write actions) and the dispatcher.
//! - [`agent`] — the loop ([`agent::run_native_agent`]).

pub mod agent;
pub mod chat;
pub mod diff;
pub mod tools;
