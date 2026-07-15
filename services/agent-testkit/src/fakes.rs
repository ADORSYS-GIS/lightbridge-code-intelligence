//! Fake implementations of the agent-loop's model, tool, runtime, and transcript-sink traits.
//!
//! Each fake is a deterministic dev-dependency test double: [`ScriptedModel`] scripts a
//! [`ModelClient`]'s replies, [`StaticTool`] scripts a [`Tool`]'s outcome, [`FailingRuntime`] scripts a
//! [`StepRuntime`] that can fail selected steps, and [`CapturingSink`] records a [`TranscriptSink`]'s
//! entries so tests can inspect them after the loop consumes its boxed sink.

use std::collections::{BTreeSet, VecDeque};
use std::future::{Future, pending};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lci_agent_loop::{ChatRequest, ModelClient, TranscriptEvent, TranscriptSink};
use lci_agent_step::{AwaitableId, StepRuntime};
use lci_agent_tools::{BoxFuture, ReplaySafety, Tool, ToolCx, ToolKind};
use lci_agent_types::{AssistantTurn, StepError, StepName, ToolCallReq, ToolOutcome, ToolSpec};
use serde::Serialize;
use serde::de::DeserializeOwned;

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

#[cfg(test)]
mod tests {
    use super::*;
    use lci_agent_tools::{Workspace, WorkspaceError};
    use lci_agent_types::FunctionCallReq;
    use std::path::Path;
    struct Root;
    impl Workspace for Root {
        fn root(&self) -> BoxFuture<'_, Result<&Path, WorkspaceError>> {
            Box::pin(async { Ok(Path::new("/tmp")) })
        }
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

    #[tokio::test]
    async fn scripted_model_captures_requests_and_preserves_provider_fields() {
        let scripted = crate::GoldenScenario::PlainConvergeFinish.script();
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
