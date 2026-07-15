//! Merge-bar test (RFC-0007 slice 5): drive the real loop engine ([`lci_agent_loop::AgentLoop`]) with
//! the deterministic testkit fakes ([`ScriptedModel`], [`CapturingSink`], and a testkit
//! [`StaticTool`]), the status projection tapped on, and prove:
//!
//! 1. **Live progress** — reading the projected state as the loop runs shows the turn advancing, the
//!    tool name appearing, and the findings count incrementing.
//! 2. **Behaviour-unchanged** — the wrapped sink receives byte-identical events whether the status tap
//!    is on or off, so the loop and the ADR-0034 transcript are untouched by the projection.
//!
//! The loop is driven directly (no HTTP, no control plane): [`AgentLoop`] is the engine both the
//! `run-once` review host and a future `serve` host share, and where the sink tap lives.

use std::sync::{Arc, Mutex};

use lci_agent_loop::{
    AgentLoop, ChatMessage, Conversation, LoopLimits, LoopOutcome, RequestOptions, TranscriptEvent,
    TranscriptSink,
};
use lci_agent_status::{Phase, StatusHandle, StatusSink, StatusSnapshot};
use lci_agent_step::Passthrough;
use lci_agent_testkit::{CapturingSink, ScriptedModel, StaticTool};
use lci_agent_tools::{
    BoxFuture, ReplaySafety, RuntimeCaps, Tool, ToolCx, ToolKind, ToolRegistry, Workspace,
    WorkspaceError,
};
use lci_agent_types::{AssistantTurn, FunctionCallReq, ToolCallReq, ToolOutcome, ToolSpec};
use uuid::Uuid;

const ADD_REVIEW_COMMENT: &str = "add_review_comment";
const FINISH: &str = "finish";

/// A minimal workspace — the tools under test never resolve the checkout root, but [`ToolCx`] needs a
/// `&dyn Workspace`.
struct NoopWorkspace;
impl Workspace for NoopWorkspace {
    fn root(&self) -> BoxFuture<'_, Result<&std::path::Path, WorkspaceError>> {
        Box::pin(async { Ok(std::path::Path::new("/tmp")) })
    }
}

/// A finding-record tool (`add_review_comment`, [`ToolKind::Write`]) that snapshots the shared status
/// state each time the loop dispatches it — so the timeline captures **live** mid-run progress at the
/// exact points the HTTP endpoint would read.
struct SnapshotTool {
    spec: ToolSpec,
    handle: StatusHandle,
    timeline: Arc<Mutex<Vec<StatusSnapshot>>>,
}

impl Tool for SnapshotTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Write
    }
    fn replay(&self) -> ReplaySafety {
        ReplaySafety::Idempotent
    }
    fn call<'a>(
        &'a self,
        _cx: &'a ToolCx<'a>,
        _call: &'a ToolCallReq,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            self.timeline
                .lock()
                .expect("timeline mutex")
                .push(self.handle.snapshot());
            // Same outcome string the plain `StaticTool` returns in the untapped run, so the two
            // transcripts stay byte-identical.
            ToolOutcome::Continue("recorded".into())
        })
    }
}

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

/// The frozen script both runs replay: record a finding, record another, then finish.
fn script() -> Vec<AssistantTurn> {
    vec![
        call_turn("r0", ADD_REVIEW_COMMENT, r#"{"line":1}"#),
        call_turn("r1", ADD_REVIEW_COMMENT, r#"{"line":2}"#),
        call_turn("fin", FINISH, r#"{"summary":"done"}"#),
    ]
}

fn add_spec() -> ToolSpec {
    ToolSpec::function(
        ADD_REVIEW_COMMENT,
        "record a finding",
        serde_json::json!({}),
    )
}
fn finish_spec() -> ToolSpec {
    ToolSpec::function(FINISH, "finish the review", serde_json::json!({}))
}

fn conversation() -> Conversation {
    Conversation::new(
        vec![
            ChatMessage::system("be a reviewer"),
            ChatMessage::user("review"),
        ],
        RequestOptions {
            model: "m".to_string(),
            ..RequestOptions::default()
        },
    )
}

fn limits() -> LoopLimits {
    LoopLimits {
        max_turns: 5,
        max_batch_size: 4,
        circuit_breaker_threshold: 0,
        no_tool_nudge: "use a tool".into(),
    }
}

/// Drive the loop with the status tap ON, capturing both the live snapshot timeline (from inside the
/// finding tool) and the wrapped sink's events.
async fn run_tapped() -> (Vec<StatusSnapshot>, StatusSnapshot, Vec<TranscriptEvent>) {
    let handle = StatusHandle::new(Uuid::nil());
    handle.set_phase(Phase::Reviewing);
    let timeline = Arc::new(Mutex::new(Vec::new()));

    let mut registry = ToolRegistry::new();
    registry
        .register(
            Arc::new(SnapshotTool {
                spec: add_spec(),
                handle: handle.clone(),
                timeline: timeline.clone(),
            }),
            RuntimeCaps::default(),
        )
        .unwrap();
    registry
        .register(
            Arc::new(StaticTool::new(
                finish_spec(),
                ToolKind::Terminal,
                ReplaySafety::ReadOnly,
                ToolOutcome::Finish,
            )),
            RuntimeCaps::default(),
        )
        .unwrap();

    let captured = CapturingSink::default();
    let sink: Box<dyn TranscriptSink> = Box::new(StatusSink::new(
        handle.clone(),
        Box::new(captured.clone()),
        [ADD_REVIEW_COMMENT],
    ));

    let workspace = NoopWorkspace;
    let cx = ToolCx {
        task_id: Uuid::nil(),
        workspace: &workspace,
    };
    let mut agent = AgentLoop::new(
        Passthrough,
        ScriptedModel::new(script()),
        registry,
        Vec::new(),
        sink,
        limits(),
    );
    let outcome = agent.run(conversation(), &cx).await.unwrap();
    assert_eq!(outcome, LoopOutcome::Finished);

    let timeline = timeline.lock().unwrap().clone();
    (timeline, handle.snapshot(), captured.entries())
}

/// Drive the identical loop with NO status tap (plain `StaticTool` for the finding tool, plain
/// `CapturingSink`), returning the wrapped sink's events for the parity comparison.
async fn run_untapped() -> Vec<TranscriptEvent> {
    let mut registry = ToolRegistry::new();
    registry
        .register(
            Arc::new(StaticTool::new(
                add_spec(),
                ToolKind::Write,
                ReplaySafety::Idempotent,
                ToolOutcome::Continue("recorded".into()),
            )),
            RuntimeCaps::default(),
        )
        .unwrap();
    registry
        .register(
            Arc::new(StaticTool::new(
                finish_spec(),
                ToolKind::Terminal,
                ReplaySafety::ReadOnly,
                ToolOutcome::Finish,
            )),
            RuntimeCaps::default(),
        )
        .unwrap();

    let captured = CapturingSink::default();
    let workspace = NoopWorkspace;
    let cx = ToolCx {
        task_id: Uuid::nil(),
        workspace: &workspace,
    };
    let mut agent = AgentLoop::new(
        Passthrough,
        ScriptedModel::new(script()),
        registry,
        Vec::new(),
        Box::new(captured.clone()),
        limits(),
    );
    let outcome = agent.run(conversation(), &cx).await.unwrap();
    assert_eq!(outcome, LoopOutcome::Finished);
    captured.entries()
}

#[tokio::test]
async fn status_projection_reflects_live_progress() {
    let (timeline, final_snapshot, _events) = run_tapped().await;

    // Two mid-run reads (one per finding tool dispatch): the projection is live.
    assert_eq!(
        timeline.len(),
        2,
        "expected one snapshot per finding dispatch"
    );

    // First read (turn 0 dispatch): the turn-0 assistant event has landed, but no finding recorded yet.
    assert_eq!(timeline[0].turn, 0);
    assert_eq!(timeline[0].findings_recorded, 0);
    assert_eq!(timeline[0].last_tool, None);
    // The host-set phase is visible throughout.
    assert_eq!(timeline[0].phase, Phase::Reviewing);

    // Second read (turn 1 dispatch): the turn advanced, the tool name appeared, findings incremented.
    assert_eq!(timeline[1].turn, 1, "turn must advance");
    assert_eq!(
        timeline[1].last_tool.as_deref(),
        Some(ADD_REVIEW_COMMENT),
        "the current/last tool name must appear"
    );
    assert_eq!(
        timeline[1].findings_recorded, 1,
        "findings count must increment as findings are recorded"
    );

    // Final state after the loop finishes: both findings counted, last tool is `finish`, turn reached 2.
    assert_eq!(final_snapshot.turn, 2);
    assert_eq!(final_snapshot.findings_recorded, 2);
    assert_eq!(final_snapshot.last_tool.as_deref(), Some(FINISH));
}

#[tokio::test]
async fn status_tap_leaves_the_transcript_unchanged() {
    let (_timeline, _final, tapped_events) = run_tapped().await;
    let untapped_events = run_untapped().await;
    // The wrapped sink saw byte-identical events with and without the tap: the projection is a pure
    // read-only tee, so the loop's behaviour and the ADR-0034 transcript are untouched.
    assert_eq!(
        tapped_events, untapped_events,
        "the status tap changed the events the wrapped sink received"
    );
    // And it is a real, non-empty run (guard against a vacuous parity pass).
    assert!(!tapped_events.is_empty());
}
