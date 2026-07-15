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
