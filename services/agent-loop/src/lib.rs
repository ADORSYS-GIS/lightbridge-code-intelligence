//! Runtime-independent agent loop, policy composition, and transcript seam.

#![allow(async_fn_in_trait)] // Native AFIT keeps the single model implementation statically dispatched.

use std::collections::BTreeMap;

use futures::future::join_all;
use lci_agent_step::StepRuntime;
use lci_agent_tools::{
    DispatchRefusal, DispatchResult, ReadKind, ReplaySafety, ToolCx, ToolKind, ToolRegistry,
    TurnFilter,
};
use lci_agent_types::{AssistantTurn, StepError, ToolCallReq, ToolOutcome, ToolSpec, step_names};
use serde::{Deserialize, Serialize};

/// One OpenAI-compatible conversation message.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallReq>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self::text("system", content)
    }

    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self::text("user", content)
    }

    #[must_use]
    pub fn assistant(turn: AssistantTurn) -> Self {
        Self {
            role: "assistant".into(),
            content: turn.content,
            tool_calls: turn.tool_calls,
            tool_call_id: None,
        }
    }

    #[must_use]
    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }

    fn text(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
}

fn slice_is_empty<T>(slice: &&[T]) -> bool {
    slice.is_empty()
}

/// Exact request presented to a model implementation.
#[derive(Debug, Serialize)]
pub struct ChatRequest<'a> {
    pub model: &'a str,
    pub messages: &'a [ChatMessage],
    #[serde(skip_serializing_if = "slice_is_empty")]
    pub tools: &'a [ToolSpec],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    #[serde(flatten)]
    pub extra: &'a serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

/// Static model boundary: one model implementation is selected by the assembly.
pub trait ModelClient: Send + Sync {
    async fn complete(&self, request: ChatRequest<'_>) -> Result<AssistantTurn, StepError>;
}

#[derive(Clone, Debug, Default)]
pub struct RequestOptions {
    pub model: String,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: Option<i64>,
    pub stream: Option<bool>,
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Debug)]
pub struct Conversation {
    pub messages: Vec<ChatMessage>,
    pub request: RequestOptions,
    pub initial_filter: TurnFilter,
}

impl Conversation {
    #[must_use]
    pub fn new(messages: Vec<ChatMessage>, request: RequestOptions) -> Self {
        Self {
            messages,
            request,
            initial_filter: TurnFilter::all(),
        }
    }

    #[must_use]
    pub fn with_filter(mut self, filter: TurnFilter) -> Self {
        self.initial_filter = filter;
        self
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LoopOutcome {
    Finished,
    Exhausted,
    Aborted { reason: String },
}

/// Generic transcript events. Control-plane transport rows remain owned by `agent-clients`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptEvent {
    Assistant {
        turn: usize,
        message: ChatMessage,
    },
    Tool {
        turn: usize,
        call: ToolCallReq,
        outcome: ToolOutcome,
    },
    Policy {
        turn: usize,
        name: &'static str,
        detail: serde_json::Value,
    },
}

pub trait TranscriptSink: Send {
    fn record(&mut self, entry: TranscriptEvent);
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LoopStats {
    pub files_read: usize,
    pub searches: usize,
    pub batches: usize,
    pub successful_writes: usize,
    pub findings_recorded: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCallResult {
    pub call: ToolCallReq,
    pub kind: Option<ToolKind>,
    pub outcome: ToolOutcome,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnOutcome {
    pub assistant: ChatMessage,
    pub results: Vec<ToolCallResult>,
    pub finish_requested: bool,
    pub abort_reason: Option<String>,
}

pub struct TurnState<'a> {
    pub turn: usize,
    pub max_turns: usize,
    pub messages: &'a [ChatMessage],
    pub base_tools: &'a [ToolSpec],
    pub stats: &'a LoopStats,
    /// True once an earlier policy in registration order has forced convergence this turn.
    pub converging: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Nudge(pub String);

impl From<&str> for Nudge {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}

impl From<String> for Nudge {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// Ordered policy effects. Every narrowing is intersected with the accumulated filter.
#[derive(Clone, Debug, PartialEq)]
pub enum PolicyAction {
    Narrow(TurnFilter),
    Inject(Nudge),
    TrimHistory {
        target_tokens: usize,
        convergence: Option<(TurnFilter, Option<Nudge>, Option<serde_json::Value>)>,
    },
    Converge {
        filter: TurnFilter,
        nudge: Nudge,
    },
    GuardOffered,
    RejectFinish(Nudge),
    Record {
        name: Option<&'static str>,
        detail: serde_json::Value,
    },
    ForceFinish {
        reason: &'static str,
    },
    SetFindings(usize),
}

/// Dynamic dispatch is intentional here: an assembly composes heterogeneous policies.
pub trait TurnPolicy: Send {
    fn name(&self) -> &'static str;
    fn before_turn(&mut self, state: &TurnState<'_>) -> Vec<PolicyAction>;
    fn after_turn(&mut self, _state: &TurnState<'_>, _outcome: &TurnOutcome) {}
    fn after_turn_actions(
        &mut self,
        state: &TurnState<'_>,
        outcome: &TurnOutcome,
    ) -> Vec<PolicyAction> {
        self.after_turn(state, outcome);
        Vec::new()
    }
    fn finish_actions(
        &mut self,
        _state: &TurnState<'_>,
        _outcome: &TurnOutcome,
    ) -> Vec<PolicyAction> {
        Vec::new()
    }
    fn exhausted_actions(&mut self, _state: &TurnState<'_>) -> Vec<PolicyAction> {
        Vec::new()
    }
}

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
        let mut stats = LoopStats::default();
        let mut consecutive_failures = 0_u32;
        let mut context_overflow = false;

        for turn in 0..self.limits.max_turns {
            let mut filter = conversation.initial_filter.clone();
            let mut converging = false;
            let mut guard_offered = false;
            let mut force_finish = false;

            for policy in &mut self.policies {
                let state = TurnState {
                    turn,
                    max_turns: self.limits.max_turns,
                    messages: &conversation.messages,
                    base_tools: &base_specs,
                    stats: &stats,
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
                                &base_specs,
                                target_tokens,
                            );
                            if trimmed > 0 {
                                self.sink.record(TranscriptEvent::Policy {
                                    turn,
                                    name,
                                    detail: serde_json::json!({"trimmed": trimmed}),
                                });
                            }
                            if estimate_tokens(&conversation.messages, &base_specs) >= target_tokens
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
                break;
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
            let assistant = match completion {
                Ok(turn) => {
                    consecutive_failures = 0;
                    ChatMessage::assistant(turn)
                }
                Err(error) if error.is_transient() => {
                    consecutive_failures += 1;
                    if self.limits.circuit_breaker_threshold > 0
                        && consecutive_failures >= self.limits.circuit_breaker_threshold
                    {
                        return Err(error);
                    }
                    continue;
                }
                Err(error) if is_context_overflow(&error) => {
                    context_overflow = true;
                    break;
                }
                Err(error) => return Err(error),
            };

            self.sink.record(TranscriptEvent::Assistant {
                turn,
                message: assistant.clone(),
            });
            let calls = assistant.tool_calls.clone();
            conversation.messages.push(assistant.clone());
            if calls.is_empty() {
                conversation
                    .messages
                    .push(ChatMessage::user(self.limits.no_tool_nudge.clone()));
                continue;
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
                stats.batches += 1;
            }
            for call in &calls {
                match self.tools.kind(&call.function.name) {
                    Some(ToolKind::ReadOnly(ReadKind::File)) => stats.files_read += 1,
                    Some(ToolKind::ReadOnly(ReadKind::Retrieval)) => stats.searches += 1,
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
                let outcome = if let Some(outcome) = batched.remove(&index) {
                    outcome
                } else if guard_offered
                    && !offered
                        .specs()
                        .iter()
                        .any(|spec| spec.name() == call.function.name)
                {
                    render_dispatch(
                        DispatchResult::Refused(DispatchRefusal::NotOffered {
                            tool_name: call.function.name.clone(),
                        }),
                        renderer,
                    )
                } else if self.tools.replay(&call.function.name)
                    == Some(ReplaySafety::NeedsDedupKey)
                    && call.id.trim().is_empty()
                {
                    renderer(DispatchRefusal::MissingCallId {
                        tool_name: call.function.name.clone(),
                    })
                } else {
                    self.runtime
                        .step(step_names::write_tool(turn, &call.id), async || {
                            Ok(render_dispatch(
                                full_view.dispatch(cx, call).await,
                                renderer,
                            ))
                        })
                        .await?
                };
                self.sink.record(TranscriptEvent::Tool {
                    turn,
                    call: call.clone(),
                    outcome: outcome.clone(),
                });
                match &outcome {
                    ToolOutcome::Continue(content) => {
                        if matches!(self.tools.kind(&call.function.name), Some(ToolKind::Write)) {
                            stats.successful_writes += 1;
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
                    base_tools: &base_specs,
                    stats: &stats,
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
                            stats.findings_recorded = findings;
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
                return Ok(LoopOutcome::Aborted { reason });
            }
            if finish_requested && !reject_finish {
                for policy in &mut self.policies {
                    let state = TurnState {
                        turn,
                        max_turns: self.limits.max_turns,
                        messages: &conversation.messages,
                        base_tools: &base_specs,
                        stats: &stats,
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
                return Ok(LoopOutcome::Finished);
            }
        }

        self.sink.record(TranscriptEvent::Policy {
            turn: self.limits.max_turns,
            name: "exhausted",
            detail: serde_json::json!({
                "context_overflow": context_overflow,
                "findings": stats.findings_recorded,
            }),
        });
        for policy in &mut self.policies {
            let state = TurnState {
                turn: self.limits.max_turns,
                max_turns: self.limits.max_turns,
                messages: &conversation.messages,
                base_tools: &base_specs,
                stats: &stats,
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

/// Conservative chars/4 estimator used only for context safety decisions.
#[must_use]
pub fn estimate_tokens(messages: &[ChatMessage], tools: &[ToolSpec]) -> usize {
    const PER_MESSAGE_OVERHEAD: usize = 4;
    let messages = messages
        .iter()
        .map(|message| {
            let content = message.content.as_deref().map_or(0, str::len);
            let calls = message
                .tool_calls
                .iter()
                .map(|call| call.function.name.len() + call.function.arguments.len())
                .sum::<usize>();
            PER_MESSAGE_OVERHEAD + (content + calls) / 4
        })
        .sum::<usize>();
    let tools = tools
        .iter()
        .map(|tool| {
            (tool.function.name.len()
                + tool.function.description.len()
                + tool.function.parameters.to_string().len())
                / 4
        })
        .sum::<usize>();
    messages + tools
}

/// Replace oldest consumed tool results while preserving assistant/call pairing.
pub fn trim_tool_history(messages: &mut [ChatMessage], tools: &[ToolSpec], target: usize) -> usize {
    const KEEP_RECENT: usize = 2;
    const STUB: &str = "[earlier tool output elided to fit the context budget]";
    let cutoff = messages.len().saturating_sub(KEEP_RECENT);
    let mut estimate = estimate_tokens(messages, tools);
    let mut trimmed = 0;
    for message in messages.iter_mut().take(cutoff) {
        if estimate <= target {
            break;
        }
        let old_len = match message.content.as_deref() {
            Some(content)
                if message.role == "tool" && content.len() > STUB.len() && content != STUB =>
            {
                content.len()
            }
            _ => continue,
        };
        estimate = estimate.saturating_sub((old_len - STUB.len()) / 4);
        message.content = Some(STUB.into());
        trimmed += 1;
    }
    trimmed
}

#[must_use]
pub fn convergence_filter() -> TurnFilter {
    TurnFilter::all()
        .without_kind(ToolKind::ReadOnly(ReadKind::Retrieval))
        .without_kind(ToolKind::ReadOnly(ReadKind::File))
        .without_kind(ToolKind::ReadOnly(ReadKind::Knowledge))
        .without_kind(ToolKind::Progress)
}

#[must_use]
pub fn winddown_turn(max_turns: usize) -> usize {
    const MIN_TURNS: usize = 2;
    if max_turns <= MIN_TURNS {
        return max_turns.saturating_sub(1).max(1);
    }
    let reserve = MIN_TURNS.max(max_turns / 10);
    max_turns
        .saturating_sub(reserve)
        .clamp(1, max_turns.saturating_sub(1))
}

pub mod policy {
    use super::*;

    pub struct ContextWindowTrim {
        context_window: Option<usize>,
        announced: bool,
    }

    impl ContextWindowTrim {
        #[must_use]
        pub fn new(context_window: Option<usize>) -> Self {
            Self {
                context_window,
                announced: false,
            }
        }
    }

    impl TurnPolicy for ContextWindowTrim {
        fn name(&self) -> &'static str {
            "context_trim"
        }

        fn before_turn(&mut self, state: &TurnState<'_>) -> Vec<PolicyAction> {
            let Some(window) = self.context_window else {
                return Vec::new();
            };
            let target = (window as f64 * 0.75) as usize;
            let estimate = estimate_tokens(state.messages, state.base_tools);
            if estimate < target {
                return Vec::new();
            }
            let mut preview = state.messages.to_vec();
            trim_tool_history(&mut preview, state.base_tools, target);
            let remains_over = estimate_tokens(&preview, state.base_tools) >= target;
            let convergence = if self.announced {
                remains_over.then(|| (convergence_filter(), None, None))
            } else if remains_over {
                self.announced = true;
                Some((
                    convergence_filter(),
                    Some(Nudge("⏳ Context budget nearly full. Stop investigating — record any remaining findings now with add_review_comment/add_comment, then call `finish` with your overall verdict. (The investigation tools are no longer available.)".into())),
                    Some(serde_json::json!({"reason": "Context budget nearly full"})),
                ))
            } else {
                None
            };
            vec![PolicyAction::TrimHistory {
                target_tokens: target,
                convergence,
            }]
        }
    }

    pub struct WindDown {
        max_turns: usize,
        max_batches: usize,
        announced: bool,
        disabled: bool,
        assembly_filter: TurnFilter,
    }

    impl WindDown {
        #[must_use]
        pub fn new(max_turns: usize, max_batches: usize) -> Self {
            Self {
                max_turns,
                max_batches,
                announced: false,
                disabled: false,
                assembly_filter: TurnFilter::all(),
            }
        }

        #[must_use]
        pub fn disabled(mut self, disabled: bool) -> Self {
            self.disabled = disabled;
            self
        }

        /// Apply an assembly-owned restriction whenever convergence is active (for example, the
        /// review diff-absent gate for inline-only effects).
        #[must_use]
        pub fn with_filter(mut self, filter: TurnFilter) -> Self {
            self.assembly_filter = filter;
            self
        }
    }

    impl TurnPolicy for WindDown {
        fn name(&self) -> &'static str {
            "wind_down"
        }

        fn before_turn(&mut self, state: &TurnState<'_>) -> Vec<PolicyAction> {
            if self.disabled || state.converging {
                return Vec::new();
            }
            let boundary = winddown_turn(self.max_turns);
            let batches_spent = state.stats.batches >= self.max_batches;
            if state.turn < boundary && !batches_spent {
                return Vec::new();
            }
            let mut narrowed = convergence_filter();
            narrowed.narrow(&self.assembly_filter);
            let mut actions = Vec::new();
            if !self.announced {
                self.announced = true;
                let reason = if batches_spent && state.turn < boundary {
                    format!(
                        "Investigation batch budget spent ({}/{} batches)",
                        state.stats.batches, self.max_batches
                    )
                } else {
                    format!(
                        "Turn budget almost spent (turn {}/{})",
                        state.turn, self.max_turns
                    )
                };
                actions.push(PolicyAction::Record {
                    name: None,
                    detail: serde_json::json!({"reason": reason}),
                });
                actions.push(PolicyAction::Converge {
                    filter: narrowed,
                    nudge: Nudge(format!(
                        "⏳ {reason}. Stop investigating — record any remaining findings now with add_review_comment/add_comment, then call `finish` with your overall verdict. (The investigation tools are no longer available.)"
                    )),
                });
            } else {
                actions.push(PolicyAction::Narrow(narrowed));
            }
            actions
        }
    }

    pub struct ReadBudgets {
        max_files: usize,
        max_searches: usize,
        files_announced: bool,
        searches_announced: bool,
        disabled: bool,
    }

    impl ReadBudgets {
        #[must_use]
        pub fn new(max_files: usize, max_searches: usize) -> Self {
            Self {
                max_files,
                max_searches,
                files_announced: false,
                searches_announced: false,
                disabled: false,
            }
        }

        #[must_use]
        pub fn disabled(mut self, disabled: bool) -> Self {
            self.disabled = disabled;
            self
        }
    }

    impl TurnPolicy for ReadBudgets {
        fn name(&self) -> &'static str {
            "read_budgets"
        }

        fn before_turn(&mut self, state: &TurnState<'_>) -> Vec<PolicyAction> {
            if self.disabled || state.converging {
                return Vec::new();
            }
            let files_spent = state.stats.files_read >= self.max_files;
            let searches_spent = state.stats.searches >= self.max_searches;
            let mut actions = Vec::new();
            if files_spent {
                actions.push(PolicyAction::Narrow(
                    TurnFilter::all().without_kind(ToolKind::ReadOnly(ReadKind::File)),
                ));
                if !self.files_announced {
                    self.files_announced = true;
                    actions.push(PolicyAction::Record {
                        name: Some("read_file_budget"),
                        detail: serde_json::json!({"files_read": state.stats.files_read}),
                    });
                    actions.push(PolicyAction::Inject(Nudge(format!(
                        "📄 You've read {} files (the read_file budget). Stop opening files — work from what you have, record findings, and head toward `finish`.",
                        state.stats.files_read
                    ))));
                }
            }
            if searches_spent {
                actions.push(PolicyAction::Narrow(
                    TurnFilter::all().without_kind(ToolKind::ReadOnly(ReadKind::Retrieval)),
                ));
                if !self.searches_announced {
                    self.searches_announced = true;
                    actions.push(PolicyAction::Record {
                        name: Some("retrieval_budget"),
                        detail: serde_json::json!({"searches": state.stats.searches}),
                    });
                    actions.push(PolicyAction::Inject(Nudge(format!(
                        "🔎 You've run {} searches (the retrieval budget). Stop searching — record findings from what you've found and head toward `finish`.",
                        state.stats.searches
                    ))));
                }
            }
            actions
        }
    }

    pub struct TurnBudget {
        halfway: usize,
        announced: bool,
        disabled: bool,
    }

    impl TurnBudget {
        #[must_use]
        pub fn new(max_turns: usize) -> Self {
            Self {
                halfway: max_turns / 2,
                announced: false,
                disabled: false,
            }
        }

        #[must_use]
        pub fn disabled(mut self, disabled: bool) -> Self {
            self.disabled = disabled;
            self
        }
    }

    impl TurnPolicy for TurnBudget {
        fn name(&self) -> &'static str {
            "halfway"
        }

        fn before_turn(&mut self, state: &TurnState<'_>) -> Vec<PolicyAction> {
            if self.disabled
                || self.announced
                || state.converging
                || self.halfway == 0
                || state.turn < self.halfway
            {
                return Vec::new();
            }
            self.announced = true;
            vec![
                PolicyAction::Record {
                    name: None,
                    detail: serde_json::json!({}),
                },
                PolicyAction::Inject(Nudge("You're past halfway on your turn budget — start converging: record what you've found and head toward `finish`.".into())),
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winddown_boundaries_match_the_legacy_contract() {
        assert_eq!(winddown_turn(1), 1);
        assert_eq!(winddown_turn(2), 1);
        assert_eq!(winddown_turn(5), 3);
        assert_eq!(winddown_turn(40), 36);
    }

    #[test]
    fn trim_preserves_recent_messages_and_call_pairing() {
        let mut messages = vec![
            ChatMessage::tool("old", "x".repeat(4_000)),
            ChatMessage::assistant(AssistantTurn {
                content: None,
                tool_calls: Vec::new(),
            }),
            ChatMessage::tool("new", "y".repeat(4_000)),
        ];
        let trimmed = trim_tool_history(&mut messages, &[], 10);
        assert_eq!(trimmed, 1);
        assert_eq!(messages[0].tool_call_id.as_deref(), Some("old"));
        assert!(messages[0].content.as_deref().unwrap().contains("elided"));
        assert_eq!(messages[2].content.as_deref().unwrap(), "y".repeat(4_000));
    }

    #[test]
    fn context_estimate_counts_tool_schemas_and_calls() {
        let base = estimate_tokens(&[ChatMessage::user("12345678")], &[]);
        let with_tool = estimate_tokens(
            &[ChatMessage::user("12345678")],
            &[ToolSpec::function(
                "search",
                "description",
                serde_json::json!({"type":"object"}),
            )],
        );
        assert_eq!(base, 6);
        assert!(with_tool > base);
    }

    #[test]
    fn streaming_request_preserves_usage_options() {
        let messages = [ChatMessage::system("system")];
        let extra = serde_json::Map::new();
        let request = ChatRequest {
            model: "m",
            messages: &messages,
            tools: &[],
            tool_choice: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            stream: Some(true),
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
            extra: &extra,
        };
        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json["stream"], true);
        assert_eq!(json["stream_options"]["include_usage"], true);
        assert!(json.get("tools").is_none());
    }
}
