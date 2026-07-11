use std::sync::Arc;

use lci_agent_tools::{
    BoxFuture, RegistryError, ReplaySafety, RuntimeCaps, Tool, ToolCx, ToolKind, ToolRegistry,
};
use lci_agent_types::{ToolCallReq, ToolOutcome, ToolSpec};
use serde::Deserialize;

use super::{ReviewServices, parse};

pub const ADD_REVIEW_COMMENT: &str = "add_review_comment";
pub const RETRACT_FINDING: &str = "retract_finding";

#[derive(Deserialize)]
struct AddArgs {
    file: String,
    line: i32,
    #[serde(default)]
    start_line: Option<i32>,
    title: String,
    priority: String,
    category: String,
    body: String,
    #[serde(default)]
    evidence: Option<String>,
    #[serde(default)]
    suggestion: Option<String>,
}

#[derive(Deserialize)]
struct RetractArgs {
    file: String,
    line: i32,
    #[serde(default)]
    reason: Option<String>,
}

pub fn specs() -> [ToolSpec; 2] {
    [
        ToolSpec::function(
            ADD_REVIEW_COMMENT,
            "Record one inline review finding on a line the diff adds or changes. Call once per finding as you go; nothing posts until `finish`. Re-recording the same (file, line) refines it.",
            serde_json::json!({"type":"object","properties":{"file":{"type":"string","description":"Path from repo root."},"line":{"type":"integer","description":"The line this finding anchors to — a line the diff adds or changes. When `start_line` is also given, this is the LAST line of the multi-line range."},"start_line":{"type":"integer","description":"Optional. The FIRST line of a multi-line range; the range ends at `line`. Omit for a single-line finding. Leave unset unless the finding's evidence genuinely spans multiple contiguous lines."},"title":{"type":"string","description":"Short (≤ ~8 words)."},"priority":{"type":"string","enum":["P0","P1","P2"],"description":"P0 = must fix (bug/security/data-loss), P1 = should fix, P2 = minor/nit."},"category":{"type":"string","enum":["security","correctness","quality","style","performance"],"description":"The dimension this finding is about."},"body":{"type":"string","description":"Why it matters."},"evidence":{"type":"string","description":"REQUIRED: the concrete proof — the exact lines / symbol this finding rests on, so it can be verified. If you can't cite it, don't record the finding."},"suggestion":{"type":"string","description":"Optional exact replacement source for `line` (no diff markers)."}},"required":["file","line","title","priority","category","body"]}),
        ),
        ToolSpec::function(
            RETRACT_FINDING,
            "Drop a finding you previously recorded that did NOT survive verification (its claim doesn't hold against the cited evidence). Use during your pre-finish review of your own P0/P1 findings — a wrong finding costs more trust than a missed one.",
            serde_json::json!({"type":"object","properties":{"file":{"type":"string","description":"The finding's file (as recorded)."},"line":{"type":"integer","description":"The finding's line (as recorded)."},"reason":{"type":"string","description":"Why it didn't hold (optional)."}},"required":["file","line"]}),
        ),
    ]
}

struct AddTool {
    spec: ToolSpec,
    services: ReviewServices,
}
struct RetractTool {
    spec: ToolSpec,
    services: ReviewServices,
}

pub(crate) fn register(
    registry: &mut ToolRegistry,
    services: &ReviewServices,
    caps: RuntimeCaps,
) -> Result<(), RegistryError> {
    let [add, retract] = specs();
    registry.register(
        Arc::new(AddTool {
            spec: add,
            services: services.clone(),
        }),
        caps,
    )?;
    registry.register(
        Arc::new(RetractTool {
            spec: retract,
            services: services.clone(),
        }),
        caps,
    )
}

impl Tool for AddTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Write
    }
    fn replay(&self) -> ReplaySafety {
        ReplaySafety::Idempotent
    }
    fn call<'a>(&'a self, cx: &'a ToolCx<'a>, call: &'a ToolCallReq) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let args = match parse::<AddArgs>(&call.function.arguments) {
                Ok(args) => args,
                Err(error) => {
                    return ToolOutcome::Continue(format!(
                        "{error} Expected JSON like {{\"file\": \"path\", \"line\": 42, \"title\": \"…\", \"priority\": \"P0\", \"category\": \"security\", \"body\": \"…\", \"evidence\": \"the lines this rests on\", \"suggestion\": \"optional\", \"start_line\": 40}}. priority is P0|P1|P2; category is security|correctness|quality|style|performance; start_line is optional — the integer first line of a multi-line range (omit for a single-line finding)."
                    ));
                }
            };
            let body = match args
                .evidence
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                Some(evidence) => format!("{}\n\n**Evidence:** {evidence}", args.body.trim_end()),
                None => args.body.clone(),
            };
            match self
                .services
                .client
                .add_review_comment(
                    cx.task_id,
                    &args.file,
                    args.line,
                    args.start_line,
                    Some(&args.title),
                    Some(&args.priority),
                    Some(&args.category),
                    args.suggestion.as_deref(),
                    &body,
                )
                .await
            {
                Ok(()) => ToolOutcome::Continue(format!(
                    "recorded finding at {}:{}",
                    args.file, args.line
                )),
                Err(error) => {
                    ToolOutcome::Continue(format!("error: could not record finding: {error:#}"))
                }
            }
        })
    }
}

impl Tool for RetractTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Write
    }
    fn replay(&self) -> ReplaySafety {
        ReplaySafety::Idempotent
    }
    fn call<'a>(&'a self, cx: &'a ToolCx<'a>, call: &'a ToolCallReq) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            match parse::<RetractArgs>(&call.function.arguments) {
                Ok(args) => match self
                    .services
                    .client
                    .retract_finding(cx.task_id, &args.file, args.line)
                    .await
                {
                    Ok(()) => ToolOutcome::Continue(format!(
                        "retracted finding at {}:{}{}",
                        args.file,
                        args.line,
                        args.reason
                            .as_deref()
                            .map(|reason| format!(" ({reason})"))
                            .unwrap_or_default()
                    )),
                    Err(error) => ToolOutcome::Continue(format!(
                        "error: could not retract finding: {error:#}"
                    )),
                },
                Err(error) => ToolOutcome::Continue(format!(
                    "{error} Expected JSON like {{\"file\": \"path\", \"line\": 42, \"reason\": \"optional\"}}."
                )),
            }
        })
    }
}
