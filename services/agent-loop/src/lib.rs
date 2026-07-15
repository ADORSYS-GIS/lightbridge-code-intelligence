//! Runtime-independent agent loop, policy composition, and transcript seam.
//!
//! Module layout:
//! - [`chat`]: wire types for the model boundary (`ChatMessage`, `ChatRequest`, `ModelClient`,
//!   `Conversation`).
//! - [`transcript`]: loop outcome and transcript recording (`LoopOutcome`, `TranscriptEvent`,
//!   `TranscriptSink`).
//! - [`turn`]: the per-turn state and `TurnPolicy` contract that policy implementations satisfy.
//! - [`budget`]: pure context-budget and wind-down arithmetic shared by the driver and policies.
//! - [`loop_driver`]: the turn-taking loop driver (`AgentLoop`, `LoopLimits`).
//! - [`policy`]: built-in `TurnPolicy` implementations (context trim, wind-down, read/turn
//!   budgets).

#![allow(async_fn_in_trait)] // Native AFIT keeps the single model implementation statically dispatched.

mod budget;
mod chat;
mod loop_driver;
pub mod policy;
mod transcript;
mod turn;

pub use budget::{convergence_filter, estimate_tokens, trim_tool_history, winddown_turn};
pub use chat::{
    ChatMessage, ChatRequest, Conversation, ModelClient, RequestOptions, StreamOptions,
};
pub use loop_driver::{AgentLoop, LoopLimits, RefusalRenderer};
pub use transcript::{LoopOutcome, TranscriptEvent, TranscriptSink};
pub use turn::{
    LoopStats, Nudge, PolicyAction, ToolCallResult, TurnOutcome, TurnPolicy, TurnState,
};
