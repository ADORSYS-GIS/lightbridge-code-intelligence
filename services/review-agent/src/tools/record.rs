use std::sync::Arc;

use lci_agent_tools::{
    BoxFuture, RegistryError, ReplaySafety, RuntimeCaps, Tool, ToolCx, ToolKind, ToolRegistry,
};
use lci_agent_types::{ToolCallReq, ToolOutcome, ToolSpec};
use serde::Deserialize;

use super::{ReviewServices, parse};

pub const ADD_REVIEW_COMMENT: &str = "add_review_comment";
pub const RETRACT_FINDING: &str = "retract_finding";

/// Rank a `P0`/`P1`/`P2` priority string, most → least severe (`P0` = 0). An unrecognized value ranks
/// below `P2` — permissive on a malformed value rather than filtering it out by mistake.
fn priority_rank(priority: &str) -> u8 {
    match priority {
        "P0" => 0,
        "P1" => 1,
        "P2" => 2,
        _ => 3,
    }
}

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

/// The `priority` field's description (below) is the only severity rubric anchor that ships with
/// every deployment regardless of the operator's own `config.reviewSystemPrompt` (ai-helm-values) —
/// #285/#421 found no other in-repo text anchors P0/P1/P2 at all. It's impact-anchored ("decide by
/// IMPACT IF REAL, not by how confident you feel"), matching the operator-side calibration already
/// shipped for the same reason (ai-helm-values#53 — severity must not become a confidence dial a model
/// can hide behind), and it treats P1 as blocking (not merely "should fix") to match how this product
/// actually uses it (epic #252's own trust contract: "P1 = fix before merge, P2 = nit").
///
/// This wording is a hypothesis, not a proven fix — #421 explicitly blocks the actual claim of
/// improvement on a before/after measurement from the #420 variance harness; see that ticket.
pub fn specs() -> [ToolSpec; 2] {
    [
        ToolSpec::function(
            ADD_REVIEW_COMMENT,
            "Record one inline review finding on a line the diff adds or changes. Call once per finding as you go; nothing posts until `finish`. Re-recording the same (file, line) refines it.",
            serde_json::json!({"type":"object","properties":{"file":{"type":"string","description":"Path from repo root."},"line":{"type":"integer","description":"The line this finding anchors to — a line the diff adds or changes. When `start_line` is also given, this is the LAST line of the multi-line range."},"start_line":{"type":"integer","description":"Optional. The FIRST line of a multi-line range; the range ends at `line`. Omit for a single-line finding. Leave unset unless the finding's evidence genuinely spans multiple contiguous lines."},"title":{"type":"string","description":"Short (≤ ~8 words)."},"priority":{"type":"string","enum":["P0","P1","P2"],"description":"P0 = blocking — security/data-loss: exploitable without special access, or causes data/state loss on a realistic path. P1 = blocking — a real defect: demonstrably wrong on a realistic input or precondition, even if that precondition is rare; decide by IMPACT IF REAL, not by how confident you feel. P2 = non-blocking: cosmetic, needs an already-broken precondition, or a claim you cannot verify from the diff alone. Never downgrade a defect you can prove to P2 to dodge scrutiny — a wrong severity costs more trust than a missed one."},"category":{"type":"string","enum":["security","correctness","quality","style","performance"],"description":"The dimension this finding is about."},"body":{"type":"string","description":"Why it matters."},"evidence":{"type":"string","description":"REQUIRED: the concrete proof — the exact lines / symbol this finding rests on, so it can be verified. If you can't cite it, don't record the finding."},"suggestion":{"type":"string","description":"Optional exact replacement source for `line` (no diff markers)."}},"required":["file","line","title","priority","category","body"]}),
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
            // Repo `severity.min` (ADR-0030): a finding below the repo's configured floor is never
            // sent to the control plane at all — the model still gets an acknowledgement (so it doesn't
            // retry the call), just not one that claims the finding was recorded.
            if let Some(min) = self.services.min_priority.as_deref()
                && priority_rank(&args.priority) > priority_rank(min)
            {
                return ToolOutcome::Continue(format!(
                    "not recorded: {} is below this repo's configured minimum severity ({min}) — {}:{}",
                    args.priority, args.file, args.line
                ));
            }
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

#[cfg(test)]
mod tests {
    use lci_agent_clients::{ControlPlaneClient, EmbeddingsClient};
    use lci_agent_types::FunctionCallReq;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::tools::EagerWorkspace;

    fn call(priority: &str) -> ToolCallReq {
        ToolCallReq {
            id: "c1".into(),
            kind: "function".into(),
            function: FunctionCallReq {
                name: ADD_REVIEW_COMMENT.into(),
                arguments: serde_json::json!({
                    "file": "a.rs", "line": 2, "title": "t", "priority": priority,
                    "category": "quality", "body": "b", "evidence": "line 2",
                })
                .to_string(),
            },
            extra_content: None,
        }
    }

    async fn tool(cp: &MockServer, min_priority: Option<&str>) -> AddTool {
        AddTool {
            spec: specs()[0].clone(),
            services: ReviewServices {
                client: Arc::new(ControlPlaneClient::new(cp.uri(), "tok")),
                embedder: Arc::new(EmbeddingsClient::new("http://unused", "key", "model")),
                min_priority: min_priority.map(str::to_string),
            },
        }
    }

    // ADR-0030: a repo's `severity.min` is genuinely enforced — a below-threshold finding is never
    // POSTed to the control plane at all (not just labelled/downgraded).
    #[tokio::test]
    async fn a_finding_below_the_repo_minimum_is_never_sent_to_the_control_plane() {
        let cp = MockServer::start().await;
        // No mock mounted for /review/inline — if the tool posts anyway, wiremock 404s (proving a bug).
        let workspace = EagerWorkspace::new(std::path::PathBuf::from("/tmp"));
        let cx = ToolCx {
            task_id: uuid::Uuid::nil(),
            workspace: &workspace,
        };
        let add = tool(&cp, Some("P1")).await;
        let outcome = add.call(&cx, &call("P2")).await;
        let ToolOutcome::Continue(msg) = outcome else {
            panic!("expected Continue");
        };
        assert!(
            msg.contains("not recorded") && msg.contains("P2") && msg.contains("P1"),
            "acknowledges the filter without claiming it was recorded: {msg}"
        );
    }

    // The same finding, with no repo minimum configured, IS sent — proving the filter (not something
    // else) is what suppressed it above.
    #[tokio::test]
    async fn with_no_minimum_configured_every_priority_is_sent() {
        let cp = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!(
                "/internal/tasks/{}/review/inline",
                uuid::Uuid::nil()
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(&cp)
            .await;
        let workspace = EagerWorkspace::new(std::path::PathBuf::from("/tmp"));
        let cx = ToolCx {
            task_id: uuid::Uuid::nil(),
            workspace: &workspace,
        };
        let add = tool(&cp, None).await;
        let outcome = add.call(&cx, &call("P2")).await;
        assert_eq!(
            outcome,
            ToolOutcome::Continue("recorded finding at a.rs:2".to_string())
        );
    }

    // A finding AT or ABOVE the minimum still records normally.
    #[tokio::test]
    async fn a_finding_at_the_repo_minimum_is_still_recorded() {
        let cp = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!(
                "/internal/tasks/{}/review/inline",
                uuid::Uuid::nil()
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(&cp)
            .await;
        let workspace = EagerWorkspace::new(std::path::PathBuf::from("/tmp"));
        let cx = ToolCx {
            task_id: uuid::Uuid::nil(),
            workspace: &workspace,
        };
        let add = tool(&cp, Some("P1")).await;
        let outcome = add.call(&cx, &call("P1")).await;
        assert_eq!(
            outcome,
            ToolOutcome::Continue("recorded finding at a.rs:2".to_string())
        );
    }
}
