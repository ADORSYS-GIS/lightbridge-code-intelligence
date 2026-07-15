//! ADR-0087 `CheckpointRuntime`: the third `StepRuntime` implementation, which makes the agent loop
//! **resume from storage** instead of restarting from turn 0 on a pod death.
//!
//! `step(name, f)` asks the journal whether a result already exists for this step. If it does, the
//! stored result is returned and the effect is **not** re-executed; otherwise `f` runs, its result is
//! journaled, and returned. On a requeue of the SAME `run_epoch` the loop re-runs from turn 0 but each
//! step replays from storage until the first gap, then continues live — the resume the k8s Job model
//! never had, entirely inside the isolated execution unit (RFC-0007), the agent still DB-less
//! (it journals THROUGH the mediated internal API, ADR-0002/0037).
//!
//! This lives in `agent-clients` — a **host** crate that already owns the internal-API client — so the
//! dep-light agent crates (`agent-step`/`agent-loop`) stay free of any store/transport machinery
//! (ADR-0083). The generic `step<T, F>` is reconciled with a non-generic, erased [`DurableStepStore`]
//! (over `serde_json::Value`), so the store can be a trait object-shaped seam with a real HTTP impl
//! (prod) and an in-memory impl (tests).
#![allow(async_fn_in_trait)] // Mirrors the agent-step seam: native AFIT is the accepted host boundary.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lci_agent_step::{AwaitableId, Passthrough, StepError, StepRuntime};
use lci_agent_types::StepName;
use serde::Serialize;
use serde::de::DeserializeOwned;
use uuid::Uuid;

use crate::ControlPlaneClient;

/// The journal a [`CheckpointRuntime`] reads and writes. Erased over `serde_json::Value` so the
/// runtime's generic `step<T>` composes with a single non-generic store seam (the ADR-0082
/// boxed/erased reconciliation). Scoped to one run: an implementation carries the `(task_id,
/// run_epoch)` identity, so callers key only by [`StepName`].
pub trait DurableStepStore: Send + Sync {
    /// The stored result for `step`, or `None` if it has not run yet (the replay gap where the loop
    /// continues live).
    async fn fetch(&self, step: &StepName) -> Result<Option<serde_json::Value>, StepError>;

    /// Journal `result` (with its `content_hash`) for `step`. Idempotent on the run+step key.
    async fn upsert(
        &self,
        step: &StepName,
        result: &serde_json::Value,
        content_hash: &str,
    ) -> Result<(), StepError>;
}

/// Share one store across the two runtimes of a crash+resume (or two `serve` replicas) via `Arc`.
impl<T: DurableStepStore> DurableStepStore for Arc<T> {
    async fn fetch(&self, step: &StepName) -> Result<Option<serde_json::Value>, StepError> {
        (**self).fetch(step).await
    }

    async fn upsert(
        &self,
        step: &StepName,
        result: &serde_json::Value,
        content_hash: &str,
    ) -> Result<(), StepError> {
        (**self).upsert(step, result, content_hash).await
    }
}

/// Content hash of a journaled result (ADR-0087 C3): `sha256` over its canonical JSON bytes
/// (`serde_json::Value`'s map keys are ordered, so the encoding is deterministic).
#[must_use]
pub fn content_hash(value: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(value.to_string().as_bytes());
    hex::encode(hasher.finalize())
}

/// The durable, replay-on-requeue `StepRuntime` (ADR-0087). Generic over its [`DurableStepStore`], so
/// the same runtime backs the HTTP store (prod) and the in-memory store (tests).
pub struct CheckpointRuntime<S> {
    store: S,
}

impl<S> CheckpointRuntime<S> {
    #[must_use]
    pub fn new(store: S) -> Self {
        Self { store }
    }
}

impl<S: DurableStepStore> StepRuntime for CheckpointRuntime<S> {
    async fn step<T, F>(&self, name: StepName, f: F) -> Result<T, StepError>
    where
        T: Serialize + DeserializeOwned + Send + 'static,
        F: AsyncFnOnce() -> Result<T, StepError> + Send,
    {
        // 1) Replay: if this step already ran, return its journaled result — do NOT re-execute `f`.
        match self.store.fetch(&name).await {
            Ok(Some(value)) => match serde_json::from_value::<T>(value) {
                Ok(replayed) => return Ok(replayed),
                Err(error) => {
                    // Contract drift (ADR-0087 C1): a stored result no longer rehydrates into this
                    // step's type. Tolerate it like a missing-suffix gap — fall through and run live
                    // rather than fail the run.
                    tracing::warn!(step = %name, %error, "durable step did not rehydrate; running live");
                }
            },
            Ok(None) => {}
            Err(error) => {
                // Never fail the loop on a journal-read blip: treat it as a miss and run live (the
                // accepted at-least-once window). CheckpointRuntime only ever ADDS journaling; it
                // introduces no failure mode `Passthrough` doesn't have.
                tracing::warn!(step = %name, %error, "durable step fetch failed; running live");
            }
        }

        // 2) Gap: run the effect once, journal its result, return it.
        let result = f().await?;
        match serde_json::to_value(&result) {
            Ok(value) => {
                let hash = content_hash(&value);
                if let Err(error) = self.store.upsert(&name, &value, &hash).await {
                    // Best-effort journal: a persist failure is equivalent to a crash-before-persist —
                    // replay re-runs this one step (the at-least-once window), never a hard failure.
                    tracing::warn!(step = %name, %error, "journaling durable step failed; replay will re-run it");
                }
            }
            Err(error) => {
                tracing::warn!(step = %name, %error, "durable step result did not serialize; not journaled");
            }
        }
        Ok(result)
    }

    async fn sleep(&self, name: StepName, after: Duration) -> Result<(), StepError> {
        // Timers are not resume-critical (the replay skip covers llm_turn/tools/write_tool); match
        // Passthrough so behavior is identical off the journaled path.
        Passthrough.sleep(name, after).await
    }

    async fn awaitable<T>(
        &self,
        name: StepName,
    ) -> Result<
        (
            AwaitableId,
            impl std::future::Future<Output = Result<T, StepError>> + Send,
        ),
        StepError,
    >
    where
        T: Serialize + DeserializeOwned + Send + 'static,
    {
        // External resolution is ADR-0081's concern; keep the same honest pending-future seam as
        // Passthrough.
        Passthrough.awaitable::<T>(name).await
    }
}

/// The production [`DurableStepStore`]: journals through the control-plane internal API (ADR-0087),
/// so the agent pod keeps no DB credential. Holds one task's identity; `run_epoch` is resolved
/// server-side from the task row, so the agent never supplies it.
#[derive(Clone)]
pub struct ControlPlaneStepStore {
    client: ControlPlaneClient,
    task_id: Uuid,
}

impl ControlPlaneStepStore {
    #[must_use]
    pub fn new(client: ControlPlaneClient, task_id: Uuid) -> Self {
        Self { client, task_id }
    }
}

impl DurableStepStore for ControlPlaneStepStore {
    async fn fetch(&self, step: &StepName) -> Result<Option<serde_json::Value>, StepError> {
        match self.client.fetch_step(self.task_id, step.as_str()).await {
            Ok(Some(stored)) => Ok(Some(stored.result)),
            Ok(None) => Ok(None),
            // Transport errors are transient: the runtime downgrades a fetch error to "run live".
            Err(error) => Err(StepError::transient(error, None)),
        }
    }

    async fn upsert(
        &self,
        step: &StepName,
        result: &serde_json::Value,
        content_hash: &str,
    ) -> Result<(), StepError> {
        self.client
            .upsert_step(self.task_id, step.as_str(), result, content_hash)
            .await
            .map_err(|error| StepError::transient(error, None))
    }
}

/// An in-memory [`DurableStepStore`] modelling the `durable_step` rows of one `(task_id, run_epoch)`.
/// Reusing one instance across two runtimes IS "the same run_epoch requeued". Used by the resume
/// tests (no DB, no control plane) and available to any in-process host.
#[derive(Default)]
pub struct InMemoryStepStore {
    entries: Mutex<HashMap<String, (serde_json::Value, String)>>,
}

impl InMemoryStepStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many steps are journaled — the resume-state footprint.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .expect("in-memory step store mutex")
            .len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drop the whole journal — the in-process analogue of the control plane's purge-on-success
    /// (`finalize` deletes `WHERE task_id=? AND run_epoch=?`). After this, a resume re-runs every step.
    pub fn purge(&self) {
        self.entries
            .lock()
            .expect("in-memory step store mutex")
            .clear();
    }
}

impl DurableStepStore for InMemoryStepStore {
    async fn fetch(&self, step: &StepName) -> Result<Option<serde_json::Value>, StepError> {
        Ok(self
            .entries
            .lock()
            .expect("in-memory step store mutex")
            .get(step.as_str())
            .map(|(value, _)| value.clone()))
    }

    async fn upsert(
        &self,
        step: &StepName,
        result: &serde_json::Value,
        content_hash: &str,
    ) -> Result<(), StepError> {
        self.entries
            .lock()
            .expect("in-memory step store mutex")
            .insert(
                step.as_str().to_string(),
                (result.clone(), content_hash.to_string()),
            );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use lci_agent_loop::{
        AgentLoop, ChatMessage, Conversation, LoopLimits, LoopOutcome, RequestOptions,
        TranscriptEvent, TranscriptSink,
    };
    use lci_agent_testkit::{CapturingSink, ScriptedModel, StaticTool};
    use lci_agent_tools::{
        BoxFuture, ReadKind, ReplaySafety, RuntimeCaps, ToolCx, ToolKind, ToolRegistry, Workspace,
        WorkspaceError,
    };
    use lci_agent_types::{
        AssistantTurn, FunctionCallReq, StepError, ToolCallReq, ToolOutcome, ToolSpec,
        TurnTelemetry, step_names,
    };

    struct TmpWorkspace(std::path::PathBuf);
    impl Workspace for TmpWorkspace {
        fn root(&self) -> BoxFuture<'_, Result<&Path, WorkspaceError>> {
            Box::pin(async move { Ok(self.0.as_path()) })
        }
    }

    fn read_turn() -> AssistantTurn {
        AssistantTurn {
            content: None,
            tool_calls: vec![ToolCallReq {
                id: "read".into(),
                kind: "function".into(),
                function: FunctionCallReq {
                    name: "read_file".into(),
                    arguments: "{}".into(),
                },
                extra_content: None,
            }],
            ..Default::default()
        }
    }

    fn finish_turn() -> AssistantTurn {
        AssistantTurn {
            content: None,
            tool_calls: vec![ToolCallReq {
                id: "fin".into(),
                kind: "function".into(),
                function: FunctionCallReq {
                    name: "finish".into(),
                    arguments: "{}".into(),
                },
                extra_content: None,
            }],
            ..Default::default()
        }
    }

    /// Like [`read_turn`] but with telemetry attached — the shape a real `ChatClient` reply carries
    /// (#411/#417): reasoning text + token counts riding the turn itself, not a side-channel.
    fn read_turn_with_telemetry(reasoning: &str) -> AssistantTurn {
        AssistantTurn {
            telemetry: Some(TurnTelemetry {
                model: "m".into(),
                prompt_tokens: Some(100),
                completion_tokens: Some(20),
                reasoning_tokens: Some(15),
                reasoning: Some(reasoning.into()),
            }),
            ..read_turn()
        }
    }

    /// The `telemetry` recorded on the Nth `TranscriptEvent::Assistant` among `events` (turn index,
    /// not vec position — a run can also record `Tool`/`Policy` events between assistant turns).
    fn assistant_telemetry(events: &[TranscriptEvent], turn: usize) -> Option<&TurnTelemetry> {
        events.iter().find_map(|event| match event {
            TranscriptEvent::Assistant {
                turn: t, telemetry, ..
            } if *t == turn => telemetry.as_ref(),
            _ => None,
        })
    }

    /// A read-only `read_file` + a terminal `finish`, registered under replay-capable caps (which is
    /// what a `CheckpointRuntime` host declares: it replays completed steps and keys writes per call).
    fn registry() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        let caps = RuntimeCaps {
            replays_completed_steps: true,
            per_call_dedup: true,
        };
        registry
            .register(
                Arc::new(StaticTool::new(
                    ToolSpec::function("read_file", "read a file", serde_json::json!({})),
                    ToolKind::ReadOnly(ReadKind::File),
                    ReplaySafety::ReadOnly,
                    ToolOutcome::Continue("file contents".into()),
                )),
                caps,
            )
            .unwrap();
        registry
            .register(
                Arc::new(StaticTool::new(
                    ToolSpec::function("finish", "finish", serde_json::json!({})),
                    ToolKind::Terminal,
                    ReplaySafety::Idempotent,
                    ToolOutcome::Finish,
                )),
                caps,
            )
            .unwrap();
        registry
    }

    fn conversation() -> Conversation {
        Conversation::new(
            vec![ChatMessage::system("review"), ChatMessage::user("go")],
            RequestOptions {
                model: "m".into(),
                ..RequestOptions::default()
            },
        )
    }

    fn limits(max_turns: usize) -> LoopLimits {
        LoopLimits {
            max_turns,
            max_batch_size: 4,
            circuit_breaker_threshold: 0,
            no_tool_nudge: "use tools".into(),
        }
    }

    /// Drive the loop under `CheckpointRuntime`, returning both the outcome and every recorded
    /// transcript event — so a caller can inspect what a replayed turn actually carried (e.g. its
    /// `telemetry`), not just that the run finished.
    async fn drive(
        store: Arc<InMemoryStepStore>,
        model: ScriptedModel,
        max_turns: usize,
    ) -> (LoopOutcome, Vec<TranscriptEvent>) {
        let checkout = std::env::temp_dir();
        let workspace = TmpWorkspace(checkout);
        let cx = ToolCx {
            task_id: Uuid::nil(),
            workspace: &workspace,
        };
        let capturing = CapturingSink::default();
        let sink_handle = capturing.clone();
        let sink: Box<dyn TranscriptSink> = Box::new(capturing);
        let mut agent = AgentLoop::new(
            CheckpointRuntime::new(store),
            model,
            registry(),
            Vec::new(),
            sink,
            limits(max_turns),
        );
        let outcome = agent.run(conversation(), &cx).await.unwrap();
        (outcome, sink_handle.entries())
    }

    /// The ADR-0087 / #356 merge bar: drive the loop under `CheckpointRuntime` + an in-memory store,
    /// stop mid-run after turn N, then re-run with the SAME store (== same run_epoch). Assert the
    /// completed turns are served from the journal — the model is NOT re-invoked for turns 0..N — and
    /// the loop continues at turn N+1. Proves ZERO duplicate model calls on replay.
    #[tokio::test]
    async fn resume_serves_completed_turns_from_the_journal_with_zero_duplicate_model_calls() {
        let store = Arc::new(InMemoryStepStore::new());

        // Run 1: the pod processes 2 turns (both `read_file`), then "dies" — we bound it at 2 turns.
        // Each turn journals `llm_turn:{n}` (the model reply) and `tools:{n}` (the read batch).
        let run1_model = ScriptedModel::new([read_turn(), read_turn()]);
        let (out1, _) = drive(store.clone(), run1_model.clone(), 2).await;
        assert_eq!(
            out1,
            LoopOutcome::Exhausted,
            "run 1 stops after its 2 turns"
        );
        assert_eq!(
            run1_model.requests().len(),
            2,
            "run 1 called the model once per live turn"
        );
        // Journaled: llm_turn:0, tools:0, llm_turn:1, tools:1.
        assert_eq!(
            store.len(),
            4,
            "run 1 journaled both turns' llm + tools steps"
        );

        // Run 2: the SAME store is requeued (same run_epoch). Turns 0 and 1 must replay from the
        // journal — this fresh model is only ever asked about turn 2 onward. It finishes at turn 2.
        let run2_model = ScriptedModel::new([finish_turn()]);
        let (out2, _) = drive(store.clone(), run2_model.clone(), 5).await;
        assert_eq!(out2, LoopOutcome::Finished, "run 2 resumes and finishes");
        assert_eq!(
            run2_model.requests().len(),
            1,
            "ZERO duplicate model calls: turns 0..1 replayed from the journal, only turn 2 ran live"
        );
    }

    /// The #411/#417 regression: telemetry (reasoning/tokens) must survive a resume, not just the
    /// model-visible content/tool_calls. Before the fix, telemetry rode a side-channel populated only
    /// when the model closure actually ran — a replayed turn skipped the closure, so its `AssistantTurn`
    /// journaled fine but the turn's reasoning/tokens were unrecoverable after resume. Telemetry now
    /// rides ON the journaled `AssistantTurn` itself, so it replays with the turn by construction.
    #[tokio::test]
    async fn resume_preserves_telemetry_on_the_replayed_turn() {
        let store = Arc::new(InMemoryStepStore::new());

        // Run 1: one turn, carrying real telemetry (reasoning text + token counts) — the shape
        // `ChatClient::complete` actually returns. The pod "dies" right after.
        let run1_model = ScriptedModel::new([read_turn_with_telemetry("thinking about the diff")]);
        let (out1, events1) = drive(store.clone(), run1_model, 1).await;
        assert_eq!(out1, LoopOutcome::Exhausted, "run 1 stops after its turn");
        let live_telemetry = assistant_telemetry(&events1, 0)
            .expect("run 1's live turn carries telemetry")
            .clone();
        assert_eq!(
            live_telemetry.reasoning.as_deref(),
            Some("thinking about the diff")
        );

        // Run 2: the SAME store resumes. Turn 0 replays from the journal — a fresh model that would
        // return DIFFERENT telemetry if asked proves it never runs for turn 0.
        let run2_model = ScriptedModel::new([finish_turn()]);
        let (out2, events2) = drive(store.clone(), run2_model, 5).await;
        assert_eq!(out2, LoopOutcome::Finished, "run 2 resumes and finishes");

        let replayed_telemetry = assistant_telemetry(&events2, 0)
            .expect("the replayed turn still carries its original telemetry, not None");
        assert_eq!(
            replayed_telemetry, &live_telemetry,
            "telemetry on a replayed turn is byte-identical to what the live turn recorded"
        );
    }

    /// The read tool itself is a completed step: on replay `tools:{n}` is served from the journal, so
    /// the tool is NOT re-dispatched either — proving side-effectful steps aren't re-run on resume.
    #[tokio::test]
    async fn resume_does_not_re_dispatch_journaled_tool_steps() {
        struct CountingTool {
            spec: ToolSpec,
            calls: Arc<AtomicUsize>,
        }
        impl lci_agent_tools::Tool for CountingTool {
            fn spec(&self) -> &ToolSpec {
                &self.spec
            }
            fn kind(&self) -> ToolKind {
                ToolKind::ReadOnly(ReadKind::File)
            }
            fn replay(&self) -> ReplaySafety {
                ReplaySafety::ReadOnly
            }
            fn call<'a>(
                &'a self,
                _cx: &'a ToolCx<'a>,
                _call: &'a ToolCallReq,
            ) -> BoxFuture<'a, ToolOutcome> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { ToolOutcome::Continue("file contents".into()) })
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let store = Arc::new(InMemoryStepStore::new());
        let caps = RuntimeCaps {
            replays_completed_steps: true,
            per_call_dedup: true,
        };
        let build_registry = |calls: Arc<AtomicUsize>| {
            let mut registry = ToolRegistry::new();
            registry
                .register(
                    Arc::new(CountingTool {
                        spec: ToolSpec::function("read_file", "read", serde_json::json!({})),
                        calls,
                    }),
                    caps,
                )
                .unwrap();
            registry
                .register(
                    Arc::new(StaticTool::new(
                        ToolSpec::function("finish", "finish", serde_json::json!({})),
                        ToolKind::Terminal,
                        ReplaySafety::Idempotent,
                        ToolOutcome::Finish,
                    )),
                    caps,
                )
                .unwrap();
            registry
        };

        let workspace = TmpWorkspace(std::env::temp_dir());
        let cx = ToolCx {
            task_id: Uuid::nil(),
            workspace: &workspace,
        };

        // Run 1: one read turn, bounded at 1 turn — dispatches the tool exactly once and journals it.
        let mut agent1 = AgentLoop::new(
            CheckpointRuntime::new(store.clone()),
            ScriptedModel::new([read_turn()]),
            build_registry(calls.clone()),
            Vec::new(),
            Box::new(CapturingSink::default()) as Box<dyn TranscriptSink>,
            limits(1),
        );
        agent1.run(conversation(), &cx).await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "run 1 dispatched the read once"
        );

        // Run 2: resume with the same store; turn 0 replays from `tools:0` → the tool is NOT called
        // again; turn 1 finishes live.
        let mut agent2 = AgentLoop::new(
            CheckpointRuntime::new(store.clone()),
            ScriptedModel::new([finish_turn()]),
            build_registry(calls.clone()),
            Vec::new(),
            Box::new(CapturingSink::default()) as Box<dyn TranscriptSink>,
            limits(5),
        );
        let out = agent2.run(conversation(), &cx).await.unwrap();
        assert_eq!(out, LoopOutcome::Finished);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "resume served the journaled tool step — the effect was NOT re-executed"
        );
    }

    /// Purge-on-success (ADR-0087): after `finalize`, the run's journal is dropped. Modelled here by
    /// [`InMemoryStepStore::purge`]. Once purged, a resume re-invokes the model for every turn — the
    /// resume state is genuinely gone, not merely hidden.
    #[tokio::test]
    async fn purge_on_success_drops_the_journal_so_a_later_run_re_executes() {
        let store = Arc::new(InMemoryStepStore::new());
        drive(
            store.clone(),
            ScriptedModel::new([read_turn(), read_turn()]),
            2,
        )
        .await;
        assert_eq!(store.len(), 4, "the run journaled its steps");

        // finalize → purge-on-success.
        store.purge();
        assert!(store.is_empty(), "purge dropped the whole journal");

        // A subsequent run over the purged store re-executes turn 0 (no replay to serve it).
        let model = ScriptedModel::new([finish_turn()]);
        let (out, _) = drive(store.clone(), model.clone(), 5).await;
        assert_eq!(out, LoopOutcome::Finished);
        assert_eq!(
            model.requests().len(),
            1,
            "with the journal purged, turn 0 ran live again (state is gone, not hidden)"
        );
    }

    /// The runtime unit: a journaled step is served without re-running its closure; an un-journaled
    /// step runs once and is journaled.
    #[tokio::test]
    async fn step_runs_once_then_replays_from_the_store() {
        let store = Arc::new(InMemoryStepStore::new());
        let runtime = CheckpointRuntime::new(store.clone());
        let runs = AtomicUsize::new(0);

        let first = runtime
            .step(step_names::llm_turn(0), async || {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok::<_, StepError>(7_u32)
            })
            .await
            .unwrap();
        assert_eq!(first, 7);
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        // A second runtime over the same store replays without re-running the closure.
        let resumed = CheckpointRuntime::new(store.clone());
        let second = resumed
            .step(step_names::llm_turn(0), async || {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok::<_, StepError>(99_u32)
            })
            .await
            .unwrap();
        assert_eq!(
            second, 7,
            "served the journaled result, not the fresh closure"
        );
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "the closure did not re-run on replay"
        );
    }

    #[test]
    fn content_hash_is_stable_and_distinguishes_values() {
        let a = serde_json::json!({"x": 1, "y": 2});
        let b = serde_json::json!({"y": 2, "x": 1});
        assert_eq!(content_hash(&a), content_hash(&b), "key order is canonical");
        assert_ne!(content_hash(&a), content_hash(&serde_json::json!({"x": 2})));
    }
}
