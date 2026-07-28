//! `abort` — the clean give-up terminal tool. Proposes nothing; leaves the sandbox to be wiped.

use std::sync::Arc;

use lci_agent_tools::{
    BoxFuture, RegistryError, ReplaySafety, RuntimeCaps, Tool, ToolCx, ToolKind, ToolRegistry,
};
use lci_agent_types::{ToolCallReq, ToolOutcome, ToolSpec};
use serde::Deserialize;

use super::parse;

pub const ABORT: &str = "abort";

#[derive(Deserialize)]
struct Args {
    reason: String,
}

pub fn abort_spec() -> ToolSpec {
    ToolSpec::function(
        ABORT,
        "Give up cleanly when you cannot produce a useful pull request (e.g. the ticket is \
         underspecified or the change is out of scope). Proposes nothing; the sandbox is discarded.",
        serde_json::json!({"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"]}),
    )
}

struct AbortTool {
    spec: ToolSpec,
}

pub(crate) fn register(
    registry: &mut ToolRegistry,
    caps: RuntimeCaps,
) -> Result<(), RegistryError> {
    registry.register(Arc::new(AbortTool { spec: abort_spec() }), caps)
}

impl Tool for AbortTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Terminal
    }
    fn replay(&self) -> ReplaySafety {
        ReplaySafety::Idempotent
    }
    fn call<'a>(&'a self, _: &'a ToolCx<'a>, call: &'a ToolCallReq) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            match parse::<Args>(&call.function.arguments) {
                Ok(args) => ToolOutcome::Abort(args.reason),
                Err(error) => ToolOutcome::Continue(error.to_string()),
            }
        })
    }
}
