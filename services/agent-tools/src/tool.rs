//! The executable tool contract: what a tool is, how it is classified, and the replay
//! guarantee it demands from a host before it may be registered.

use std::future::Future;
use std::pin::Pin;

use lci_agent_types::{ToolCallReq, ToolOutcome, ToolSpec};

use crate::ToolCx;

/// The boxed future used only at the heterogeneous tool/workspace boundaries.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One executable tool. Dynamic dispatch is intentional: a registry is heterogeneous.
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;
    fn kind(&self) -> ToolKind;
    fn replay(&self) -> ReplaySafety;
    fn call<'a>(&'a self, cx: &'a ToolCx<'a>, call: &'a ToolCallReq) -> BoxFuture<'a, ToolOutcome>;
}

/// Classification used by budgets and per-turn offered-set policies.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ToolKind {
    ReadOnly(ReadKind),
    Write,
    Terminal,
    Progress,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ReadKind {
    Retrieval,
    File,
    Knowledge,
}

/// The replay guarantee a host must honor before registering a tool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplaySafety {
    ReadOnly,
    Idempotent,
    NeedsDedupKey,
}

/// Capabilities supplied by the runtime hosting a registry.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuntimeCaps {
    /// Whether completed effects may be replayed by this host.
    pub replays_completed_steps: bool,
    pub per_call_dedup: bool,
}
