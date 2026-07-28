//! The review flow: compose the review policies in their exact order, run the engine loop, and flush
//! the post-loop coverage disclosure. This is the review-specific assembly the host (and its own test
//! module below) drive.
//!
//! ADR-0103: every preset (however an operator names it) runs through this SAME policy composition —
//! the same tools, the same gates, the same prompt text. A preset varies the review ONLY through the
//! numeric budgets in [`ReviewRunParams`] (`max_turns`, `max_files_read`, …) the host resolves from its
//! config; nothing here branches on which preset is running (the old FAST-tier structural narrowing —
//! `FastTierGuard`, a hardcoded 5-turn cap, and skipping the coverage/refute/SAST-anchor gates — was
//! retired for exactly this reason: PR #488 already proved gate parity works, this makes divergence
//! structurally impossible instead of a discipline).
//!
//! The host injects the runtime, model, sink, tool registry, seeded conversation, and the numeric
//! [`ReviewRunParams`]; this module owns the policy vector, the [`LoopLimits`], and the coverage flush —
//! nothing that couples it back to the runner's config.

use std::path::PathBuf;

use lci_agent_clients::ControlPlaneClient;
use lci_agent_loop::policy::{ContextWindowTrim, ReadBudgets, TurnBudget, WindDown};
use lci_agent_loop::{
    AgentLoop, Conversation, LoopLimits, LoopOutcome, ModelClient, TranscriptSink, TurnPolicy,
};
use lci_agent_step::StepRuntime;
use lci_agent_tools::{ToolCx, ToolRegistry, TurnFilter};

use crate::policies::{
    CoverageGate, FindingFinishNudge, RefuteGate, SastAnchorGate, SastLeadSink,
    ScratchpadLoopGuard, render_fast_refusal,
};
pub use crate::tools::EagerWorkspace;
use crate::tools::{RETRACT_FINDING, tool_defs};

/// The review-agent-owned numeric envelope for one run. The host maps its `ReviewConfig` onto this at
/// the call boundary, so `review-agent` depends on nothing in the runner. `diff_present` / `diff_files`
/// carry the change set the winddown filter and coverage gate need.
#[derive(Debug, Clone)]
pub struct ReviewRunParams {
    /// Requested turn ceiling — the preset's own `max_turns` (or the default), applied as-is: no
    /// structural per-tier cap (ADR-0103).
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
    /// Whether a PR diff is present — gates the winddown filter's inline-tool restriction.
    pub diff_present: bool,
    /// The changed-file set the coverage gate tracks engagement against.
    pub diff_files: Vec<String>,
    /// The shared feed the `run_sast` tool pushes opengrep leads into as it scans (ADR-0073), so
    /// [`SastAnchorGate`] can reject a triage verdict anchored to a different line (#305). Starts empty
    /// and may never fill — SAST off, no diff, or the agent simply never calls the tool.
    pub sast_leads: SastLeadSink,
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
    let max_turns = params.max_turns;

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
    );

    // Policy order is a behavioural contract (registration order = evaluation order in the engine):
    // context trim → wind-down → read budgets → turn budget → scratchpad guard → coverage gate →
    // refute gate → SAST anchor gate → finding-finish nudge.
    //
    // ADR-0103: every preset composes the SAME policy set, always enabled — a tight-budget preset
    // (what used to be the structurally-distinct FAST tier) converges quickly because its own
    // `max_turns`/`max_files_read`/`max_searches` are small, not because gates are skipped for it. The
    // coverage/refute/SAST-anchor gates already ran identically for both tiers on the OpenCode path
    // (fast-tier-parity, see `ReviewGates`); this brings the native/legacy loop to the same guarantee.
    let policies: Vec<Box<dyn TurnPolicy>> = vec![
        Box::new(ContextWindowTrim::new(params.context_window)),
        Box::new(WindDown::new(max_turns, params.max_batches).with_filter(winddown_filter)),
        Box::new(ReadBudgets::new(params.max_files_read, params.max_searches)),
        Box::new(TurnBudget::new(max_turns)),
        Box::new(ScratchpadLoopGuard::new()),
        Box::new(coverage),
        Box::new(RefuteGate::new()),
        Box::new(SastAnchorGate::new(params.sast_leads)),
        Box::new(FindingFinishNudge::new()),
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

    use std::sync::{Arc, Mutex};

    use lci_agent_clients::{ControlPlaneClient, EmbeddingsClient};
    use lci_agent_loop::{ChatMessage, RequestOptions};
    use lci_agent_step::Passthrough;
    use lci_agent_testkit::{CapturingSink, ScriptedModel};
    use lci_agent_tools::RuntimeCaps;
    use lci_agent_types::{AssistantTurn, FunctionCallReq, ToolCallReq};
    use uuid::Uuid;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::tools::{
        ADD_REVIEW_COMMENT, FINISH, GRAPH_FIND_SYMBOL, GRAPH_GET_CALLERS, READ_FILE,
        REPORT_PROGRESS, VECTOR_SEMANTIC_SEARCH, tool_registry,
    };

    fn call_turn(id: &str, name: &str, arguments: &str) -> AssistantTurn {
        AssistantTurn {
            content: None,
            tool_calls: vec![ToolCallReq {
                id: id.into(),
                kind: "function".into(),
                function: FunctionCallReq {
                    name: name.into(),
                    arguments: arguments.into(),
                },
                extra_content: None,
            }],
            ..Default::default()
        }
    }

    fn finish_turn() -> AssistantTurn {
        call_turn("fin", FINISH, r#"{"summary":"done"}"#)
    }

    /// Drive `run_review` to a clean `finish` over the deterministic testkit, with the given model
    /// script. The finish tool records its summary control-plane-side, so a wiremock accepts that one
    /// write; a no-diff run raises no coverage disclosure, so no other write is needed.
    async fn drive_to_finish(diff_present: bool, script: Vec<AssistantTurn>) -> LoopOutcome {
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
        if diff_present {
            tokio::fs::write(checkout.path().join("a.rs"), "one\ntwo\nthree\n")
                .await
                .unwrap();
        }
        let client = ControlPlaneClient::new(cp.uri(), "tok");
        let embedder = EmbeddingsClient::new("http://unused", "key", "model");
        let registry = tool_registry(
            Arc::new(client.clone()),
            Arc::new(embedder),
            [],
            RuntimeCaps::default(),
            None,
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
            diff_present,
            diff_files: if diff_present {
                vec!["a.rs".to_string()]
            } else {
                Vec::new()
            },
            sast_leads: Arc::new(Mutex::new(Vec::new())),
        };
        run_review(
            Passthrough,
            ScriptedModel::new(script),
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

    // A no-diff run: the winddown filter takes its diff-absent branch (dropping the inline
    // `retract_finding`), the coverage gate has nothing to bounce against (empty change set), and a
    // first-turn finish converges cleanly with no disclosure.
    #[tokio::test]
    async fn no_diff_run_converges_on_finish() {
        assert_eq!(
            drive_to_finish(false, vec![finish_turn()]).await,
            LoopOutcome::Finished
        );
    }

    // ADR-0103: every preset runs the SAME gates — with a diff present, the coverage gate is always
    // in force (no tier-based skip), so the model must actually engage the changed file before
    // `finish` converges; a tight-budget ("what used to be fast") preset gets there via a short
    // script exactly like a generous-budget one would.
    #[tokio::test]
    async fn diff_run_converges_on_finish_once_the_changed_file_is_engaged() {
        let script = vec![
            call_turn(
                "read",
                READ_FILE,
                r#"{"path":"a.rs"}"#,
            ),
            finish_turn(),
        ];
        assert_eq!(
            drive_to_finish(true, script).await,
            LoopOutcome::Finished
        );
    }

    // Regression for #407: WindDown strips every ReadOnly tool once it converges, so if RefuteGate
    // bounces a `finish` carrying a P0/P1 finding at or after that point, the bounce's own
    // "re-verify" directive (which names read_file / the graph / vector tools) would otherwise land
    // on a turn where none of them are offered — spending the one-shot bounce for nothing. Drives:
    // turn 0 records a P1 finding, turns 1-2 are filler (pre-winddown), turn 3 is at the winddown
    // boundary (`winddown_turn(5) == 3`) and attempts `finish` — RefuteGate bounces it — turn 4 is
    // the bounce's follow-up and must still offer `read_file` (and the rest of the retrieval set)
    // despite WindDown's narrowing still being in effect.
    #[tokio::test]
    async fn refute_bounce_forces_retrieval_tools_back_after_winddown_narrows_them_away() {
        let cp = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!(
                "/internal/tasks/{}/review/summary",
                Uuid::nil()
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(&cp)
            .await;
        Mock::given(method("POST"))
            .and(path(format!(
                "/internal/tasks/{}/review/inline",
                Uuid::nil()
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(&cp)
            .await;
        let checkout = tempfile::tempdir().unwrap();
        tokio::fs::write(checkout.path().join("a.rs"), "one\ntwo\nthree\n")
            .await
            .unwrap();
        let client = ControlPlaneClient::new(cp.uri(), "tok");
        let embedder = EmbeddingsClient::new("http://unused", "key", "model");
        let registry = tool_registry(
            Arc::new(client.clone()),
            Arc::new(embedder),
            [],
            RuntimeCaps::default(),
            None,
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
            max_turns: 5,
            max_batch_size: 8,
            max_batches: 6,
            max_files_read: 30,
            max_searches: 15,
            // Isolate the RefuteGate bounce from CoverageGate's own bounce mechanism.
            max_coverage_bounces: 0,
            circuit_breaker_threshold: 3,
            context_window: None,
            diff_present: true,
            diff_files: vec!["a.rs".to_string()],
            sast_leads: Arc::new(Mutex::new(Vec::new())),
        };
        let model = ScriptedModel::new([
            call_turn(
                "finding",
                ADD_REVIEW_COMMENT,
                r#"{"file":"a.rs","line":2,"title":"t","priority":"P1","category":"quality","body":"b","evidence":"line 2"}"#,
            ),
            call_turn("p1", REPORT_PROGRESS, r#"{"note":"working"}"#),
            call_turn("p2", REPORT_PROGRESS, r#"{"note":"working"}"#),
            finish_turn(), // turn 3: at the winddown boundary — RefuteGate bounces this finish
            finish_turn(), // turn 4: the bounce's follow-up turn
        ]);
        let model_handle = model.clone();
        let outcome = run_review(
            Passthrough,
            model,
            Box::new(CapturingSink::default()),
            &cx,
            registry,
            conversation,
            params,
            &client,
        )
        .await
        .unwrap();
        assert_eq!(outcome, LoopOutcome::Finished);

        let requests = model_handle.requests();
        assert_eq!(requests.len(), 5, "expected exactly 5 turns to have run");
        let offered_names = |index: usize| -> Vec<String> {
            requests[index]["tools"]
                .as_array()
                .expect("tools is a non-empty array")
                .iter()
                .map(|tool| tool["function"]["name"].as_str().unwrap().to_string())
                .collect()
        };
        // Sanity check: winddown really did strip read-only tools by the bounce turn — otherwise
        // this test would pass without ever exercising the fix.
        assert!(
            !offered_names(3).contains(&READ_FILE.to_string()),
            "expected winddown to have already narrowed away read_file by turn 3, got {:?}",
            offered_names(3)
        );
        let post_bounce = offered_names(4);
        for tool in [
            READ_FILE,
            GRAPH_FIND_SYMBOL,
            GRAPH_GET_CALLERS,
            VECTOR_SEMANTIC_SEARCH,
        ] {
            assert!(
                post_bounce.contains(&tool.to_string()),
                "expected {tool} to be forced back onto the post-bounce turn, got {post_bounce:?}"
            );
        }
    }
}
