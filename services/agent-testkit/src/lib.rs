//! Deterministic support for the R1 legacy-vs-extracted agent comparison.

use std::collections::{BTreeSet, VecDeque};
use std::future::{Future, pending};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lci_agent_loop::{ChatRequest, ModelClient, TranscriptEvent, TranscriptSink};
use lci_agent_step::{AwaitableId, StepRuntime};
use lci_agent_tools::{BoxFuture, ReplaySafety, Tool, ToolCx, ToolKind};
use lci_agent_types::{
    AssistantTurn, FunctionCallReq, StepError, StepName, ToolCallReq, ToolOutcome, ToolSpec,
};
use serde::de::DeserializeOwned;
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

    /// One source of truth for the model replies used by both the legacy and extracted loops.
    #[must_use]
    pub fn script(self) -> GoldenScript {
        let call = |id: &str, name: &str, arguments: &str, extra_content| AssistantTurn {
            content: None,
            tool_calls: vec![ToolCallReq {
                id: id.into(),
                kind: "function".into(),
                function: FunctionCallReq {
                    name: name.into(),
                    arguments: arguments.into(),
                },
                extra_content,
            }],
        };
        let turns = match self {
            Self::PlainConvergeFinish => vec![
                call(
                    "plain-record",
                    "add_review_comment",
                    r#"{"file":"a.rs","line":2,"title":"Issue","priority":"P2","category":"quality","body":"body","evidence":"line 2"}"#,
                    Some(serde_json::json!({"provider":{"signature":"opaque"}})),
                ),
                call(
                    "plain-finish",
                    "finish",
                    r#"{"summary":"one finding"}"#,
                    None,
                ),
            ],
            Self::WindDownEntry => vec![
                call(
                    "wind-progress",
                    "report_progress",
                    r#"{"note":"working"}"#,
                    None,
                ),
                call("wind-finish", "finish", r#"{"summary":"done"}"#, None),
            ],
            Self::ContextTrimTrigger => vec![
                call("trim-read", "read_file", r#"{"path":"big.txt"}"#, None),
                call(
                    "trim-progress",
                    "report_progress",
                    r#"{"note":"working"}"#,
                    None,
                ),
                call("trim-finish", "finish", r#"{"summary":"done"}"#, None),
            ],
            Self::FastTierRefusal => vec![call(
                "fast-illegal",
                "read_file",
                r#"{"path":"a.rs"}"#,
                None,
            )],
            Self::CoverageBounce => vec![
                call(
                    "coverage-finish-1",
                    "finish",
                    r#"{"summary":"early"}"#,
                    None,
                ),
                call("coverage-read", "read_file", r#"{"path":"a.rs"}"#, None),
                call("coverage-finish-2", "finish", r#"{"summary":"done"}"#, None),
            ],
            Self::ExhaustedBackstop => vec![AssistantTurn {
                content: Some("still thinking".into()),
                tool_calls: Vec::new(),
            }],
        };
        GoldenScript { turns }
    }

    #[must_use]
    pub fn settings(self) -> GoldenSettings {
        match self {
            Self::PlainConvergeFinish => GoldenSettings::new(5).with_diff(),
            Self::WindDownEntry => GoldenSettings::new(2),
            Self::ContextTrimTrigger => GoldenSettings::new(5)
                .with_diff()
                .with_context_window(2_000),
            Self::FastTierRefusal => GoldenSettings::new(1).with_diff().fast(),
            Self::CoverageBounce => GoldenSettings::new(5).with_diff().with_coverage_bounces(1),
            Self::ExhaustedBackstop => GoldenSettings::new(2),
        }
    }
}

#[derive(Clone, Debug)]
pub struct GoldenScript {
    pub turns: Vec<AssistantTurn>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GoldenSettings {
    pub max_turns: usize,
    pub diff_present: bool,
    pub context_window: Option<usize>,
    pub fast: bool,
    pub max_coverage_bounces: usize,
}

impl GoldenSettings {
    fn new(max_turns: usize) -> Self {
        Self {
            max_turns,
            diff_present: false,
            context_window: None,
            fast: false,
            max_coverage_bounces: 3,
        }
    }

    fn with_diff(mut self) -> Self {
        self.diff_present = true;
        self
    }

    fn with_context_window(mut self, context_window: usize) -> Self {
        self.context_window = Some(context_window);
        self
    }

    fn fast(mut self) -> Self {
        self.fast = true;
        self
    }

    fn with_coverage_bounces(mut self, bounces: usize) -> Self {
        self.max_coverage_bounces = bounces;
        self
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

enum ScriptedResponse {
    Turn(AssistantTurn),
    Terminal(String),
    Transient(String),
}

/// Deterministic native-AFIT model fake that captures every complete request before replying.
#[derive(Clone, Default)]
pub struct ScriptedModel {
    responses: Arc<Mutex<VecDeque<ScriptedResponse>>>,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
    last_turn: Arc<Mutex<Option<AssistantTurn>>>,
    repeat_last: bool,
}

impl ScriptedModel {
    #[must_use]
    pub fn new(turns: impl IntoIterator<Item = AssistantTurn>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(
                turns.into_iter().map(ScriptedResponse::Turn).collect(),
            )),
            requests: Arc::default(),
            last_turn: Arc::default(),
            repeat_last: true,
        }
    }

    #[must_use]
    pub fn terminal(reason: impl Into<String>) -> Self {
        let model = Self::default();
        model
            .responses
            .lock()
            .expect("scripted model mutex")
            .push_back(ScriptedResponse::Terminal(reason.into()));
        model
    }

    #[must_use]
    pub fn transient(reason: impl Into<String>) -> Self {
        let model = Self::default();
        model
            .responses
            .lock()
            .expect("scripted model mutex")
            .push_back(ScriptedResponse::Transient(reason.into()));
        model
    }

    #[must_use]
    pub fn requests(&self) -> Vec<serde_json::Value> {
        self.requests
            .lock()
            .expect("scripted requests mutex")
            .clone()
    }
}

impl ModelClient for ScriptedModel {
    async fn complete(&self, request: ChatRequest<'_>) -> Result<AssistantTurn, StepError> {
        self.requests
            .lock()
            .expect("scripted requests mutex")
            .push(serde_json::to_value(request).expect("chat request serializes"));
        match self
            .responses
            .lock()
            .expect("scripted model mutex")
            .pop_front()
        {
            Some(ScriptedResponse::Turn(turn)) => {
                *self.last_turn.lock().expect("scripted last-turn mutex") = Some(turn.clone());
                Ok(turn)
            }
            Some(ScriptedResponse::Terminal(reason)) => Err(StepError::terminal(reason)),
            Some(ScriptedResponse::Transient(reason)) => {
                Err(StepError::transient(std::io::Error::other(reason), None))
            }
            None if self.repeat_last => self
                .last_turn
                .lock()
                .expect("scripted last-turn mutex")
                .clone()
                .ok_or_else(|| StepError::terminal("scripted model exhausted")),
            None => Err(StepError::terminal("scripted model exhausted")),
        }
    }
}

/// Cloneable handle to transcript entries after the loop consumes its boxed sink.
#[derive(Clone, Default)]
pub struct CapturingSink(Arc<Mutex<Vec<TranscriptEvent>>>);

impl CapturingSink {
    #[must_use]
    pub fn entries(&self) -> Vec<TranscriptEvent> {
        self.0.lock().expect("capturing sink mutex").clone()
    }
}

impl TranscriptSink for CapturingSink {
    fn record(&mut self, entry: TranscriptEvent) {
        self.0.lock().expect("capturing sink mutex").push(entry);
    }
}

/// Runtime fake that captures stable names and can fail selected steps before executing them.
#[derive(Clone, Default)]
pub struct FailingRuntime {
    fail: Arc<Mutex<BTreeSet<String>>>,
    steps: Arc<Mutex<Vec<String>>>,
}

impl FailingRuntime {
    #[must_use]
    pub fn on(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            fail: Arc::new(Mutex::new(names.into_iter().map(Into::into).collect())),
            steps: Arc::default(),
        }
    }

    #[must_use]
    pub fn steps(&self) -> Vec<String> {
        self.steps.lock().expect("runtime steps mutex").clone()
    }

    fn record(&self, name: &StepName) -> Result<(), StepError> {
        self.steps
            .lock()
            .expect("runtime steps mutex")
            .push(name.as_str().into());
        if self
            .fail
            .lock()
            .expect("runtime failure mutex")
            .contains(name.as_str())
        {
            Err(StepError::terminal(format!(
                "injected failure at {}",
                name.as_str()
            )))
        } else {
            Ok(())
        }
    }
}

impl StepRuntime for FailingRuntime {
    async fn step<T, F>(&self, name: StepName, f: F) -> Result<T, StepError>
    where
        T: Serialize + DeserializeOwned + Send + 'static,
        F: AsyncFnOnce() -> Result<T, StepError> + Send,
    {
        self.record(&name)?;
        f().await
    }

    async fn sleep(&self, name: StepName, _after: Duration) -> Result<(), StepError> {
        self.record(&name)
    }

    async fn awaitable<T>(
        &self,
        name: StepName,
    ) -> Result<
        (
            AwaitableId,
            impl Future<Output = Result<T, StepError>> + Send,
        ),
        StepError,
    >
    where
        T: Serialize + DeserializeOwned + Send + 'static,
    {
        self.record(&name)?;
        let (id, _) = lci_agent_step::Passthrough.awaitable::<T>(name).await?;
        Ok((id, pending()))
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use lci_agent_tools::{Workspace, WorkspaceError};
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

    #[tokio::test]
    async fn scripted_model_captures_requests_and_preserves_provider_fields() {
        let scripted = GoldenScenario::PlainConvergeFinish.script();
        let model = ScriptedModel::new(scripted.turns);
        let request = lci_agent_loop::ChatRequest {
            model: "m",
            messages: &[],
            tools: &[],
            tool_choice: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            stream: None,
            stream_options: None,
            extra: &serde_json::Map::new(),
        };
        let turn = model.complete(request).await.unwrap();
        assert_eq!(
            turn.tool_calls[0].extra_content.as_ref().unwrap()["provider"]["signature"],
            "opaque"
        );
        assert_eq!(
            model.requests(),
            vec![serde_json::json!({"model":"m","messages":[]})]
        );
    }

    #[tokio::test]
    async fn failing_runtime_records_before_failing() {
        let runtime = FailingRuntime::on(["llm_turn:0"]);
        let error = runtime
            .step(lci_agent_types::step_names::llm_turn(0), async || {
                Ok::<_, StepError>(1_u8)
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("injected failure"));
        assert_eq!(runtime.steps(), vec!["llm_turn:0"]);
    }

    #[test]
    fn capturing_sink_is_observable_after_boxing() {
        let sink = CapturingSink::default();
        let mut boxed: Box<dyn TranscriptSink> = Box::new(sink.clone());
        boxed.record(TranscriptEvent::Policy {
            turn: 3,
            name: "x",
            detail: serde_json::json!({}),
        });
        assert_eq!(sink.entries().len(), 1);
    }
}
