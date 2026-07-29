//! The turn-taking loop driver: owns the runtime/model/tools/policies and runs `AgentLoop::run`.

use std::collections::BTreeMap;

use futures::future::join_all;
use lci_agent_step::StepRuntime;
use lci_agent_tools::{
    DispatchRefusal, DispatchResult, ReadKind, ReplaySafety, ToolCx, ToolKind, ToolRegistry,
    TurnFilter,
};
use lci_agent_types::{StepError, ToolOutcome, ToolSpec, step_names};
use tracing::Instrument;

use crate::budget::{estimate_tokens, trim_tool_history};
use crate::chat::{ChatMessage, ChatRequest, Conversation, ModelClient, StreamOptions};
use crate::transcript::{LoopOutcome, TranscriptEvent, TranscriptSink};
use crate::turn::{LoopStats, PolicyAction, ToolCallResult, TurnOutcome, TurnPolicy, TurnState};

#[derive(Clone, Debug)]
pub struct LoopLimits {
    pub max_turns: usize,
    pub max_batch_size: usize,
    pub circuit_breaker_threshold: u32,
    pub no_tool_nudge: String,
}

impl Default for LoopLimits {
    fn default() -> Self {
        Self {
            max_turns: 40,
            max_batch_size: 4,
            circuit_breaker_threshold: 0,
            no_tool_nudge:
                "Use the available tools, then finish or abort; do not reply only in prose.".into(),
        }
    }
}

/// What one turn tells the outer loop to do next — the signal [`AgentLoop::run_turn`] returns.
/// Three-way, not a two-way `ControlFlow`: `StopLoop` needs to fall through to the SAME shared
/// exhaustion-tail logic (the `"exhausted"` transcript event + `exhausted_actions` policy hook) that
/// the `for` loop's own natural exit runs — collapsing it into `Return` would either duplicate that
/// tail inside `run_turn` or silently skip it for the force-finish/context-overflow paths.
enum TurnFlow {
    /// Proceed to the next turn.
    Continue,
    /// Stop iterating turns now; fall through to `run`'s exhaustion tail.
    StopLoop,
    /// Return this outcome immediately, bypassing the exhaustion tail (a real finish/abort verdict).
    Return(LoopOutcome),
}

/// The state one turn can mutate that must persist to the NEXT turn (and, for `stats`, into the
/// exhaustion tail after the loop) — bundled into one `&mut` so [`AgentLoop::run_turn`]'s signature
/// stays readable despite the number of cross-turn accumulators.
#[derive(Default)]
struct TurnAccumulators {
    stats: LoopStats,
    consecutive_failures: u32,
    context_overflow: bool,
}

pub type RefusalRenderer = fn(DispatchRefusal) -> ToolOutcome;

fn default_refusal(refusal: DispatchRefusal) -> ToolOutcome {
    let detail = match refusal {
        DispatchRefusal::NotOffered { tool_name } => {
            format!("tool {tool_name:?} was not offered on this turn")
        }
        DispatchRefusal::MissingCallId { tool_name } => {
            format!("tool {tool_name:?} requires a non-empty call id")
        }
    };
    ToolOutcome::Continue(format!("error: {detail}"))
}

pub struct AgentLoop<R, M> {
    runtime: R,
    model: M,
    tools: ToolRegistry,
    policies: Vec<Box<dyn TurnPolicy>>,
    sink: Box<dyn TranscriptSink>,
    limits: LoopLimits,
    refusal_renderer: RefusalRenderer,
}

impl<R, M> AgentLoop<R, M> {
    #[must_use]
    pub fn new(
        runtime: R,
        model: M,
        tools: ToolRegistry,
        policies: Vec<Box<dyn TurnPolicy>>,
        sink: Box<dyn TranscriptSink>,
        limits: LoopLimits,
    ) -> Self {
        Self {
            runtime,
            model,
            tools,
            policies,
            sink,
            limits,
            refusal_renderer: default_refusal,
        }
    }

    #[must_use]
    pub fn with_refusal_renderer(mut self, renderer: RefusalRenderer) -> Self {
        self.refusal_renderer = renderer;
        self
    }
}

impl<R: StepRuntime, M: ModelClient> AgentLoop<R, M> {
    pub async fn run(
        &mut self,
        mut conversation: Conversation,
        cx: &ToolCx<'_>,
    ) -> Result<LoopOutcome, StepError> {
        let base_specs = self.tools.view(&TurnFilter::all()).specs().to_vec();
        let mut acc = TurnAccumulators::default();

        for turn in 0..self.limits.max_turns {
            let flow = self
                .run_turn(turn, cx, &mut conversation, &base_specs, &mut acc)
                .instrument(tracing::info_span!("agent.turn", turn))
                .await?;
            match flow {
                TurnFlow::Continue => continue,
                TurnFlow::StopLoop => break,
                TurnFlow::Return(outcome) => return Ok(outcome),
            }
        }

        self.sink.record(TranscriptEvent::Policy {
            turn: self.limits.max_turns,
            name: "exhausted",
            detail: serde_json::json!({
                "context_overflow": acc.context_overflow,
                "findings": acc.stats.findings_recorded,
            }),
        });
        for policy in &mut self.policies {
            let state = TurnState {
                turn: self.limits.max_turns,
                max_turns: self.limits.max_turns,
                messages: &conversation.messages,
                base_tools: &base_specs,
                stats: &acc.stats,
                converging: true,
            };
            let name = policy.name();
            for action in policy.exhausted_actions(&state) {
                if let PolicyAction::Record {
                    name: event_name,
                    detail,
                } = action
                {
                    self.sink.record(TranscriptEvent::Policy {
                        turn: self.limits.max_turns,
                        name: event_name.unwrap_or(name),
                        detail,
                    });
                }
            }
        }
        Ok(LoopOutcome::Exhausted)
    }

    /// One turn's worth of work — the `for turn in 0..max_turns` loop body in [`Self::run`], extracted
    /// so it can be individually `#[instrument]`ed (an `agent.turn` span per turn, ticket #246) without
    /// holding a `tracing::Span` guard across an `.await` (a known anti-pattern the loop's many awaits
    /// would otherwise force). Behavior-neutral: the logic below is identical to the pre-extraction
    /// inline body — only the control-flow exits changed shape, from raw `continue`/`break`/`return` on
    /// the `for` loop to a returned [`TurnFlow`], since a nested async fn can't `continue`/`break` its
    /// caller's loop directly.
    async fn run_turn(
        &mut self,
        turn: usize,
        cx: &ToolCx<'_>,
        conversation: &mut Conversation,
        base_specs: &[ToolSpec],
        acc: &mut TurnAccumulators,
    ) -> Result<TurnFlow, StepError> {
        let mut filter = conversation.initial_filter.clone();
        let mut converging = false;
        let mut guard_offered = false;
        let mut force_finish = false;

        for policy in &mut self.policies {
            let state = TurnState {
                turn,
                max_turns: self.limits.max_turns,
                messages: &conversation.messages,
                base_tools: base_specs,
                stats: &acc.stats,
                converging,
            };
            let name = policy.name();
            for action in policy.before_turn(&state) {
                match action {
                    PolicyAction::Narrow(next) => filter.narrow(&next),
                    PolicyAction::Inject(nudge) => {
                        conversation.messages.push(ChatMessage::user(nudge.0));
                    }
                    PolicyAction::Converge {
                        filter: next,
                        nudge,
                    } => {
                        filter.narrow(&next);
                        conversation.messages.push(ChatMessage::user(nudge.0));
                        converging = true;
                    }
                    PolicyAction::TrimHistory {
                        target_tokens,
                        convergence,
                    } => {
                        let trimmed = trim_tool_history(
                            &mut conversation.messages,
                            base_specs,
                            target_tokens,
                        );
                        if trimmed > 0 {
                            self.sink.record(TranscriptEvent::Policy {
                                turn,
                                name,
                                detail: serde_json::json!({"trimmed": trimmed}),
                            });
                        }
                        if estimate_tokens(&conversation.messages, base_specs) >= target_tokens
                            && let Some((next, nudge, detail)) = convergence
                        {
                            filter.narrow(&next);
                            if let Some(nudge) = nudge {
                                conversation.messages.push(ChatMessage::user(nudge.0));
                            }
                            if let Some(detail) = detail {
                                self.sink.record(TranscriptEvent::Policy {
                                    turn,
                                    name: "wind_down",
                                    detail,
                                });
                            }
                            converging = true;
                        }
                    }
                    PolicyAction::GuardOffered => guard_offered = true,
                    PolicyAction::Record {
                        name: event_name,
                        detail,
                    } => self.sink.record(TranscriptEvent::Policy {
                        turn,
                        name: event_name.unwrap_or(name),
                        detail,
                    }),
                    PolicyAction::ForceFinish { .. } => force_finish = true,
                    PolicyAction::RejectFinish(_) | PolicyAction::SetFindings(_) => {}
                }
            }
        }
        if force_finish {
            return Ok(TurnFlow::StopLoop);
        }

        let offered = self.tools.view(&filter);
        let request = ChatRequest {
            model: &conversation.request.model,
            messages: &conversation.messages,
            tools: offered.specs(),
            tool_choice: (!offered.specs().is_empty()).then_some("auto"),
            temperature: conversation.request.temperature,
            top_p: conversation.request.top_p,
            max_tokens: conversation.request.max_tokens,
            stream: conversation.request.stream,
            stream_options: conversation.request.stream.map(|_| StreamOptions {
                include_usage: true,
            }),
            extra: &conversation.request.extra,
        };
        let completion = self
            .runtime
            .step(step_names::llm_turn(turn), async || {
                self.model.complete(request).await
            })
            .await;
        let (assistant, telemetry) = match completion {
            Ok(mut turn) => {
                acc.consecutive_failures = 0;
                let telemetry = turn.telemetry.take();
                (ChatMessage::assistant(turn), telemetry)
            }
            Err(error) if error.is_transient() => {
                acc.consecutive_failures += 1;
                if self.limits.circuit_breaker_threshold > 0
                    && acc.consecutive_failures >= self.limits.circuit_breaker_threshold
                {
                    return Err(error);
                }
                return Ok(TurnFlow::Continue);
            }
            Err(error) if is_context_overflow(&error) => {
                acc.context_overflow = true;
                return Ok(TurnFlow::StopLoop);
            }
            Err(error) => return Err(error),
        };

        self.sink.record(TranscriptEvent::Assistant {
            turn,
            message: assistant.clone(),
            telemetry,
        });
        let calls = assistant.tool_calls.clone();
        conversation.messages.push(assistant.clone());
        if calls.is_empty() {
            conversation
                .messages
                .push(ChatMessage::user(self.limits.no_tool_nudge.clone()));
            return Ok(TurnFlow::Continue);
        }

        let read_indices: Vec<usize> = calls
            .iter()
            .enumerate()
            .filter(|(_, call)| {
                let dispatchable = !guard_offered
                    || offered
                        .specs()
                        .iter()
                        .any(|spec| spec.name() == call.function.name);
                matches!(
                    self.tools.kind(&call.function.name),
                    Some(ToolKind::ReadOnly(_))
                ) && dispatchable
            })
            .map(|(index, _)| index)
            .collect();
        if !read_indices.is_empty() {
            acc.stats.batches += 1;
        }
        for call in &calls {
            match self.tools.kind(&call.function.name) {
                Some(ToolKind::ReadOnly(ReadKind::File)) => acc.stats.files_read += 1,
                Some(ToolKind::ReadOnly(ReadKind::Retrieval)) => acc.stats.searches += 1,
                _ => {}
            }
        }

        let full_view = self.tools.view(&TurnFilter::all());
        let dispatch_view = if guard_offered { &offered } else { &full_view };
        let renderer = self.refusal_renderer;
        let batch_size = self.limits.max_batch_size.max(1);
        let batched = if read_indices.is_empty() {
            Vec::new()
        } else {
            self.runtime
                .step(step_names::tools(turn), async || {
                    let mut ordered = Vec::with_capacity(read_indices.len());
                    for chunk in read_indices.chunks(batch_size) {
                        let calls_ref = &calls;
                        let futures = chunk.iter().map(|&index| async move {
                            let outcome = render_dispatch(
                                dispatch_view.dispatch(cx, &calls_ref[index]).await,
                                renderer,
                            );
                            (index, outcome)
                        });
                        ordered.extend(join_all(futures).await);
                    }
                    Ok(ordered)
                })
                .await?
        };
        let mut batched: BTreeMap<usize, ToolOutcome> = batched.into_iter().collect();

        let mut results = Vec::with_capacity(calls.len());
        let mut finish_requested = false;
        let mut abort_reason = None;
        for (index, call) in calls.iter().enumerate() {
            let outcome = match batched.remove(&index) {
                Some(outcome) => outcome,
                None if guard_offered
                    && !offered
                        .specs()
                        .iter()
                        .any(|spec| spec.name() == call.function.name) =>
                {
                    render_dispatch(
                        DispatchResult::Refused(DispatchRefusal::NotOffered {
                            tool_name: call.function.name.clone(),
                        }),
                        renderer,
                    )
                }
                None if self.tools.replay(&call.function.name)
                    == Some(ReplaySafety::NeedsDedupKey)
                    && call.id.trim().is_empty() =>
                {
                    renderer(DispatchRefusal::MissingCallId {
                        tool_name: call.function.name.clone(),
                    })
                }
                None => {
                    self.runtime
                        .step(step_names::write_tool(turn, &call.id), async || {
                            Ok(render_dispatch(
                                full_view.dispatch(cx, call).await,
                                renderer,
                            ))
                        })
                        .await?
                }
            };
            self.sink.record(TranscriptEvent::Tool {
                turn,
                call: call.clone(),
                outcome: outcome.clone(),
            });
            match &outcome {
                ToolOutcome::Continue(content) => {
                    if matches!(self.tools.kind(&call.function.name), Some(ToolKind::Write)) {
                        acc.stats.successful_writes += 1;
                    }
                    conversation
                        .messages
                        .push(ChatMessage::tool(call.id.clone(), content.clone()));
                }
                ToolOutcome::Finish => finish_requested = true,
                ToolOutcome::Abort(reason) => abort_reason = Some(reason.clone()),
            }
            results.push(ToolCallResult {
                call: call.clone(),
                kind: self.tools.kind(&call.function.name),
                outcome,
            });
        }

        let outcome = TurnOutcome {
            assistant,
            results,
            finish_requested,
            abort_reason: abort_reason.clone(),
        };
        let mut reject_finish = false;
        for policy in &mut self.policies {
            let state = TurnState {
                turn,
                max_turns: self.limits.max_turns,
                messages: &conversation.messages,
                base_tools: base_specs,
                stats: &acc.stats,
                converging,
            };
            let name = policy.name();
            for action in policy.after_turn_actions(&state, &outcome) {
                match action {
                    PolicyAction::Inject(nudge) => {
                        conversation.messages.push(ChatMessage::user(nudge.0));
                    }
                    PolicyAction::RejectFinish(nudge) => {
                        reject_finish = true;
                        conversation.messages.push(ChatMessage::user(nudge.0));
                    }
                    PolicyAction::Record {
                        name: event_name,
                        detail,
                    } => self.sink.record(TranscriptEvent::Policy {
                        turn,
                        name: event_name.unwrap_or(name),
                        detail,
                    }),
                    PolicyAction::SetFindings(findings) => {
                        acc.stats.findings_recorded = findings;
                    }
                    PolicyAction::Narrow(_)
                    | PolicyAction::TrimHistory { .. }
                    | PolicyAction::Converge { .. }
                    | PolicyAction::GuardOffered
                    | PolicyAction::ForceFinish { .. } => {}
                }
            }
        }

        if let Some(reason) = abort_reason {
            return Ok(TurnFlow::Return(LoopOutcome::Aborted { reason }));
        }
        if finish_requested && !reject_finish {
            for policy in &mut self.policies {
                let state = TurnState {
                    turn,
                    max_turns: self.limits.max_turns,
                    messages: &conversation.messages,
                    base_tools: base_specs,
                    stats: &acc.stats,
                    converging,
                };
                let name = policy.name();
                for action in policy.finish_actions(&state, &outcome) {
                    if let PolicyAction::Record {
                        name: event_name,
                        detail,
                    } = action
                    {
                        self.sink.record(TranscriptEvent::Policy {
                            turn,
                            name: event_name.unwrap_or(name),
                            detail,
                        });
                    }
                }
            }
            return Ok(TurnFlow::Return(LoopOutcome::Finished));
        }

        Ok(TurnFlow::Continue)
    }
}

fn render_dispatch(result: DispatchResult, renderer: RefusalRenderer) -> ToolOutcome {
    match result {
        DispatchResult::Completed(outcome) => outcome,
        DispatchResult::Refused(refusal) => renderer(refusal),
    }
}

fn is_context_overflow(error: &StepError) -> bool {
    let message = error.to_string().to_lowercase();
    [
        "context length",
        "context_length_exceeded",
        "maximum context",
        "too many tokens",
        "reduce the length",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use lci_agent_step::Passthrough;
    use lci_agent_tools::{BoxFuture, RuntimeCaps, Tool, Workspace, WorkspaceError};
    use lci_agent_types::{AssistantTurn, FunctionCallReq, ToolCallReq};
    use uuid::Uuid;

    use super::*;
    use crate::chat::RequestOptions;

    struct Root;
    impl Workspace for Root {
        fn root(&self) -> BoxFuture<'_, Result<&Path, WorkspaceError>> {
            Box::pin(async { Ok(Path::new("/tmp")) })
        }
    }

    fn cx(workspace: &Root) -> ToolCx<'_> {
        ToolCx {
            task_id: Uuid::nil(),
            workspace,
        }
    }

    fn call(id: &str, name: &str) -> ToolCallReq {
        ToolCallReq {
            id: id.into(),
            kind: "function".into(),
            function: FunctionCallReq {
                name: name.into(),
                arguments: "{}".into(),
            },
            extra_content: None,
        }
    }

    fn assistant_turn(calls: Vec<ToolCallReq>) -> AssistantTurn {
        AssistantTurn {
            content: None,
            tool_calls: calls,
            telemetry: None,
        }
    }

    /// Test-only `Tool`: always returns the same outcome, ignoring the call.
    struct FixedTool {
        spec: ToolSpec,
        kind: ToolKind,
        replay: ReplaySafety,
        outcome: ToolOutcome,
    }

    impl Tool for FixedTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }
        fn kind(&self) -> ToolKind {
            self.kind
        }
        fn replay(&self) -> ReplaySafety {
            self.replay
        }
        fn call<'a>(
            &'a self,
            _cx: &'a ToolCx<'a>,
            _call: &'a ToolCallReq,
        ) -> BoxFuture<'a, ToolOutcome> {
            let outcome = self.outcome.clone();
            Box::pin(async move { outcome })
        }
    }

    fn fixed_tool(
        name: &str,
        kind: ToolKind,
        replay: ReplaySafety,
        outcome: ToolOutcome,
    ) -> Arc<dyn Tool> {
        Arc::new(FixedTool {
            spec: ToolSpec::function(name, name, serde_json::json!({})),
            kind,
            replay,
            outcome,
        })
    }

    fn registry(tools: Vec<Arc<dyn Tool>>) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        for tool in tools {
            registry
                .register(
                    tool,
                    RuntimeCaps {
                        replays_completed_steps: true,
                        per_call_dedup: true,
                    },
                )
                .expect("test tool registers");
        }
        registry
    }

    fn conversation(messages: Vec<ChatMessage>) -> Conversation {
        Conversation::new(
            messages,
            RequestOptions {
                model: "test-model".into(),
                ..Default::default()
            },
        )
    }

    /// Test-only `ModelClient` that plays back a fixed script of turns/errors, in order.
    enum ScriptedTurn {
        Turn(AssistantTurn),
        Transient(&'static str),
        Terminal(&'static str),
    }

    struct SequencedModel(Mutex<VecDeque<ScriptedTurn>>);

    impl SequencedModel {
        fn new(turns: Vec<ScriptedTurn>) -> Self {
            Self(Mutex::new(turns.into_iter().collect()))
        }
    }

    impl ModelClient for SequencedModel {
        async fn complete(&self, _request: ChatRequest<'_>) -> Result<AssistantTurn, StepError> {
            match self
                .0
                .lock()
                .expect("sequenced model mutex")
                .pop_front()
                .expect("sequenced model script exhausted")
            {
                ScriptedTurn::Turn(turn) => Ok(turn),
                ScriptedTurn::Transient(reason) => {
                    Err(StepError::transient(std::io::Error::other(reason), None))
                }
                ScriptedTurn::Terminal(reason) => Err(StepError::terminal(reason)),
            }
        }
    }

    /// Test-only `TranscriptSink` that records every entry for later inspection.
    #[derive(Clone, Default)]
    struct CapturingSink(Arc<Mutex<Vec<TranscriptEvent>>>);

    impl CapturingSink {
        fn entries(&self) -> Vec<TranscriptEvent> {
            self.0.lock().expect("capturing sink mutex").clone()
        }
    }

    impl TranscriptSink for CapturingSink {
        fn record(&mut self, entry: TranscriptEvent) {
            self.0.lock().expect("capturing sink mutex").push(entry);
        }
    }

    /// Test-only `TurnPolicy` scripted per-turn: `before`/`after` are indexed by turn number,
    /// `exhausted` fires once if the loop runs out of turns.
    #[derive(Default)]
    struct ScriptedPolicy {
        before: Vec<Vec<PolicyAction>>,
        after: Vec<Vec<PolicyAction>>,
        exhausted: Vec<PolicyAction>,
    }

    impl TurnPolicy for ScriptedPolicy {
        fn name(&self) -> &'static str {
            "scripted"
        }
        fn before_turn(&mut self, state: &TurnState<'_>) -> Vec<PolicyAction> {
            self.before.get(state.turn).cloned().unwrap_or_default()
        }
        fn after_turn_actions(
            &mut self,
            state: &TurnState<'_>,
            _outcome: &TurnOutcome,
        ) -> Vec<PolicyAction> {
            self.after.get(state.turn).cloned().unwrap_or_default()
        }
        fn exhausted_actions(&mut self, _state: &TurnState<'_>) -> Vec<PolicyAction> {
            self.exhausted.clone()
        }
    }

    #[test]
    fn loop_limits_default_matches_documented_contract() {
        let limits = LoopLimits::default();
        assert_eq!(limits.max_turns, 40);
        assert_eq!(limits.max_batch_size, 4);
        assert_eq!(limits.circuit_breaker_threshold, 0);
        assert!(limits.no_tool_nudge.contains("finish or abort"));
    }

    #[test]
    fn default_refusal_renders_not_offered_and_missing_call_id() {
        assert_eq!(
            default_refusal(DispatchRefusal::NotOffered {
                tool_name: "x".into()
            }),
            ToolOutcome::Continue(format!(
                "error: tool {:?} was not offered on this turn",
                "x"
            ))
        );
        assert_eq!(
            default_refusal(DispatchRefusal::MissingCallId {
                tool_name: "y".into()
            }),
            ToolOutcome::Continue(format!(
                "error: tool {:?} requires a non-empty call id",
                "y"
            ))
        );
    }

    #[test]
    fn is_context_overflow_matches_known_phrases_case_insensitively() {
        for phrase in [
            "Context Length exceeded",
            "CONTEXT_LENGTH_EXCEEDED",
            "Maximum context reached",
            "too many tokens",
            "please reduce the length",
        ] {
            assert!(is_context_overflow(&StepError::terminal(phrase)));
        }
    }

    #[test]
    fn is_context_overflow_ignores_unrelated_errors() {
        assert!(!is_context_overflow(&StepError::terminal("rate limited")));
    }

    #[tokio::test]
    async fn run_reaches_exhaustion_and_runs_the_policy_exhausted_hook() {
        let root = Root;
        let model = SequencedModel::new(vec![
            ScriptedTurn::Turn(assistant_turn(Vec::new())),
            ScriptedTurn::Turn(assistant_turn(Vec::new())),
        ]);
        let sink = CapturingSink::default();
        let policy = ScriptedPolicy {
            exhausted: vec![PolicyAction::Record {
                name: Some("custom_exhausted"),
                detail: serde_json::json!({"x": 1}),
            }],
            ..Default::default()
        };
        let mut agent = AgentLoop::new(
            Passthrough,
            model,
            registry(Vec::new()),
            vec![Box::new(policy)],
            Box::new(sink.clone()),
            LoopLimits {
                max_turns: 2,
                ..LoopLimits::default()
            },
        );
        let outcome = agent
            .run(conversation(Vec::new()), &cx(&root))
            .await
            .expect("loop runs");
        assert_eq!(outcome, LoopOutcome::Exhausted);
        assert!(sink.entries().iter().any(|entry| matches!(
            entry,
            TranscriptEvent::Policy { name: "custom_exhausted", detail, .. }
                if detail["x"] == 1
        )));
    }

    #[tokio::test]
    async fn run_stops_early_on_context_overflow_and_records_it() {
        let root = Root;
        let model = SequencedModel::new(vec![ScriptedTurn::Terminal("context_length_exceeded")]);
        let sink = CapturingSink::default();
        let mut agent = AgentLoop::new(
            Passthrough,
            model,
            registry(Vec::new()),
            Vec::new(),
            Box::new(sink.clone()),
            LoopLimits {
                max_turns: 1,
                ..LoopLimits::default()
            },
        );
        let outcome = agent
            .run(conversation(Vec::new()), &cx(&root))
            .await
            .expect("loop runs");
        assert_eq!(outcome, LoopOutcome::Exhausted);
        assert!(sink.entries().iter().any(|entry| matches!(
            entry,
            TranscriptEvent::Policy { name: "exhausted", detail, .. }
                if detail["context_overflow"] == true
        )));
    }

    #[tokio::test]
    async fn run_turn_tolerates_transient_errors_when_the_circuit_breaker_is_disabled() {
        let root = Root;
        let model = SequencedModel::new(vec![
            ScriptedTurn::Transient("boom"),
            ScriptedTurn::Transient("boom"),
            ScriptedTurn::Turn(assistant_turn(Vec::new())),
        ]);
        let mut agent = AgentLoop::new(
            Passthrough,
            model,
            registry(Vec::new()),
            Vec::new(),
            Box::new(CapturingSink::default()),
            LoopLimits {
                max_turns: 3,
                circuit_breaker_threshold: 0,
                ..LoopLimits::default()
            },
        );
        let outcome = agent
            .run(conversation(Vec::new()), &cx(&root))
            .await
            .expect("transient errors below threshold never break the loop");
        assert_eq!(outcome, LoopOutcome::Exhausted);
    }

    #[tokio::test]
    async fn run_turn_trips_the_circuit_breaker_at_the_configured_threshold() {
        let root = Root;
        let model = SequencedModel::new(vec![
            ScriptedTurn::Transient("boom"),
            ScriptedTurn::Transient("boom"),
        ]);
        let mut agent = AgentLoop::new(
            Passthrough,
            model,
            registry(Vec::new()),
            Vec::new(),
            Box::new(CapturingSink::default()),
            LoopLimits {
                max_turns: 5,
                circuit_breaker_threshold: 2,
                ..LoopLimits::default()
            },
        );
        let error = agent
            .run(conversation(Vec::new()), &cx(&root))
            .await
            .expect_err("two consecutive transient failures trip a threshold of 2");
        assert!(error.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn guard_offered_narrows_dispatch_refuses_unoffered_tools_and_counts_retrieval_reads() {
        let root = Root;
        let tools = registry(vec![
            fixed_tool(
                "allowed",
                ToolKind::ReadOnly(ReadKind::Retrieval),
                ReplaySafety::ReadOnly,
                ToolOutcome::Continue("ok".into()),
            ),
            fixed_tool(
                "blocked",
                ToolKind::ReadOnly(ReadKind::File),
                ReplaySafety::ReadOnly,
                ToolOutcome::Continue("unreachable".into()),
            ),
            fixed_tool(
                "finish",
                ToolKind::Terminal,
                ReplaySafety::Idempotent,
                ToolOutcome::Finish,
            ),
        ]);
        let model = SequencedModel::new(vec![
            ScriptedTurn::Turn(assistant_turn(vec![
                call("allowed-1", "allowed"),
                call("blocked-1", "blocked"),
            ])),
            ScriptedTurn::Turn(assistant_turn(vec![call("finish-1", "finish")])),
        ]);
        let sink = CapturingSink::default();
        let policy = ScriptedPolicy {
            before: vec![vec![
                PolicyAction::Narrow(TurnFilter::only_names(["allowed", "finish"])),
                PolicyAction::GuardOffered,
            ]],
            after: vec![Vec::new(), vec![PolicyAction::Narrow(TurnFilter::all())]],
            ..Default::default()
        };
        let mut conversation = conversation(Vec::new());
        conversation.request.stream = Some(true);
        let mut agent = AgentLoop::new(
            Passthrough,
            model,
            tools,
            vec![Box::new(policy)],
            Box::new(sink.clone()),
            LoopLimits {
                max_turns: 2,
                ..LoopLimits::default()
            },
        );
        let outcome = agent
            .run(conversation, &cx(&root))
            .await
            .expect("loop runs");
        assert_eq!(outcome, LoopOutcome::Finished);
        let entries = sink.entries();
        assert!(entries.iter().any(|entry| matches!(
            entry,
            TranscriptEvent::Tool { call, outcome: ToolOutcome::Continue(detail), .. }
                if call.function.name == "blocked" && detail.contains("not offered")
        )));
        assert!(entries.iter().any(|entry| matches!(
            entry,
            TranscriptEvent::Tool { call, outcome: ToolOutcome::Continue(detail), .. }
                if call.function.name == "allowed" && detail == "ok"
        )));
    }

    #[tokio::test]
    async fn dedup_key_tools_without_a_call_id_are_refused() {
        let root = Root;
        let tools = registry(vec![fixed_tool(
            "write",
            ToolKind::Write,
            ReplaySafety::NeedsDedupKey,
            ToolOutcome::Continue("should not run".into()),
        )]);
        let model = SequencedModel::new(vec![ScriptedTurn::Turn(assistant_turn(vec![call(
            "", "write",
        )]))]);
        let sink = CapturingSink::default();
        let mut agent = AgentLoop::new(
            Passthrough,
            model,
            tools,
            Vec::new(),
            Box::new(sink.clone()),
            LoopLimits {
                max_turns: 1,
                ..LoopLimits::default()
            },
        );
        agent
            .run(conversation(Vec::new()), &cx(&root))
            .await
            .expect("loop runs");
        assert!(sink.entries().iter().any(|entry| matches!(
            entry,
            TranscriptEvent::Tool { outcome: ToolOutcome::Continue(detail), .. }
                if detail.contains("non-empty call id")
        )));
    }

    #[tokio::test]
    async fn tool_abort_outcome_stops_the_loop_with_the_given_reason() {
        let root = Root;
        let tools = registry(vec![fixed_tool(
            "abort",
            ToolKind::Write,
            ReplaySafety::Idempotent,
            ToolOutcome::Abort("stop now".into()),
        )]);
        let model = SequencedModel::new(vec![ScriptedTurn::Turn(assistant_turn(vec![call(
            "abort-1", "abort",
        )]))]);
        let mut agent = AgentLoop::new(
            Passthrough,
            model,
            tools,
            Vec::new(),
            Box::new(CapturingSink::default()),
            LoopLimits {
                max_turns: 5,
                ..LoopLimits::default()
            },
        );
        let outcome = agent
            .run(conversation(Vec::new()), &cx(&root))
            .await
            .expect("loop runs");
        assert_eq!(
            outcome,
            LoopOutcome::Aborted {
                reason: "stop now".into()
            }
        );
    }

    #[tokio::test]
    async fn before_turn_force_finish_and_passthrough_actions_stop_the_loop() {
        let root = Root;
        let model = SequencedModel::new(vec![ScriptedTurn::Turn(assistant_turn(Vec::new()))]);
        let sink = CapturingSink::default();
        let policy = ScriptedPolicy {
            before: vec![vec![
                PolicyAction::SetFindings(2),
                PolicyAction::ForceFinish { reason: "budget" },
            ]],
            exhausted: vec![PolicyAction::Record {
                name: Some("force_finished"),
                detail: serde_json::json!({}),
            }],
            ..Default::default()
        };
        let mut agent = AgentLoop::new(
            Passthrough,
            model,
            registry(Vec::new()),
            vec![Box::new(policy)],
            Box::new(sink.clone()),
            LoopLimits {
                max_turns: 5,
                ..LoopLimits::default()
            },
        );
        let outcome = agent
            .run(conversation(Vec::new()), &cx(&root))
            .await
            .expect("loop runs");
        assert_eq!(outcome, LoopOutcome::Exhausted);
        assert!(sink.entries().iter().any(|entry| matches!(
            entry,
            TranscriptEvent::Policy {
                name: "force_finished",
                ..
            }
        )));
    }

    #[tokio::test]
    async fn trim_history_without_convergence_reaches_the_merge_point() {
        let root = Root;
        let model = SequencedModel::new(vec![ScriptedTurn::Turn(assistant_turn(Vec::new()))]);
        let policy = ScriptedPolicy {
            before: vec![vec![PolicyAction::TrimHistory {
                target_tokens: 1,
                convergence: None,
            }]],
            ..Default::default()
        };
        let mut agent = AgentLoop::new(
            Passthrough,
            model,
            registry(Vec::new()),
            vec![Box::new(policy)],
            Box::new(CapturingSink::default()),
            LoopLimits {
                max_turns: 1,
                ..LoopLimits::default()
            },
        );
        let outcome = agent
            .run(conversation(Vec::new()), &cx(&root))
            .await
            .expect("loop runs");
        assert_eq!(outcome, LoopOutcome::Exhausted);
    }
}
