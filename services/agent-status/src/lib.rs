//! The live per-review **status projection** — a read-only view of a running agent loop's progress
//! (RFC-0007 slice 5, ADR-0085 "a live per-review status API").
//!
//! This crate is the status **mechanism**, deliberately **host-agnostic**: it holds no notion of
//! `run-once` vs `serve`. Today the `run-once` host (`agent-runner`) runs the [`server`] alongside
//! the review loop; a future `serve` host reuses the exact same projection + server without change.
//! The `serve` host *topology* stays gated on the measurement in ticket #358 — this crate is the
//! non-gated half.
//!
//! ## What it is (and is not)
//!
//! - It is a **projection**: a shared observable state ([`StatusHandle`]) updated as the loop runs,
//!   plus a read-only HTTP surface ([`server`]) that returns a [`StatusSnapshot`].
//! - The loop-sourced fields (turn, current/last tool name, findings recorded) are fed by a
//!   [`StatusSink`] that **wraps** the host's real [`TranscriptSink`]
//!   and forwards every event **unchanged** — so the loop's behaviour and the ADR-0034 transcript
//!   contract are untouched (the tap is a pure read-only tee). Fields the sink can't see — the coarse
//!   [`Phase`] and token usage — are set by the host through [`StatusHandle`] setters.
//! - It exposes **only progress metadata**. Never the diff, file contents, finding text bodies,
//!   secrets, or env — see [`StatusSnapshot`]. That is a credential-safety invariant, asserted in
//!   tests.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lci_agent_loop::{TranscriptEvent, TranscriptSink};
use serde::Serialize;
use uuid::Uuid;

pub mod server;

pub use server::{StatusServerConfig, serve, spawn};

/// Coarse lifecycle position of a run. **Not** sourced from the loop sink (the sink only sees turns,
/// tools, and findings) — the host sets it via [`StatusHandle::set_phase`] as it walks the task
/// lifecycle. Ordered roughly as the run progresses, but callers may skip phases (an already-indexed
/// review goes straight to [`Phase::Reviewing`]).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Bootstrapping: config resolved, checkout in progress, before any indexing or review work.
    #[default]
    Starting,
    /// Building the code graph + embeddings (`index` mode, or a cold repo before review).
    Indexing,
    /// The deterministic SAST pass over the PR's changed files (ADR-0061), before the agent runs.
    Sast,
    /// The review agent loop is running (investigating + recording findings).
    Reviewing,
    /// Flushing buffered findings / posting the grouped review (finalize).
    Finalizing,
    /// The run is complete (terminal — success or handled failure).
    Done,
}

/// A credential-safe, immutable snapshot of a run's live progress — the exact JSON body the status
/// endpoint returns. It carries **only** progress metadata:
///
/// - `task_id` — which task this is.
/// - `phase` — coarse lifecycle position ([`Phase`]).
/// - `turn` — current (0-based) turn index the loop has reached.
/// - `last_tool` — the **name only** of the current/last tool call (never its arguments or result).
/// - `findings_recorded` — count of findings recorded so far (successful record-tool calls).
/// - `prompt_tokens` / `completion_tokens` — token usage reported so far (host-fed, best-effort).
/// - `elapsed_secs` — wall-clock seconds since the projection started.
///
/// It NEVER contains the diff, file contents, finding text bodies, secrets, or environment — the
/// `serde` field set is the whole surface, and `test`s assert it stays metadata-only.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StatusSnapshot {
    pub task_id: Uuid,
    pub phase: Phase,
    pub turn: usize,
    pub last_tool: Option<String>,
    pub findings_recorded: usize,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub elapsed_secs: u64,
}

/// The mutable state behind [`StatusHandle`]. Guarded by a `Mutex` (reads are tiny and infrequent —
/// one HTTP poll at a time — so a plain mutex is simpler than an `RwLock` and never contended by the
/// single-threaded sink writes).
#[derive(Debug)]
struct StatusState {
    task_id: Uuid,
    phase: Phase,
    turn: usize,
    last_tool: Option<String>,
    findings_recorded: usize,
    prompt_tokens: u64,
    completion_tokens: u64,
    started_at: Instant,
}

/// A cloneable handle to a run's live status. Cheap to clone (`Arc`); the [`StatusSink`], the host's
/// phase/token updates, and the HTTP server all share one handle. All methods are non-blocking and
/// never fail — a poisoned mutex is recovered in place, because a status projection must never take
/// down the run it observes.
#[derive(Clone, Debug)]
pub struct StatusHandle(Arc<Mutex<StatusState>>);

impl StatusHandle {
    /// Start a fresh projection for `task_id`, phase [`Phase::Starting`], elapsed clock at zero.
    #[must_use]
    pub fn new(task_id: Uuid) -> Self {
        Self(Arc::new(Mutex::new(StatusState {
            task_id,
            phase: Phase::Starting,
            turn: 0,
            last_tool: None,
            findings_recorded: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            started_at: Instant::now(),
        })))
    }

    fn with<T>(&self, f: impl FnOnce(&mut StatusState) -> T) -> T {
        // Recover from a poisoned lock rather than panic: the projection is best-effort and must
        // never abort the run it merely observes.
        let mut guard = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut guard)
    }

    /// Set the coarse lifecycle [`Phase`] (host-driven; the sink never touches this).
    pub fn set_phase(&self, phase: Phase) {
        self.with(|state| state.phase = phase);
    }

    /// Record cumulative token usage reported so far (host-fed from the model telemetry side-channel;
    /// the loop sink can't see token counts). Monotonic in practice; set as totals, not deltas.
    pub fn observe_usage(&self, prompt_tokens: u64, completion_tokens: u64) {
        self.with(|state| {
            state.prompt_tokens = prompt_tokens;
            state.completion_tokens = completion_tokens;
        });
    }

    /// Apply one loop [`TranscriptEvent`] to the projection: advance the turn index, remember the
    /// current tool name, and count a finding when a successful record-tool call lands. `finding_tools`
    /// names the tools whose successful call counts as a recorded finding (e.g. `add_review_comment`),
    /// kept caller-supplied so this crate stays assembly-agnostic.
    fn apply_event(&self, event: &TranscriptEvent, finding_tools: &HashSet<String>) {
        self.with(|state| match event {
            // Turn indices arrive monotonically; `max` guards against any out-of-order record.
            TranscriptEvent::Assistant { turn, .. } | TranscriptEvent::Policy { turn, .. } => {
                state.turn = state.turn.max(*turn);
            }
            TranscriptEvent::Tool {
                turn,
                call,
                outcome,
            } => {
                state.turn = state.turn.max(*turn);
                let name = &call.function.name;
                state.last_tool = Some(name.clone());
                // A finding is "recorded" only when the record tool actually succeeded — a refused or
                // errored call (`Abort`/`Finish` carry no finding) must not inflate the count.
                if finding_tools.contains(name)
                    && matches!(outcome, lci_agent_types::ToolOutcome::Continue(_))
                {
                    state.findings_recorded += 1;
                }
            }
        });
    }

    /// A point-in-time [`StatusSnapshot`] — what the HTTP endpoint serves. Computes `elapsed_secs`
    /// from the projection's start instant.
    #[must_use]
    pub fn snapshot(&self) -> StatusSnapshot {
        self.with(|state| StatusSnapshot {
            task_id: state.task_id,
            phase: state.phase,
            turn: state.turn,
            last_tool: state.last_tool.clone(),
            findings_recorded: state.findings_recorded,
            prompt_tokens: state.prompt_tokens,
            completion_tokens: state.completion_tokens,
            elapsed_secs: state.started_at.elapsed().as_secs(),
        })
    }
}

/// A [`TranscriptSink`] that **tees** the loop's events into a [`StatusHandle`] and then forwards each
/// one **unchanged** to the host's real sink. Installing it is behaviour-neutral: the wrapped sink
/// receives exactly the events it would have without the tap (proven by the parity test), so the loop
/// and the ADR-0034 transcript are untouched.
pub struct StatusSink {
    handle: StatusHandle,
    inner: Box<dyn TranscriptSink>,
    finding_tools: Arc<HashSet<String>>,
}

impl StatusSink {
    /// Wrap `inner`, projecting into `handle`. `finding_tools` names the tools whose successful call is
    /// counted as a recorded finding (the review assembly passes `add_review_comment`).
    #[must_use]
    pub fn new(
        handle: StatusHandle,
        inner: Box<dyn TranscriptSink>,
        finding_tools: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            handle,
            inner,
            finding_tools: Arc::new(finding_tools.into_iter().map(Into::into).collect()),
        }
    }
}

impl TranscriptSink for StatusSink {
    fn record(&mut self, entry: TranscriptEvent) {
        // Project first (by reference), then forward the owned event verbatim — the inner sink's view
        // is byte-identical to an untapped run.
        self.handle.apply_event(&entry, &self.finding_tools);
        self.inner.record(entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lci_agent_loop::{ChatMessage, TranscriptEvent};
    use lci_agent_types::{FunctionCallReq, ToolCallReq, ToolOutcome};

    fn tool_event(turn: usize, name: &str, outcome: ToolOutcome) -> TranscriptEvent {
        TranscriptEvent::Tool {
            turn,
            call: ToolCallReq {
                id: format!("{name}-{turn}"),
                kind: "function".into(),
                function: FunctionCallReq {
                    name: name.into(),
                    arguments: "{}".into(),
                },
                extra_content: None,
            },
            outcome,
        }
    }

    fn finding_tools() -> HashSet<String> {
        ["add_review_comment".to_string()].into_iter().collect()
    }

    #[test]
    fn assistant_and_policy_events_advance_the_turn_monotonically() {
        let handle = StatusHandle::new(Uuid::nil());
        let tools = finding_tools();
        handle.apply_event(
            &TranscriptEvent::Assistant {
                turn: 3,
                message: ChatMessage::assistant(lci_agent_types::AssistantTurn {
                    content: None,
                    tool_calls: Vec::new(),
                }),
            },
            &tools,
        );
        assert_eq!(handle.snapshot().turn, 3);
        // An out-of-order lower turn must never rewind the projection.
        handle.apply_event(
            &TranscriptEvent::Policy {
                turn: 1,
                name: "wind_down",
                detail: serde_json::json!({}),
            },
            &tools,
        );
        assert_eq!(handle.snapshot().turn, 3);
    }

    #[test]
    fn only_successful_record_tool_calls_increment_findings() {
        let handle = StatusHandle::new(Uuid::nil());
        let tools = finding_tools();
        // A successful record → +1, and the tool name is remembered.
        handle.apply_event(
            &tool_event(
                0,
                "add_review_comment",
                ToolOutcome::Continue("recorded".into()),
            ),
            &tools,
        );
        // A non-record tool updates last_tool but not the finding count.
        handle.apply_event(
            &tool_event(1, "read_file", ToolOutcome::Continue("...".into())),
            &tools,
        );
        // A record tool that FINISHED (terminal, no finding) must not count.
        handle.apply_event(
            &tool_event(2, "add_review_comment", ToolOutcome::Finish),
            &tools,
        );
        let snap = handle.snapshot();
        assert_eq!(snap.findings_recorded, 1);
        assert_eq!(snap.last_tool.as_deref(), Some("add_review_comment"));
        assert_eq!(snap.turn, 2);
    }

    #[test]
    fn phase_and_usage_are_host_fed() {
        let handle = StatusHandle::new(Uuid::nil());
        assert_eq!(handle.snapshot().phase, Phase::Starting);
        handle.set_phase(Phase::Reviewing);
        handle.observe_usage(1200, 340);
        let snap = handle.snapshot();
        assert_eq!(snap.phase, Phase::Reviewing);
        assert_eq!(snap.prompt_tokens, 1200);
        assert_eq!(snap.completion_tokens, 340);
    }

    #[test]
    fn snapshot_json_is_metadata_only() {
        // Credential-safety: the serialized surface is exactly the safe metadata keys — no diff, file
        // contents, finding bodies, tokens-beyond-counts, secrets, or env can ride along.
        let handle = StatusHandle::new(Uuid::nil());
        handle.set_phase(Phase::Reviewing);
        handle.apply_event(
            &tool_event(
                0,
                "add_review_comment",
                ToolOutcome::Continue("recorded".into()),
            ),
            &finding_tools(),
        );
        let value = serde_json::to_value(handle.snapshot()).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "completion_tokens",
                "elapsed_secs",
                "findings_recorded",
                "last_tool",
                "phase",
                "prompt_tokens",
                "task_id",
                "turn",
            ]
        );
    }
}
