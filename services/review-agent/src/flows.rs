//! The review flow: compose the review policies in their exact order, run the engine loop, and flush
//! the post-loop coverage disclosure. This is the review-specific assembly the host (and the golden
//! parity test) drive — behaviour-preserving with the legacy `run_native_agent` loop.
//!
//! The host injects the runtime, model, sink, tool registry, seeded conversation, and the numeric
//! [`ReviewRunParams`]; this module owns the policy vector, the [`LoopLimits`], the FAST-tier turn cap,
//! and the coverage flush — nothing that couples it back to the runner's config.

use std::path::PathBuf;

use lci_agent_clients::ControlPlaneClient;
use lci_agent_loop::policy::{ContextWindowTrim, ReadBudgets, TurnBudget, WindDown};
use lci_agent_loop::{
    AgentLoop, Conversation, LoopLimits, LoopOutcome, ModelClient, TranscriptSink, TurnPolicy,
};
use lci_agent_step::StepRuntime;
use lci_agent_tools::{ToolCx, ToolRegistry, TurnFilter};

use crate::policies::{
    CoverageGate, FastTierGuard, FindingFinishNudge, RefuteGate, SastAnchorGate, SastLead,
    ScratchpadLoopGuard, render_fast_refusal,
};
pub use crate::tools::EagerWorkspace;
use crate::tools::{RETRACT_FINDING, tool_defs};

/// Turn ceiling for the FAST tier (ADR-0062). The fast tier's cheapness comes from **no retrieval** + a
/// cheap model + short timeout — NOT from a single turn. One turn is too few: the model's first action is
/// also its last, so it can't both act and then `finish` (whose summary becomes the review body) — a
/// 1-turn fast pass posted an empty review on a PR with changes (vymalo-shop#301). A few no-retrieval
/// turns let it record findings via `add_review_comment` and `finish`. The fast block's own `max_turns`
/// (if set) lowers this; this caps it so an unset fast block can't inherit the generous default (40).
/// Applied here (not just via the golden `GoldenSettings`, which pre-cap) because it's a real production
/// requirement the byte-frozen traces don't exercise.
const FAST_TIER_MAX_TURNS: usize = 5;

/// The review-agent-owned numeric envelope for one run. The host maps its `ReviewConfig` onto this at
/// the call boundary, so `review-agent` depends on nothing in the runner. `diff_present` / `diff_files`
/// carry the change set the winddown filter and coverage gate need.
#[derive(Debug, Clone)]
pub struct ReviewRunParams {
    /// Requested turn ceiling (the FAST cap in [`FAST_TIER_MAX_TURNS`] is applied inside [`run_review`]).
    pub max_turns: usize,
    /// Max read-only tool calls run concurrently within one turn (ADR-0042).
    pub max_batch_size: usize,
    /// Investigation-batch budget: once spent, the wind-down narrowing fires (ADR-0042).
    pub max_batches: usize,
    /// Cumulative `read_file` budget (ADR-0042).
    pub max_files_read: usize,
    /// Cumulative retrieval budget (ADR-0042).
    pub max_searches: usize,
    /// Coverage-gate bounce cap (ADR-0069); `0` disables the bounce.
    pub max_coverage_bounces: usize,
    /// Per-run circuit-breaker threshold on consecutive transient turn failures (ADR-0039).
    pub circuit_breaker_threshold: u32,
    /// Model context window in tokens (ADR-0045); `None` disables budgeting.
    pub context_window: Option<usize>,
    /// FAST tier (ADR-0062): single diff-only pass, no retrieval, no investigation loop.
    pub fast: bool,
    /// Whether a PR diff is present — gates the winddown filter's inline-tool restriction.
    pub diff_present: bool,
    /// The changed-file set the coverage gate tracks engagement against.
    pub diff_files: Vec<String>,
    /// The opengrep coordinates the deterministic SAST pass flagged this run (ADR-0061), so
    /// [`SastAnchorGate`] can reject a triage verdict anchored to a different line (#305). Empty when
    /// SAST is off or found nothing.
    pub sast_leads: Vec<SastLead>,
}

/// Build a [`Workspace`](lci_agent_tools::Workspace) over an already-materialized checkout root, for
/// composing the [`ToolCx`] the loop runs under. The current Job host has the working tree on disk
/// before the agent starts, so the root resolves eagerly.
#[must_use]
pub fn eager_workspace(root: PathBuf) -> EagerWorkspace {
    EagerWorkspace::new(root)
}

/// Run one review: compose the policies in their frozen order, drive the engine loop over the injected
/// runtime + model, then flush any coverage disclosure. Returns the [`LoopOutcome`] the host maps to a
/// visible PR artifact; only a true transport/loop failure is `Err`.
///
/// Generic over the runtime + model so the Passthrough Job host and the scripted-model golden test share
/// exactly this assembly.
#[allow(clippy::too_many_arguments)]
pub async fn run_review<R, M>(
    runtime: R,
    model: M,
    sink: Box<dyn TranscriptSink>,
    cx: &ToolCx<'_>,
    registry: ToolRegistry,
    conversation: Conversation,
    params: ReviewRunParams,
    client: &ControlPlaneClient,
) -> anyhow::Result<LoopOutcome>
where
    R: StepRuntime,
    M: ModelClient,
{
    // FAST tier (ADR-0062): cap the turn budget so an unset fast block can't inherit the generous deep
    // default. A no-op for deep, and for the goldens (which pre-cap via `GoldenSettings`).
    let max_turns = if params.fast {
        params.max_turns.min(FAST_TIER_MAX_TURNS)
    } else {
        params.max_turns
    };

    // Wind-down inline-tool gate: with a diff, the wind-down tail keeps the full (convergence-narrowed)
    // set; without one, drop the inline `retract_finding` too — a no-diff run has no inline findings to
    // refute, mirroring the legacy diff-absent narrowing.
    let winddown_filter = if params.diff_present {
        TurnFilter::all()
    } else {
        TurnFilter::only_names(
            tool_defs()
                .into_iter()
                .map(|spec| spec.function.name)
                .filter(|name| name != RETRACT_FINDING),
        )
    };

    // Full-diff coverage gate (B, #137 + ADR-0069): tracks which changed files the agent engaged and
    // bounces an early finish that leaves some un-engaged; `coverage_state` carries the post-loop
    // disclosure back out.
    let (coverage, coverage_state) = CoverageGate::new(
        params.diff_files.clone(),
        params.max_coverage_bounces,
        max_turns,
        params.fast,
    );

    // Policy order is a behavioural contract (registration order = evaluation order in the engine):
    // context trim → wind-down → read budgets → turn budget → fast guard → scratchpad guard → coverage
    // gate → refute gate → SAST anchor gate → finding-finish nudge.
    let policies: Vec<Box<dyn TurnPolicy>> = vec![
        Box::new(ContextWindowTrim::new(params.context_window)),
        Box::new(
            WindDown::new(max_turns, params.max_batches)
                .disabled(params.fast)
                .with_filter(winddown_filter),
        ),
        Box::new(
            ReadBudgets::new(params.max_files_read, params.max_searches).disabled(params.fast),
        ),
        Box::new(TurnBudget::new(max_turns).disabled(params.fast)),
        Box::new(FastTierGuard::new(params.fast)),
        Box::new(ScratchpadLoopGuard::new()),
        Box::new(coverage),
        Box::new(RefuteGate::new(params.fast)),
        Box::new(SastAnchorGate::new(params.sast_leads, params.fast)),
        Box::new(FindingFinishNudge::new(params.fast)),
    ];

    let mut agent = AgentLoop::new(
        runtime,
        model,
        registry,
        policies,
        sink,
        LoopLimits {
            max_turns,
            max_batch_size: params.max_batch_size,
            circuit_breaker_threshold: params.circuit_breaker_threshold,
            no_tool_nudge: "Use the tools to investigate and record findings with `add_review_comment` (or a reply with `add_comment`), then call `finish` with your verdict (or `abort`). Do not reply in prose.".into(),
        },
    )
    .with_refusal_renderer(render_fast_refusal);

    let outcome = agent
        .run(conversation, cx)
        .await
        .map_err(|error| anyhow::anyhow!("review agent loop failed: {error}"))?;

    // Post-loop coverage disclosure (run bac4b5d8): a finish that went through with changed files still
    // un-engaged gets a machine-authored coverage note appended to the posted summary. Best-effort — a
    // failed re-post keeps the model's own summary rather than failing a finished run.
    if let Some(amended) = coverage_state.amended_summary() {
        let flushed = client.set_review_summary(cx.task_id, &amended).await;
        if let Err(error) = flushed {
            tracing::warn!(%error, task_id = %cx.task_id, "coverage disclosure re-post failed (non-fatal)");
        }
    }

    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use lci_agent_clients::{ControlPlaneClient, EmbeddingsClient};
    use lci_agent_loop::{ChatMessage, RequestOptions};
    use lci_agent_step::Passthrough;
    use lci_agent_testkit::{CapturingSink, ScriptedModel};
    use lci_agent_tools::RuntimeCaps;
    use lci_agent_types::{AssistantTurn, FunctionCallReq, ToolCallReq};
    use uuid::Uuid;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::tools::{FINISH, tool_registry};

    fn finish_turn() -> AssistantTurn {
        AssistantTurn {
            content: None,
            tool_calls: vec![ToolCallReq {
                id: "fin".into(),
                kind: "function".into(),
                function: FunctionCallReq {
                    name: FINISH.into(),
                    arguments: r#"{"summary":"done"}"#.into(),
                },
                extra_content: None,
            }],
        }
    }

    /// Drive `run_review` to a clean `finish` over the deterministic testkit. The finish tool records
    /// its summary control-plane-side, so a wiremock accepts that one write; a fast / no-diff run raises
    /// no coverage disclosure, so no other write is needed.
    async fn drive_to_finish(fast: bool, diff_present: bool) -> LoopOutcome {
        let cp = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!(
                "/internal/tasks/{}/review/summary",
                Uuid::nil()
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(&cp)
            .await;
        let checkout = tempfile::tempdir().unwrap();
        let client = ControlPlaneClient::new(cp.uri(), "tok");
        let embedder = EmbeddingsClient::new("http://unused", "key", "model");
        let registry = tool_registry(
            Arc::new(client.clone()),
            Arc::new(embedder),
            [],
            RuntimeCaps::default(),
        )
        .unwrap();
        let workspace = eager_workspace(checkout.path().to_path_buf());
        let cx = ToolCx {
            task_id: Uuid::nil(),
            workspace: &workspace,
        };
        let conversation = Conversation::new(
            vec![
                ChatMessage::system("be a reviewer"),
                ChatMessage::user("review"),
            ],
            RequestOptions {
                model: "m".to_string(),
                ..RequestOptions::default()
            },
        );
        let params = ReviewRunParams {
            max_turns: 40,
            max_batch_size: 8,
            max_batches: 6,
            max_files_read: 30,
            max_searches: 15,
            max_coverage_bounces: 3,
            circuit_breaker_threshold: 3,
            context_window: None,
            fast,
            diff_present,
            diff_files: if diff_present {
                vec!["a.rs".to_string()]
            } else {
                Vec::new()
            },
            sast_leads: Vec::new(),
        };
        run_review(
            Passthrough,
            ScriptedModel::new([finish_turn()]),
            Box::new(CapturingSink::default()),
            &cx,
            registry,
            conversation,
            params,
            &client,
        )
        .await
        .unwrap()
    }

    // A deep no-diff run: the winddown filter takes its diff-absent branch (dropping the inline
    // `retract_finding`) and the 40-turn budget is never capped; a first-turn finish converges cleanly
    // with no coverage disclosure (empty change set).
    #[tokio::test]
    async fn deep_no_diff_run_converges_on_finish() {
        assert_eq!(drive_to_finish(false, false).await, LoopOutcome::Finished);
    }

    // A fast run with a diff: the winddown filter takes its diff-present branch, the FAST turn cap +
    // `FastTierGuard` are in force, and coverage bounces are disabled — so a first-turn finish still
    // converges (and raises no disclosure, so no control-plane write).
    #[tokio::test]
    async fn fast_run_with_diff_converges_on_finish() {
        assert_eq!(drive_to_finish(true, true).await, LoopOutcome::Finished);
    }
}
