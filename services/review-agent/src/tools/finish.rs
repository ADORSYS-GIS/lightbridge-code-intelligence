use super::{ReviewServices, parse};
use lci_agent_tools::{
    BoxFuture, RegistryError, ReplaySafety, RuntimeCaps, Tool, ToolCx, ToolKind, ToolRegistry,
};
use lci_agent_types::{ToolCallReq, ToolOutcome, ToolSpec};
use serde::Deserialize;
use std::sync::Arc;

pub const FINISH: &str = "finish";
pub const REPORT_PROGRESS: &str = "report_progress";
pub const ABORT: &str = "abort";

#[derive(Deserialize)]
struct FinishArgs {
    summary: String,
}
#[derive(Deserialize)]
struct NoteArgs {
    note: String,
}
#[derive(Deserialize)]
struct AbortArgs {
    reason: String,
}

pub fn finish_spec() -> ToolSpec {
    ToolSpec::function(
        FINISH,
        "Finish the run: record your overall verdict/summary and post everything you buffered. Call exactly once when done — investigate and record findings/replies first.",
        serde_json::json!({"type":"object","properties":{"summary":{"type":"string","description":"1–3 sentence overall verdict: does the change do what it intends, and is it correct and safe?"}},"required":["summary"]}),
    )
}
pub fn aux_specs() -> [ToolSpec; 2] {
    [
        ToolSpec::function(
            REPORT_PROGRESS,
            "Optionally report a short progress note for observability. Does not affect the result.",
            serde_json::json!({"type":"object","properties":{"note":{"type":"string"}},"required":["note"]}),
        ),
        ToolSpec::function(
            ABORT,
            "Abort when you cannot produce a useful result (e.g. the diff is unreadable). Recorded as a clean abort, not a crash. Any findings/replies you buffered are discarded — but `reason` itself is posted verbatim as the public review note on the PR, so it must be a plain sentence for the PR author, never an internal note or scratch reasoning.",
            serde_json::json!({"type":"object","properties":{"reason":{"type":"string","description":"One honest sentence explaining why, written for the PR author to read — this is posted publicly as the review body, not a private note."}},"required":["reason"]}),
        ),
    ]
}

struct FinishTool {
    spec: ToolSpec,
    services: ReviewServices,
}
struct ProgressTool {
    spec: ToolSpec,
}
struct AbortTool {
    spec: ToolSpec,
}
pub(crate) fn register(
    registry: &mut ToolRegistry,
    services: &ReviewServices,
    caps: RuntimeCaps,
) -> Result<(), RegistryError> {
    let [progress, abort] = aux_specs();
    registry.register(
        Arc::new(FinishTool {
            spec: finish_spec(),
            services: services.clone(),
        }),
        caps,
    )?;
    registry.register(Arc::new(ProgressTool { spec: progress }), caps)?;
    registry.register(Arc::new(AbortTool { spec: abort }), caps)
}
impl Tool for FinishTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Terminal
    }
    fn replay(&self) -> ReplaySafety {
        ReplaySafety::Idempotent
    }
    fn call<'a>(&'a self, cx: &'a ToolCx<'a>, call: &'a ToolCallReq) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            match parse::<FinishArgs>(&call.function.arguments) {
                Ok(args) => match self
                    .services
                    .client
                    .set_review_summary(cx.task_id, &args.summary)
                    .await
                {
                    Ok(()) => ToolOutcome::Finish,
                    Err(error) => ToolOutcome::Continue(format!(
                        "error: could not record the summary: {error:#}. Call `finish` again."
                    )),
                },
                Err(error) => ToolOutcome::Continue(format!(
                    "{error} Expected JSON like {{\"summary\": \"…your overall verdict…\"}}."
                )),
            }
        })
    }
}
impl Tool for ProgressTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Progress
    }
    fn replay(&self) -> ReplaySafety {
        ReplaySafety::Idempotent
    }
    fn call<'a>(&'a self, _: &'a ToolCx<'a>, call: &'a ToolCallReq) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            match parse::<NoteArgs>(&call.function.arguments) {
                Ok(args) => {
                    tracing::info!(note=%args.note,"review agent progress");
                    ToolOutcome::Continue("acknowledged".into())
                }
                Err(error) => ToolOutcome::Continue(error),
            }
        })
    }
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
            match parse::<AbortArgs>(&call.function.arguments) {
                Ok(args) => ToolOutcome::Abort(args.reason),
                Err(error) => ToolOutcome::Continue(error),
            }
        })
    }
}
