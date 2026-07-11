//! Deterministic support for the R1 legacy-vs-extracted agent comparison.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lci_agent_loop::{ChatRequest, ModelClient, TranscriptSink};
use lci_agent_step::StepRuntime;
use lci_agent_tools::{BoxFuture, ReplaySafety, Tool, ToolCx, ToolKind};
use lci_agent_types::{
    AssistantTurn, LoopOutcome, StepError, StepName, ToolCallReq, ToolOutcome, ToolSpec,
    TranscriptEntry,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoldenScenario {
    PlainConvergeFinish,
    WindDownEntry,
    ContextTrimTrigger,
    FastTierRefusal,
    CoverageBounce,
    ExhaustedBackstop,
}

impl GoldenScenario {
    pub const ALL: [Self; 6] = [
        Self::PlainConvergeFinish,
        Self::WindDownEntry,
        Self::ContextTrimTrigger,
        Self::FastTierRefusal,
        Self::CoverageBounce,
        Self::ExhaustedBackstop,
    ];
    fn fixture(self) -> &'static str {
        match self {
            Self::PlainConvergeFinish => include_str!("../goldens/plain_converge_finish.json"),
            Self::WindDownEntry => include_str!("../goldens/wind_down_entry.json"),
            Self::ContextTrimTrigger => include_str!("../goldens/context_trim_trigger.json"),
            Self::FastTierRefusal => include_str!("../goldens/fast_tier_refusal.json"),
            Self::CoverageBounce => include_str!("../goldens/coverage_bounce.json"),
            Self::ExhaustedBackstop => include_str!("../goldens/exhausted_backstop.json"),
        }
    }
}

/// Canonical legacy-side trace. Chat requests are the exact JSON bodies observed by wiremock, so
/// messages retain assistant tool calls, tool_call_id, and provider extra_content, while each turn's
/// complete descriptions/schemas/order are frozen under `tools` in that request.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LegacyTrace {
    pub scenario: GoldenScenario,
    pub chat_requests: Vec<serde_json::Value>,
    pub calls: Vec<ObservedCall>,
    pub policy_events: Vec<serde_json::Value>,
    pub control_plane_writes: Vec<ObservedWrite>,
    pub outcome: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ObservedCall {
    pub turn: usize,
    pub call: ToolCallReq,
    pub outcome: ToolOutcome,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ObservedWrite {
    pub endpoint: String,
    pub body: serde_json::Value,
}

pub struct GoldenHarness;
impl GoldenHarness {
    #[must_use]
    pub fn expected(scenario: GoldenScenario) -> LegacyTrace {
        serde_json::from_str(scenario.fixture()).expect("checked-in legacy trace is valid JSON")
    }
    #[must_use]
    pub fn canonical_bytes(trace: &LegacyTrace) -> Vec<u8> {
        serde_json::to_vec_pretty(trace).expect("legacy trace serializes")
    }
    pub fn assert_fixture(scenario: GoldenScenario, actual: &LegacyTrace) {
        let expected = Self::expected(scenario);
        assert_eq!(
            Self::canonical_bytes(actual),
            Self::canonical_bytes(&expected),
            "actual run_native_agent trace changed for {scenario:?}"
        );
    }
    pub fn assert_parity(legacy: &LegacyTrace, extracted: &LegacyTrace) {
        assert_eq!(
            Self::canonical_bytes(extracted),
            Self::canonical_bytes(legacy),
            "extracted loop changed the canonical legacy trace"
        );
    }
}

pub struct StaticTool {
    spec: ToolSpec,
    kind: ToolKind,
    replay: ReplaySafety,
    outcome: ToolOutcome,
    calls: Mutex<Vec<ToolCallReq>>,
}
impl StaticTool {
    #[must_use]
    pub fn new(spec: ToolSpec, kind: ToolKind, replay: ReplaySafety, outcome: ToolOutcome) -> Self {
        Self {
            spec,
            kind,
            replay,
            outcome,
            calls: Mutex::new(Vec::new()),
        }
    }
    #[must_use]
    pub fn calls(&self) -> Vec<ToolCallReq> {
        self.calls.lock().expect("static tool mutex").clone()
    }
}
impl Tool for StaticTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn kind(&self) -> ToolKind {
        self.kind
    }
    fn replay(&self) -> ReplaySafety {
        self.replay
    }
    fn call<'a>(&'a self, _: &'a ToolCx<'a>, call: &'a ToolCallReq) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            self.calls
                .lock()
                .expect("static tool mutex")
                .push(call.clone());
            self.outcome.clone()
        })
    }
}

/// A [`ModelClient`] that returns one canned assistant turn per call, in order — the "script" the
/// R1d parity test drives the extracted loop with (companion doc §7). Running out of turns is a
/// terminal error, which surfaces as a test failure rather than a silent hang.
pub struct ScriptedModel {
    turns: Mutex<VecDeque<AssistantTurn>>,
}

impl ScriptedModel {
    #[must_use]
    pub fn new(turns: impl IntoIterator<Item = AssistantTurn>) -> Self {
        Self {
            turns: Mutex::new(turns.into_iter().collect()),
        }
    }
}

impl ModelClient for ScriptedModel {
    async fn complete(&self, _request: ChatRequest<'_>) -> Result<AssistantTurn, StepError> {
        self.turns
            .lock()
            .expect("scripted model mutex")
            .pop_front()
            .ok_or_else(|| StepError::terminal("scripted model ran out of turns"))
    }
}

/// A [`TranscriptSink`] that keeps every recorded entry for inspection. Cloning shares the buffer, so
/// a test can hand one clone to the loop and read the run through another.
#[derive(Clone, Default)]
pub struct CapturingSink {
    entries: Arc<Mutex<Vec<TranscriptEntry>>>,
}

impl CapturingSink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every entry the loop recorded, in order.
    #[must_use]
    pub fn entries(&self) -> Vec<TranscriptEntry> {
        self.entries.lock().expect("capturing sink mutex").clone()
    }

    /// Project the run down to the engine's observable *decisions* — the policy-event sequence, the
    /// tool-call sequence (by name + outcome tag), and the final outcome — for comparison against a
    /// frozen golden's generic projection ([`DecisionTrace::from_legacy`]).
    #[must_use]
    pub fn decision_trace(&self, outcome: &LoopOutcome) -> DecisionTrace {
        let entries = self.entries.lock().expect("capturing sink mutex");
        let policy_events = entries
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::Policy { turn, name, .. } => Some((*turn, name.clone())),
                _ => None,
            })
            .collect();
        let calls = entries
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::ToolResult {
                    turn,
                    call,
                    outcome,
                } => Some((
                    *turn,
                    call.function.name.clone(),
                    outcome_tag(outcome).to_string(),
                )),
                _ => None,
            })
            .collect();
        DecisionTrace {
            policy_events,
            calls,
            outcome: outcome_status(outcome).to_string(),
        }
    }
}

impl TranscriptSink for CapturingSink {
    fn record(&mut self, entry: TranscriptEntry) {
        self.entries
            .lock()
            .expect("capturing sink mutex")
            .push(entry);
    }
}

/// A [`StepRuntime`] whose every step fails terminally, without running the step body — used to prove
/// the engine surfaces a runtime failure instead of swallowing it.
pub struct FailingRuntime {
    reason: String,
}

impl FailingRuntime {
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl StepRuntime for FailingRuntime {
    async fn step<T, F>(&self, _name: StepName, _f: F) -> Result<T, StepError>
    where
        T: Serialize + serde::de::DeserializeOwned + Send + 'static,
        F: AsyncFnOnce() -> Result<T, StepError> + Send,
    {
        Err(StepError::terminal(self.reason.clone()))
    }

    async fn sleep(&self, _name: StepName, _after: Duration) -> Result<(), StepError> {
        Ok(())
    }
}

/// The generic policy-event names the extracted engine emits. A frozen golden may also carry
/// review-flavored events (`coverage_bounce`, `finding_finish_nudge`, `exhausted`, …); those are the
/// review assembly's to reproduce in R1e, so the R1d projection filters them out.
pub const GENERIC_POLICY_EVENTS: [&str; 5] = [
    "wind_down",
    "context_trim",
    "read_file_budget",
    "retrieval_budget",
    "halfway",
];

/// The engine's observable decisions for one run, comparable between the extracted loop and a golden.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecisionTrace {
    /// `(turn, policy-event name)` in emission order.
    pub policy_events: Vec<(usize, String)>,
    /// `(turn, tool name, outcome tag)` in call order.
    pub calls: Vec<(usize, String, String)>,
    /// `finished` / `exhausted` / `aborted`.
    pub outcome: String,
}

impl DecisionTrace {
    /// The generic projection of a frozen legacy trace: its calls and outcome verbatim, but only the
    /// generic policy events (review-flavored events are excluded — they land in R1e).
    #[must_use]
    pub fn from_legacy(trace: &LegacyTrace) -> Self {
        let policy_events = trace
            .policy_events
            .iter()
            .filter_map(|event| {
                let turn = event.get("turn")?.as_u64()? as usize;
                let name = event.get("name")?.as_str()?.to_string();
                GENERIC_POLICY_EVENTS
                    .contains(&name.as_str())
                    .then_some((turn, name))
            })
            .collect();
        let calls = trace
            .calls
            .iter()
            .map(|observed| {
                (
                    observed.turn,
                    observed.call.function.name.clone(),
                    outcome_tag(&observed.outcome).to_string(),
                )
            })
            .collect();
        let outcome = trace
            .outcome
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        Self {
            policy_events,
            calls,
            outcome,
        }
    }
}

fn outcome_tag(outcome: &ToolOutcome) -> &'static str {
    match outcome {
        ToolOutcome::Continue(_) => "Continue",
        ToolOutcome::Finish => "Finish",
        ToolOutcome::Abort(_) => "Abort",
    }
}

fn outcome_status(outcome: &LoopOutcome) -> &'static str {
    match outcome {
        LoopOutcome::Finished => "finished",
        LoopOutcome::Exhausted => "exhausted",
        LoopOutcome::Aborted { .. } => "aborted",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lci_agent_loop::{AgentLoop, LoopLimits, ReadBudgets, TurnBudget, TurnPolicy, WindDown};
    use lci_agent_step::Passthrough;
    use lci_agent_tools::{ReplaySafety, RuntimeCaps, ToolRegistry, Workspace, WorkspaceError};
    use lci_agent_types::{ChatMessage, FunctionCallReq};
    use std::path::Path;
    struct Root;
    impl Workspace for Root {
        fn root(&self) -> BoxFuture<'_, Result<&Path, WorkspaceError>> {
            Box::pin(async { Ok(Path::new("/tmp")) })
        }
    }
    #[test]
    fn scenario_list_is_frozen() {
        assert_eq!(GoldenScenario::ALL.len(), 6);
        for scenario in GoldenScenario::ALL {
            let trace = GoldenHarness::expected(scenario);
            assert_eq!(trace.scenario, scenario);
            assert!(!trace.chat_requests.is_empty());
            GoldenHarness::assert_fixture(scenario, &trace);
        }
    }
    #[test]
    fn parity_detects_protocol_drift() {
        let base = LegacyTrace {
            scenario: GoldenScenario::ExhaustedBackstop,
            chat_requests: vec![],
            calls: vec![],
            policy_events: vec![],
            control_plane_writes: vec![],
            outcome: serde_json::json!("exhausted"),
        };
        let mut changed = base.clone();
        changed.outcome = serde_json::json!("finished");
        assert!(
            std::panic::catch_unwind(|| GoldenHarness::assert_parity(&base, &changed)).is_err()
        );
    }
    #[test]
    fn static_tool_preserves_the_full_call() {
        let tool = StaticTool::new(
            ToolSpec::function("x", "x", serde_json::json!({})),
            ToolKind::Write,
            ReplaySafety::NeedsDedupKey,
            ToolOutcome::Continue("ok".into()),
        );
        let call = ToolCallReq {
            id: "actual-id".into(),
            kind: "function".into(),
            function: FunctionCallReq {
                name: "x".into(),
                arguments: "{}".into(),
            },
            extra_content: Some(serde_json::json!({"provider":"opaque"})),
        };
        tool.calls.lock().unwrap().push(call.clone());
        assert_eq!(tool.spec().name(), "x");
        assert_eq!(tool.kind(), ToolKind::Write);
        assert_eq!(tool.replay(), ReplaySafety::NeedsDedupKey);
        assert_eq!(tool.calls(), vec![call]);
    }
    #[tokio::test]
    async fn static_tool_executes_and_captures_the_actual_call() {
        let tool = StaticTool::new(
            ToolSpec::function("x", "x", serde_json::json!({})),
            ToolKind::Write,
            ReplaySafety::NeedsDedupKey,
            ToolOutcome::Continue("ok".into()),
        );
        let call = ToolCallReq {
            id: "call-id".into(),
            kind: "function".into(),
            function: FunctionCallReq {
                name: "x".into(),
                arguments: "{}".into(),
            },
            extra_content: None,
        };
        let cx = ToolCx {
            task_id: uuid::Uuid::nil(),
            workspace: &Root,
        };
        assert_eq!(
            tool.call(&cx, &call).await,
            ToolOutcome::Continue("ok".into())
        );
        assert_eq!(tool.calls(), vec![call]);
    }
    #[test]
    fn frozen_fixture_rejects_full_tool_spec_drift() {
        let mut actual = GoldenHarness::expected(GoldenScenario::PlainConvergeFinish);
        actual.chat_requests[0]["tools"][0]["function"]["description"] =
            serde_json::json!("drifted");
        assert!(
            std::panic::catch_unwind(|| GoldenHarness::assert_fixture(
                GoldenScenario::PlainConvergeFinish,
                &actual
            ))
            .is_err()
        );
    }

    fn generic_policies() -> Vec<Box<dyn TurnPolicy>> {
        vec![
            Box::new(WindDown::new()),
            Box::new(ReadBudgets::new()),
            Box::new(TurnBudget::new()),
        ]
    }

    fn budget(max_turns: usize) -> LoopLimits {
        LoopLimits {
            max_turns,
            max_batch_size: 8,
            max_batches: 100,
            max_files_read: 100,
            max_searches: 100,
            context_window: None,
        }
    }

    fn static_tool(name: &str, kind: ToolKind, outcome: ToolOutcome) -> std::sync::Arc<dyn Tool> {
        let replay = match kind {
            ToolKind::Terminal | ToolKind::Write => ReplaySafety::Idempotent,
            _ => ReplaySafety::ReadOnly,
        };
        std::sync::Arc::new(StaticTool::new(
            ToolSpec::function(name, "t", serde_json::json!({"type": "object"})),
            kind,
            replay,
            outcome,
        ))
    }

    fn call_turn(name: &str) -> AssistantTurn {
        AssistantTurn {
            content: None,
            tool_calls: vec![ToolCallReq {
                id: format!("c-{name}"),
                kind: "function".into(),
                function: FunctionCallReq {
                    name: name.into(),
                    arguments: "{}".into(),
                },
                extra_content: None,
            }],
        }
    }

    fn text_turn(text: &str) -> AssistantTurn {
        AssistantTurn {
            content: Some(text.into()),
            tool_calls: vec![],
        }
    }

    /// The R1d merge bar: the extracted engine, driven through the same script, reproduces the
    /// `wind_down_entry` golden's generic decision trace (companion doc §7). `report_progress` at
    /// turn 0, then `finish` at turn 1 — where `max_turns = 2` puts turn 1 in wind-down.
    #[tokio::test]
    async fn extracted_engine_reproduces_the_wind_down_entry_generic_trace() {
        let mut registry = ToolRegistry::new();
        registry
            .register(
                static_tool(
                    "report_progress",
                    ToolKind::Progress,
                    ToolOutcome::Continue("recorded".into()),
                ),
                RuntimeCaps::default(),
            )
            .unwrap();
        registry
            .register(
                static_tool("finish", ToolKind::Terminal, ToolOutcome::Finish),
                RuntimeCaps::default(),
            )
            .unwrap();
        let sink = CapturingSink::new();
        let mut agent = AgentLoop::new(
            Passthrough,
            ScriptedModel::new([call_turn("report_progress"), call_turn("finish")]),
            registry,
            generic_policies(),
            Box::new(sink.clone()),
            budget(2),
        );
        let cx = ToolCx {
            task_id: uuid::Uuid::nil(),
            workspace: &Root,
        };
        let outcome = agent
            .run(&cx, vec![ChatMessage::user("review")])
            .await
            .unwrap();

        let expected =
            DecisionTrace::from_legacy(&GoldenHarness::expected(GoldenScenario::WindDownEntry));
        assert_eq!(
            sink.decision_trace(&outcome),
            expected,
            "extracted engine's generic decision trace must match the wind_down_entry golden",
        );
    }

    /// The exhausted backstop: a prose-only run never calls a terminal tool, so the loop exhausts its
    /// budget. Reproduces the `exhausted_backstop` golden's generic trace (`wind_down` at turn 1, no
    /// calls, `exhausted`); the review-flavored `exhausted` event lands in R1e.
    #[tokio::test]
    async fn extracted_engine_reproduces_the_exhausted_backstop_generic_trace() {
        let sink = CapturingSink::new();
        let mut agent = AgentLoop::new(
            Passthrough,
            ScriptedModel::new([text_turn("thinking"), text_turn("still thinking")]),
            ToolRegistry::new(),
            generic_policies(),
            Box::new(sink.clone()),
            budget(2),
        );
        let cx = ToolCx {
            task_id: uuid::Uuid::nil(),
            workspace: &Root,
        };
        let outcome = agent
            .run(&cx, vec![ChatMessage::user("review")])
            .await
            .unwrap();

        let expected =
            DecisionTrace::from_legacy(&GoldenHarness::expected(GoldenScenario::ExhaustedBackstop));
        assert_eq!(sink.decision_trace(&outcome), expected);
    }

    #[tokio::test]
    async fn a_failing_runtime_surfaces_the_step_error() {
        let mut registry = ToolRegistry::new();
        registry
            .register(
                static_tool("finish", ToolKind::Terminal, ToolOutcome::Finish),
                RuntimeCaps::default(),
            )
            .unwrap();
        let mut agent = AgentLoop::new(
            FailingRuntime::new("runtime down"),
            ScriptedModel::new([call_turn("finish")]),
            registry,
            Vec::new(),
            Box::new(CapturingSink::new()),
            budget(3),
        );
        let cx = ToolCx {
            task_id: uuid::Uuid::nil(),
            workspace: &Root,
        };
        let error = agent
            .run(&cx, vec![ChatMessage::user("review")])
            .await
            .unwrap_err();
        assert!(!error.is_transient());
        assert!(error.to_string().contains("runtime down"));
    }
}
