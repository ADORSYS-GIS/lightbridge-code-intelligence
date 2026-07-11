//! Runtime-agnostic execution loop for Lightbridge agents.
//!
//! [`AgentLoop`] is the deterministic engine of a review run (companion doc §4): each turn builds a
//! request from the conversation + the policy-narrowed tool set, calls the model through one journaled
//! step, then dispatches the turn's tools (read-only calls batched into one step, writes/terminals each
//! their own). Durability, the model transport, the concrete tools, and the review-flavored policies
//! are all injected — the engine itself depends on none of them, so the same loop runs unchanged under
//! the Job host ([`lci_agent_step::Passthrough`]) and, later, the Restate host (R2).

mod budget;
mod model;
mod policy;
mod sink;

use std::collections::BTreeMap;

use futures::future::join_all;
use lci_agent_step::StepRuntime;
use lci_agent_tools::{
    DispatchRefusal, DispatchResult, ReadKind, ToolCx, ToolKind, ToolRegistry, TurnFilter,
};
use lci_agent_types::{
    AssistantTurn, ChatMessage, LoopOutcome, StepError, ToolOutcome, TranscriptEntry, step_names,
};
use serde_json::json;

pub use budget::{WINDDOWN_TOKEN_FRACTION, estimate_tokens, trim_tool_history, winddown_turn};
pub use model::{ChatRequest, ModelClient};
pub use policy::{
    PolicyAction, ReadBudgets, TurnBudget, TurnOutcome, TurnPolicy, TurnState, WindDown,
};
pub use sink::TranscriptSink;

/// The generic per-loop limits (companion doc §4). Review-specific clamps (e.g. the fast-tier turn
/// ceiling) are applied by the assembly *before* constructing this — the engine treats every field as
/// already-resolved.
#[derive(Clone, Copy, Debug)]
pub struct LoopLimits {
    pub max_turns: usize,
    pub max_batch_size: usize,
    pub max_batches: usize,
    pub max_files_read: usize,
    pub max_searches: usize,
    pub context_window: Option<usize>,
}

impl LoopLimits {
    /// The first turn index at which wind-down kicks in (see [`winddown_turn`]).
    #[must_use]
    pub fn winddown_turn(&self) -> usize {
        winddown_turn(self.max_turns)
    }

    /// The turn index for the soft halfway nudge.
    #[must_use]
    pub fn halfway(&self) -> usize {
        self.max_turns / 2
    }

    /// The concurrent read-only batch size, clamped to at least one (ADR-0042).
    #[must_use]
    pub fn batch_size(&self) -> usize {
        self.max_batch_size.max(1)
    }
}

/// Renders the model-facing steer for a refused tool call. The generic engine never invents
/// model-facing prose (companion doc §3.3); the review assembly injects its exact refusal text.
///
/// `Sync` as well as `Send`: the renderer is shared by reference into the concurrent read-only batch
/// step, and `&T` is only `Send` when `T: Sync`.
pub type RefusalRenderer = Box<dyn Fn(&DispatchRefusal) -> String + Send + Sync>;

fn default_refusal(refusal: &DispatchRefusal) -> String {
    match refusal {
        DispatchRefusal::NotOffered { tool_name } => {
            format!("The `{tool_name}` tool is not available on this turn.")
        }
        DispatchRefusal::MissingCallId { tool_name } => {
            format!("The `{tool_name}` call is missing an id and was not run.")
        }
    }
}

fn resolve(
    result: DispatchResult,
    refusal: &(dyn Fn(&DispatchRefusal) -> String + Send),
) -> ToolOutcome {
    match result {
        DispatchResult::Completed(outcome) => outcome,
        DispatchResult::Refused(refused) => ToolOutcome::Continue(refusal(&refused)),
    }
}

fn assistant_message(turn: &AssistantTurn) -> ChatMessage {
    ChatMessage {
        role: "assistant".to_string(),
        content: turn.content.clone(),
        tool_calls: turn.tool_calls.clone(),
        tool_call_id: None,
    }
}

/// The model-visible content of a tool result fed back into the conversation. `Continue` carries the
/// tool's own text; the terminal outcomes end the loop, so their placeholder is rarely re-sent.
fn outcome_text(outcome: &ToolOutcome) -> String {
    match outcome {
        ToolOutcome::Continue(text) => text.clone(),
        ToolOutcome::Finish => "finished".to_string(),
        ToolOutcome::Abort(reason) => reason.clone(),
    }
}

/// The runtime-agnostic agent engine. Monomorphized per host over the durability runtime `R` and the
/// model client `M`; the tool registry, policies, and transcript sink are dynamically dispatched.
pub struct AgentLoop<R, M> {
    runtime: R,
    model: M,
    tools: ToolRegistry,
    policies: Vec<Box<dyn TurnPolicy>>,
    sink: Box<dyn TranscriptSink>,
    limits: LoopLimits,
    refusal: RefusalRenderer,
}

impl<R, M> AgentLoop<R, M>
where
    R: StepRuntime,
    M: ModelClient,
{
    /// Assemble a loop. The refusal renderer defaults to a neutral message; a review host overrides
    /// it with [`AgentLoop::with_refusal`].
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
            refusal: Box::new(default_refusal),
        }
    }

    /// Replace the refusal renderer with the assembly's exact model-facing steer.
    #[must_use]
    pub fn with_refusal(
        mut self,
        refusal: impl Fn(&DispatchRefusal) -> String + Send + Sync + 'static,
    ) -> Self {
        self.refusal = Box::new(refusal);
        self
    }

    /// Drive the loop to a terminal outcome. `seed` is the initial conversation (system + user
    /// messages); `cx` carries the tool context (task id + workspace). Every effect runs inside a
    /// named [`StepRuntime`] step so a durable host can replay the run at the last completed boundary.
    pub async fn run(
        &mut self,
        cx: &ToolCx<'_>,
        seed: Vec<ChatMessage>,
    ) -> Result<LoopOutcome, StepError> {
        let mut messages = seed;
        let base_specs = self.tools.view(&TurnFilter::all()).specs().to_vec();
        let mut batches = 0usize;
        let mut files_read = 0usize;
        let mut searches = 0usize;

        for turn in 0..self.limits.max_turns {
            // ---- derive this turn's budget signals (engine-owned, pure over journaled history) ----
            let batches_spent = batches >= self.limits.max_batches;
            let mut tokens_spent = false;
            if let Some(window) = self.limits.context_window {
                let target = (window as f64 * WINDDOWN_TOKEN_FRACTION) as usize;
                let mut est = estimate_tokens(&messages, &base_specs);
                if est > target {
                    let trimmed = trim_tool_history(&mut messages, &base_specs, target);
                    if trimmed > 0 {
                        est = estimate_tokens(&messages, &base_specs);
                        self.sink.record(TranscriptEntry::Policy {
                            turn,
                            name: "context_trim".to_string(),
                            detail: json!({ "trimmed": trimmed }),
                        });
                    }
                }
                tokens_spent = est >= target;
            }
            let in_winddown = turn >= self.limits.winddown_turn() || batches_spent || tokens_spent;
            let files_spent = files_read >= self.limits.max_files_read;
            let searches_spent = searches >= self.limits.max_searches;

            // ---- run policies: gather narrowings + injected messages + telemetry events ----
            let mut filter = TurnFilter::all();
            let mut injects: Vec<ChatMessage> = Vec::new();
            let mut force_finish = false;
            {
                let state = TurnState {
                    turn,
                    limits: &self.limits,
                    batches,
                    files_read,
                    searches,
                    in_winddown,
                    batches_spent,
                    tokens_spent,
                    files_spent,
                    searches_spent,
                };
                for policy in &mut self.policies {
                    for action in policy.before_turn(&state) {
                        match action {
                            PolicyAction::Narrow(narrow) => filter.narrow(&narrow),
                            PolicyAction::Inject(message) => injects.push(message),
                            PolicyAction::Emit { name, detail } => {
                                self.sink.record(TranscriptEntry::Policy {
                                    turn,
                                    name: name.to_string(),
                                    detail,
                                });
                            }
                            PolicyAction::ForceFinish { .. } => force_finish = true,
                        }
                    }
                }
            }
            messages.extend(injects);

            // ---- the model call: one journaled step ----
            let offered_specs = self.tools.view(&filter).specs().to_vec();
            let assistant = {
                let model = &self.model;
                let request_messages = &messages;
                let request_specs = &offered_specs;
                self.runtime
                    .step(step_names::llm_turn(turn), async move || {
                        model
                            .complete(ChatRequest::new(request_messages, request_specs))
                            .await
                    })
                    .await?
            };
            self.sink.record(TranscriptEntry::Assistant {
                turn,
                assistant: assistant.clone(),
                model: None,
            });
            messages.push(assistant_message(&assistant));

            if assistant.tool_calls.is_empty() {
                // Prose-only turn: nothing to dispatch. The exhausted backstop bounds a run that
                // never calls a terminal tool.
                if force_finish {
                    return Ok(LoopOutcome::Exhausted);
                }
                continue;
            }
            let calls = assistant.tool_calls.clone();

            // ---- advance cumulative read budgets over every call the model made ----
            for call in &calls {
                match self.tools.kind_of(&call.function.name) {
                    Some(ToolKind::ReadOnly(ReadKind::File)) => files_read += 1,
                    Some(ToolKind::ReadOnly(ReadKind::Retrieval)) => searches += 1,
                    _ => {}
                }
            }

            let view = self.tools.view(&filter);
            let read_indices: Vec<usize> = calls
                .iter()
                .enumerate()
                .filter(|(_, call)| {
                    matches!(
                        view.kind_of(&call.function.name),
                        Some(ToolKind::ReadOnly(_))
                    )
                })
                .map(|(index, _)| index)
                .collect();
            if !read_indices.is_empty() {
                batches += 1;
            }

            // ---- concurrent read-only batch: ONE journaled step, ordered results for replay ----
            let mut batched: BTreeMap<usize, ToolOutcome> = BTreeMap::new();
            if !read_indices.is_empty() {
                let view_ref = &view;
                let calls_ref = &calls;
                let refusal_ref = self.refusal.as_ref();
                let read_indices_ref = &read_indices;
                let batch_size = self.limits.batch_size();
                let results: Vec<(usize, ToolOutcome)> = self
                    .runtime
                    .step(step_names::tools(turn), async move || {
                        let mut collected: Vec<(usize, ToolOutcome)> = Vec::new();
                        for chunk in read_indices_ref.chunks(batch_size) {
                            let futures = chunk.iter().map(|&index| async move {
                                (
                                    index,
                                    resolve(
                                        view_ref.dispatch(cx, &calls_ref[index]).await,
                                        refusal_ref,
                                    ),
                                )
                            });
                            collected.extend(join_all(futures).await);
                        }
                        Ok(collected)
                    })
                    .await?;
                batched.extend(results);
            }

            // ---- ordered consume: reads reuse the batch; writes/terminals each own a step ----
            let mut finished = false;
            let mut abort_reason: Option<String> = None;
            for (index, call) in calls.iter().enumerate() {
                let outcome = if let Some(outcome) = batched.remove(&index) {
                    outcome
                } else {
                    let view_ref = &view;
                    let refusal_ref = self.refusal.as_ref();
                    self.runtime
                        .step(step_names::write_tool(turn, &call.id), async move || {
                            Ok(resolve(view_ref.dispatch(cx, call).await, refusal_ref))
                        })
                        .await?
                };
                self.sink.record(TranscriptEntry::ToolResult {
                    turn,
                    call: call.clone(),
                    outcome: outcome.clone(),
                });
                messages.push(ChatMessage::tool(call.id.clone(), outcome_text(&outcome)));
                match &outcome {
                    ToolOutcome::Finish => finished = true,
                    ToolOutcome::Abort(reason) => {
                        if abort_reason.is_none() {
                            abort_reason = Some(reason.clone());
                        }
                    }
                    ToolOutcome::Continue(_) => {}
                }
            }

            // ---- after-turn bookkeeping ----
            {
                let outcome = TurnOutcome {
                    tool_calls: calls.len(),
                    finished,
                };
                let state = TurnState {
                    turn,
                    limits: &self.limits,
                    batches,
                    files_read,
                    searches,
                    in_winddown,
                    batches_spent,
                    tokens_spent,
                    files_spent,
                    searches_spent,
                };
                for policy in &mut self.policies {
                    policy.after_turn(&state, &outcome);
                }
            }

            // ---- terminal handling: abort wins over finish (matches the pre-extraction loop) ----
            if let Some(reason) = abort_reason {
                return Ok(LoopOutcome::Aborted { reason });
            }
            if finished {
                return Ok(LoopOutcome::Finished);
            }
            if force_finish {
                return Ok(LoopOutcome::Exhausted);
            }
        }
        Ok(LoopOutcome::Exhausted)
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentLoop, LoopLimits, PolicyAction, TranscriptSink, TurnPolicy, TurnState};
    use lci_agent_step::Passthrough;
    use lci_agent_tools::{
        BoxFuture, DispatchRefusal, ReadKind, ReplaySafety, RuntimeCaps, Tool, ToolCx, ToolKind,
        ToolRegistry, TurnFilter, Workspace, WorkspaceError,
    };
    use lci_agent_types::{
        AssistantTurn, ChatMessage, FunctionCallReq, LoopOutcome, StepError, ToolCallReq,
        ToolOutcome, ToolSpec, TranscriptEntry,
    };
    use std::path::Path;
    use std::sync::Mutex;

    // A scripted model that hands back one canned assistant turn per call, in order.
    struct ScriptModel {
        turns: Mutex<std::collections::VecDeque<AssistantTurn>>,
    }
    impl ScriptModel {
        fn new(turns: Vec<AssistantTurn>) -> Self {
            Self {
                turns: Mutex::new(turns.into()),
            }
        }
    }
    impl super::ModelClient for ScriptModel {
        async fn complete(
            &self,
            _request: super::ChatRequest<'_>,
        ) -> Result<AssistantTurn, StepError> {
            self.turns
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| StepError::terminal("script exhausted"))
        }
    }

    #[derive(Default)]
    struct VecSink {
        entries: std::sync::Arc<Mutex<Vec<TranscriptEntry>>>,
    }
    impl TranscriptSink for VecSink {
        fn record(&mut self, entry: TranscriptEntry) {
            self.entries.lock().unwrap().push(entry);
        }
    }

    struct CannedTool {
        spec: ToolSpec,
        kind: ToolKind,
        outcome: ToolOutcome,
    }
    impl Tool for CannedTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }
        fn kind(&self) -> ToolKind {
            self.kind
        }
        fn replay(&self) -> ReplaySafety {
            ReplaySafety::ReadOnly
        }
        fn call<'a>(
            &'a self,
            _cx: &'a ToolCx<'a>,
            _call: &'a ToolCallReq,
        ) -> BoxFuture<'a, ToolOutcome> {
            Box::pin(async move { self.outcome.clone() })
        }
    }

    struct Root;
    impl Workspace for Root {
        fn root(&self) -> BoxFuture<'_, Result<&Path, WorkspaceError>> {
            Box::pin(async { Ok(Path::new("/tmp")) })
        }
    }

    fn tool(name: &str, kind: ToolKind, outcome: ToolOutcome) -> std::sync::Arc<dyn Tool> {
        std::sync::Arc::new(CannedTool {
            spec: ToolSpec::function(name, "t", serde_json::json!({"type": "object"})),
            kind,
            outcome,
        })
    }

    fn call(name: &str) -> ToolCallReq {
        ToolCallReq {
            id: format!("call-{name}"),
            kind: "function".into(),
            function: FunctionCallReq {
                name: name.into(),
                arguments: "{}".into(),
            },
            extra_content: None,
        }
    }

    fn assistant(calls: Vec<ToolCallReq>) -> AssistantTurn {
        AssistantTurn {
            content: None,
            tool_calls: calls,
        }
    }

    fn limits(max_turns: usize) -> LoopLimits {
        LoopLimits {
            max_turns,
            max_batch_size: 8,
            max_batches: 100,
            max_files_read: 100,
            max_searches: 100,
            context_window: None,
        }
    }

    #[tokio::test]
    async fn a_finish_call_ends_the_loop_and_records_the_transcript() {
        let mut registry = ToolRegistry::new();
        registry
            .register(
                tool("finish", ToolKind::Terminal, ToolOutcome::Finish),
                RuntimeCaps::default(),
            )
            .unwrap();
        let sink = VecSink::default();
        let entries = sink.entries.clone();
        let mut agent = AgentLoop::new(
            Passthrough,
            ScriptModel::new(vec![assistant(vec![call("finish")])]),
            registry,
            vec![],
            Box::new(sink),
            limits(5),
        );
        let cx = ToolCx {
            task_id: uuid::Uuid::nil(),
            workspace: &Root,
        };
        let outcome = agent
            .run(&cx, vec![ChatMessage::user("review")])
            .await
            .unwrap();
        assert_eq!(outcome, LoopOutcome::Finished);

        let entries = entries.lock().unwrap();
        assert!(matches!(
            entries[0],
            TranscriptEntry::Assistant { turn: 0, .. }
        ));
        assert!(matches!(
            entries[1],
            TranscriptEntry::ToolResult {
                outcome: ToolOutcome::Finish,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_prose_only_run_hits_the_exhausted_backstop() {
        let registry = ToolRegistry::new();
        let mut agent = AgentLoop::new(
            Passthrough,
            ScriptModel::new(vec![
                AssistantTurn {
                    content: Some("thinking".into()),
                    tool_calls: vec![],
                },
                AssistantTurn {
                    content: Some("still thinking".into()),
                    tool_calls: vec![],
                },
            ]),
            registry,
            vec![],
            Box::new(VecSink::default()),
            limits(2),
        );
        let cx = ToolCx {
            task_id: uuid::Uuid::nil(),
            workspace: &Root,
        };
        let outcome = agent
            .run(&cx, vec![ChatMessage::user("review")])
            .await
            .unwrap();
        assert_eq!(outcome, LoopOutcome::Exhausted);
    }

    #[tokio::test]
    async fn an_abort_call_ends_the_loop_with_its_reason() {
        let mut registry = ToolRegistry::new();
        registry
            .register(
                tool(
                    "abort",
                    ToolKind::Terminal,
                    ToolOutcome::Abort("no diff".into()),
                ),
                RuntimeCaps::default(),
            )
            .unwrap();
        let mut agent = AgentLoop::new(
            Passthrough,
            ScriptModel::new(vec![assistant(vec![call("abort")])]),
            registry,
            vec![],
            Box::new(VecSink::default()),
            limits(5),
        );
        let cx = ToolCx {
            task_id: uuid::Uuid::nil(),
            workspace: &Root,
        };
        let outcome = agent
            .run(&cx, vec![ChatMessage::user("review")])
            .await
            .unwrap();
        assert_eq!(
            outcome,
            LoopOutcome::Aborted {
                reason: "no diff".into()
            }
        );
    }

    #[tokio::test]
    async fn read_only_calls_advance_the_batch_budget_then_finish() {
        let mut registry = ToolRegistry::new();
        registry
            .register(
                tool(
                    "read_file",
                    ToolKind::ReadOnly(ReadKind::File),
                    ToolOutcome::Continue("file body".into()),
                ),
                RuntimeCaps::default(),
            )
            .unwrap();
        registry
            .register(
                tool("finish", ToolKind::Terminal, ToolOutcome::Finish),
                RuntimeCaps::default(),
            )
            .unwrap();
        let sink = VecSink::default();
        let entries = sink.entries.clone();
        // Cap batches at 1 so the second turn would wind down — but the model finishes first.
        let mut lim = limits(5);
        lim.max_batches = 1;
        let mut agent = AgentLoop::new(
            Passthrough,
            ScriptModel::new(vec![
                assistant(vec![call("read_file")]),
                assistant(vec![call("finish")]),
            ]),
            registry,
            vec![Box::new(super::WindDown::new())],
            Box::new(sink),
            lim,
        );
        let cx = ToolCx {
            task_id: uuid::Uuid::nil(),
            workspace: &Root,
        };
        let outcome = agent
            .run(&cx, vec![ChatMessage::user("review")])
            .await
            .unwrap();
        assert_eq!(outcome, LoopOutcome::Finished);

        // The read result was captured, and the second turn wound down (batch budget spent).
        let entries = entries.lock().unwrap();
        let read_result = entries.iter().any(|e| {
            matches!(e, TranscriptEntry::ToolResult { call, .. } if call.function.name == "read_file")
        });
        assert!(read_result);
        let wind_down = entries
            .iter()
            .any(|e| matches!(e, TranscriptEntry::Policy { name, .. } if name == "wind_down"));
        assert!(
            wind_down,
            "batch budget spent should trigger wind-down on turn 1"
        );
    }

    // A policy that offers only the named tool every turn — to drive the refusal path.
    struct NarrowTo(&'static str);
    impl TurnPolicy for NarrowTo {
        fn name(&self) -> &'static str {
            "narrow_to"
        }
        fn before_turn(&mut self, _state: &TurnState<'_>) -> Vec<PolicyAction> {
            vec![PolicyAction::Narrow(TurnFilter::only_names([self.0]))]
        }
    }

    // A policy that forces the loop to end after the current turn.
    struct ForceFinishNow;
    impl TurnPolicy for ForceFinishNow {
        fn name(&self) -> &'static str {
            "force_finish_now"
        }
        fn before_turn(&mut self, _state: &TurnState<'_>) -> Vec<PolicyAction> {
            vec![PolicyAction::ForceFinish {
                reason: "test backstop",
            }]
        }
    }

    #[tokio::test]
    async fn a_non_offered_call_is_refused_with_the_assembly_supplied_steer() {
        let mut registry = ToolRegistry::new();
        registry
            .register(
                tool(
                    "read_file",
                    ToolKind::ReadOnly(ReadKind::File),
                    ToolOutcome::Continue("body".into()),
                ),
                RuntimeCaps::default(),
            )
            .unwrap();
        registry
            .register(
                tool("finish", ToolKind::Terminal, ToolOutcome::Finish),
                RuntimeCaps::default(),
            )
            .unwrap();
        let sink = VecSink::default();
        let entries = sink.entries.clone();
        let mut agent = AgentLoop::new(
            Passthrough,
            ScriptModel::new(vec![
                assistant(vec![call("read_file")]),
                assistant(vec![call("finish")]),
            ]),
            registry,
            vec![Box::new(NarrowTo("finish"))],
            Box::new(sink),
            limits(3),
        )
        .with_refusal(|refusal| match refusal {
            DispatchRefusal::NotOffered { tool_name } => format!("refused: {tool_name}"),
            DispatchRefusal::MissingCallId { tool_name } => format!("missing id: {tool_name}"),
        });
        let cx = ToolCx {
            task_id: uuid::Uuid::nil(),
            workspace: &Root,
        };
        let outcome = agent
            .run(&cx, vec![ChatMessage::user("review")])
            .await
            .unwrap();
        assert_eq!(outcome, LoopOutcome::Finished);

        let entries = entries.lock().unwrap();
        let refused = entries.iter().any(|e| {
            matches!(
                e,
                TranscriptEntry::ToolResult { call, outcome: ToolOutcome::Continue(steer), .. }
                    if call.function.name == "read_file" && steer == "refused: read_file"
            )
        });
        assert!(
            refused,
            "a call to the non-offered read_file must carry the custom refusal steer"
        );
    }

    #[tokio::test]
    async fn a_force_finish_policy_exhausts_the_loop() {
        let mut registry = ToolRegistry::new();
        registry
            .register(
                tool(
                    "read_file",
                    ToolKind::ReadOnly(ReadKind::File),
                    ToolOutcome::Continue("body".into()),
                ),
                RuntimeCaps::default(),
            )
            .unwrap();
        let mut agent = AgentLoop::new(
            Passthrough,
            ScriptModel::new(vec![assistant(vec![call("read_file")])]),
            registry,
            vec![Box::new(ForceFinishNow)],
            Box::new(VecSink::default()),
            limits(5),
        );
        let cx = ToolCx {
            task_id: uuid::Uuid::nil(),
            workspace: &Root,
        };
        let outcome = agent
            .run(&cx, vec![ChatMessage::user("review")])
            .await
            .unwrap();
        assert_eq!(outcome, LoopOutcome::Exhausted);
    }

    #[tokio::test]
    async fn a_full_context_window_trims_old_output_and_records_the_event() {
        let mut registry = ToolRegistry::new();
        registry
            .register(
                tool("finish", ToolKind::Terminal, ToolOutcome::Finish),
                RuntimeCaps::default(),
            )
            .unwrap();
        let sink = VecSink::default();
        let entries = sink.entries.clone();
        let mut lim = limits(3);
        lim.context_window = Some(100); // target = 75 tokens; the seed dwarfs it.
        let mut agent = AgentLoop::new(
            Passthrough,
            ScriptModel::new(vec![assistant(vec![call("finish")])]),
            registry,
            vec![Box::new(super::WindDown::new())],
            Box::new(sink),
            lim,
        );
        let seed = vec![
            ChatMessage::user("review"),
            ChatMessage::tool("c1", "x".repeat(4000).as_str()),
            ChatMessage::tool("c2", "y".repeat(4000).as_str()),
            ChatMessage::tool("c3", "z".repeat(4000).as_str()),
        ];
        let cx = ToolCx {
            task_id: uuid::Uuid::nil(),
            workspace: &Root,
        };
        let outcome = agent.run(&cx, seed).await.unwrap();
        assert_eq!(outcome, LoopOutcome::Finished);

        let entries = entries.lock().unwrap();
        assert!(
            entries.iter().any(|e| matches!(
                e,
                TranscriptEntry::Policy { name, .. } if name == "context_trim"
            )),
            "an over-full context window must record a context_trim event",
        );
    }
}
