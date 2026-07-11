use super::{ReviewServices, parse};
use lci_agent_tools::{
    BoxFuture, RegistryError, ReplaySafety, RuntimeCaps, Tool, ToolCx, ToolKind, ToolRegistry,
};
use lci_agent_types::{ToolCallReq, ToolOutcome, ToolSpec};
use serde::Deserialize;
use std::sync::Arc;

pub const ADD_COMMENT: &str = "add_comment";
#[derive(Deserialize)]
struct Args {
    body: String,
}
pub fn spec() -> ToolSpec {
    ToolSpec::function(
        ADD_COMMENT,
        "Post a plain reply on the thread (GitHub-flavored Markdown) — for answering a question or a general remark, not pinned to a diff line. Multiple calls are consolidated into one reply.",
        serde_json::json!({"type":"object","properties":{"body":{"type":"string","description":"Markdown reply body."}},"required":["body"]}),
    )
}
struct ReplyTool {
    spec: ToolSpec,
    services: ReviewServices,
}
pub(crate) fn register(
    registry: &mut ToolRegistry,
    services: &ReviewServices,
    caps: RuntimeCaps,
) -> Result<(), RegistryError> {
    registry.register(
        Arc::new(ReplyTool {
            spec: spec(),
            services: services.clone(),
        }),
        caps,
    )
}
impl Tool for ReplyTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Write
    }
    fn replay(&self) -> ReplaySafety {
        ReplaySafety::NeedsDedupKey
    }
    fn call<'a>(&'a self, cx: &'a ToolCx<'a>, call: &'a ToolCallReq) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            match parse::<Args>(&call.function.arguments) {
                Ok(args) => match self
                    .services
                    .client
                    .add_review_reply(cx.task_id, &args.body)
                    .await
                {
                    Ok(()) => ToolOutcome::Continue("comment recorded".into()),
                    Err(error) => {
                        ToolOutcome::Continue(format!("error: could not record comment: {error:#}"))
                    }
                },
                Err(error) => ToolOutcome::Continue(error),
            }
        })
    }
}
