//! OpenCode review host — the parity-critical core (RFC-0009 / ADR-0094/0095 review cutover, slice 3).
//!
//! The native review path drives `AgentLoop`, which owns both the model call and tool dispatch and
//! interleaves the review policies per turn ([`crate::flows::run_review`]). When review runs on
//! OpenCode instead, **OpenCode owns its own loop**: one `session/prompt` runs its entire internal
//! agent cycle (many model round-trips, many tool calls) and returns once. The supervisor only
//! *observes* and *re-drives*. That splits the native policies into two classes:
//!
//! - **Loop mechanics** (context-trim, wind-down, read/turn budgets — ADR-0042 batching): handed to
//!   OpenCode's own maintained loop. The supervisor cannot narrow OpenCode's tools mid-internal-loop.
//! - **Review-quality gates** ([`CoverageGate`](crate::policies::CoverageGate) /
//!   [`RefuteGate`](crate::policies::RefuteGate)): kept as Rust and run here, reusing the exact tuned
//!   `TurnPolicy` code — no TypeScript reimplementation of the coverage denominator, citation
//!   crediting, or the ADR-0091 absence-claim directive.
//!
//! Split by responsibility (SRP-by-file), all host-independent and offline-testable:
//! - [`config`] — render the per-task OpenCode config (mediated stdio MCP, built-in file tools
//!   disabled for coverage parity, read-only posture, the dynamic reviewer prompt embedded).
//! - [`recorder`] — reconstruct a review `TurnOutcome` from the OpenCode recorder JSONL (ADR-0095),
//!   the in-process completeness authority that sees every tool call, *including subagent-internal
//!   ones the ACP client is never shown* (so coverage counts an `explore` subagent's `read_file`s).
//! - [`gates`] — drive the reused coverage/refute quality gates over each observed cycle.
//! - [`driver`] — the pure re-prompt / finalize control loop over those gates.
//! - [`transcript`] — reconstruct the ADR-0034 run transcript from the same recorder JSONL.
//!
//! The async transport (spawning OpenCode, tailing the recorder, sending `session/prompt`) is the
//! host's job and layers over this core. Public paths are re-exported so callers use
//! `lci_review_agent::opencode::{…}` unchanged.

mod config;
mod driver;
mod gates;
mod recorder;
mod transcript;

#[cfg(test)]
mod test_support;

pub use config::render_review_config;
pub use driver::{DriveAction, ReviewDriver, ReviewResolution};
pub use gates::{GateDecision, ReviewGates};
pub use recorder::{
    KNOWN_REVIEW_TOOLS, RecorderEvent, cycle_turn_outcome, normalize_tool_name, parse_recorder,
};
pub use transcript::transcript_from_recorder;
