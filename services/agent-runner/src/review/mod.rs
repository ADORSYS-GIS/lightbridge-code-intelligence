//! Code review — the native in-process agent (ADR-0026 + ADR-0037), now a thin **host** over the
//! extracted `lci-review-agent` assembly (R1e).
//!
//! The runner maps its [`ReviewConfig`] onto the review-agent param structs, builds the model client
//! (`lci_review_agent::model`), the seeded conversation (`lci_review_agent::prompt::build_messages`),
//! and the tool registry (`lci_review_agent::tools::tool_registry`, with ADR-0066 MCP discovery), then
//! drives [`lci_review_agent::flows::run_review`] over the current Kubernetes-Job runtime
//! ([`Passthrough`]). The agent investigates with retrieval tools and **acts via mediated write tools**
//! (`add_review_comment` / `add_comment` / `finish`); the control plane buffers those and flushes one
//! grouped review on finalize (ADR-0037). The former OpenCode subprocess was removed in #140 — this is
//! the only review path.
//!
//! Outcome model (#137): the run returns a [`ReviewOutcome`] — `Finished` (the model called `finish`),
//! `Exhausted` (the turn budget ran out while findings may be buffered), or `Aborted(reason)` (the
//! model called `abort`). **Only a true transport/loop failure returns `Err`.** The caller finalizes on
//! all three so buffered findings are never discarded. Run observability is logs-only (epic #459):
//! per-turn proof-of-work lines go to Loki as the run happens; there is no DB transcript.
//!
//! [`run_native_agent`] is a thin **host** over four extracted concerns (quality pass, no behaviour
//! change): [`model_client`] (building the LLM client + its starting log), [`tool_surface`] (resolving
//! what's offered — diff gate, per-tier allowlist, MCP discovery), [`telemetry`] (the run-start
//! snapshot + token-usage summation), and [`transcript`] (the per-turn proof-of-work log lines).

pub mod instructions;
pub mod opencode;
pub mod repo_config;

mod model_client;
mod telemetry;
mod tool_surface;
mod transcript;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use uuid::Uuid;

use lci_agent_clients::{
    CheckpointRuntime, ControlPlaneClient, ControlPlaneStepStore, EmbeddingsClient,
};
use lci_agent_loop::{Conversation, LoopOutcome, RequestOptions, TranscriptEvent, TranscriptSink};
use lci_agent_sast::SastConfig;
use lci_agent_status::StatusHandle;
use lci_agent_step::Passthrough;
use lci_agent_tools::{RuntimeCaps, ToolCx, TurnFilter};
use lci_review_agent::flows::{self, ReviewRunParams};
use lci_review_agent::prompt::{self, PrDiffRef, PromptConfig};
use lci_review_agent::tools::{ADD_REVIEW_COMMENT, SastToolConfig, tool_registry};

use crate::bootstrap::config::ReviewConfig;
use crate::clone::PrDiff;

/// How the agent loop ended (#137). Distinct from `Err`, which is reserved for a transport/loop failure
/// where the gateway was unreachable and nothing useful happened. The caller maps these to a visible
/// artifact on the PR:
/// - [`ReviewOutcome::Finished`] — the model called `finish`; finalize flushes the buffer.
/// - [`ReviewOutcome::Exhausted`] — the turn budget ran out with findings possibly still buffered;
///   the caller posts a truncation note then finalizes (so buffered findings are NOT discarded).
/// - [`ReviewOutcome::Aborted`] — the model called `abort`; the caller posts the reason then finalizes.
#[derive(Debug)]
pub enum ReviewOutcome {
    Finished,
    Exhausted,
    Aborted(String),
}

/// Whether to drive the loop under `CheckpointRuntime` (ADR-0087 durable replay) instead of the
/// default `Passthrough`. Opt-in via `LCI_DURABLE_REPLAY` (`1`/`true`/`yes`, case-insensitive); unset
/// or anything else keeps today's prod behavior. A run-once/agent-plane host env, so the dispatcher
/// can flip it per deployment without a code change.
fn durable_replay_enabled() -> bool {
    std::env::var("LCI_DURABLE_REPLAY")
        .map(|raw| {
            matches!(
                raw.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes"
            )
        })
        .unwrap_or(false)
}

/// A [`TranscriptSink`] that captures the loop's events so the host can emit the per-turn
/// proof-of-work log lines afterwards (even on error) and sum token usage. Cloneable handle: grab a
/// clone before boxing it into the loop, then lock `.0` directly to read the events once the run
/// returns — cloning the whole (possibly still-growing) event log via [`entries`](Self::entries) is
/// test-only; production reads hold the lock in place instead (the live status poller ticks every
/// second on it).
#[derive(Clone, Default)]
struct JobSink(Arc<Mutex<Vec<TranscriptEvent>>>);

impl JobSink {
    #[cfg(test)]
    fn entries(&self) -> Vec<TranscriptEvent> {
        // Recover from a poisoned mutex instead of panicking (consistent with `StatusHandle::with`
        // and the telemetry mutex below): a panic elsewhere holding this lock must not also crash
        // every subsequent read of the transcript-so-far.
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl TranscriptSink for JobSink {
    fn record(&mut self, entry: TranscriptEvent) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(entry);
    }
}

/// Run the native agent loop. The agent acts via the mediated write tools during the run. Returns a
/// [`ReviewOutcome`] describing how it ended (`Finished` / `Exhausted` / `Aborted`) — the caller turns
/// each into a visible PR artifact and finalizes the buffer in all three cases (#137). Only a true
/// transport/loop failure returns `Err`; in that case nothing is posted (but the run's per-turn
/// proof-of-work is still logged, even on error).
#[allow(clippy::too_many_arguments)]
pub async fn run_native_agent(
    review: &ReviewConfig,
    command: &str,
    diff: Option<&PrDiff>,
    // Repo-native agent instructions (ADR-0036), prior reviews (A, #137), and per-repo feedback memory
    // (M1, ADR-0044) — all injected into the prompt as untrusted context; `None` when absent.
    repo_instructions: Option<&str>,
    prior_reviews: Option<&str>,
    repo_memory: Option<&str>,
    // The resolved SAST config (ADR-0061), handed to the `run_sast` tool (ADR-0073) instead of driving a
    // pre-agent pass. `None` when SAST is off — the tool then simply isn't registered/offered.
    sast_config: Option<&SastConfig>,
    attribution: &[(String, String)],
    client: &ControlPlaneClient,
    embedder: &EmbeddingsClient,
    task_id: Uuid,
    // The checked-out repo root (the working tree under review). `read_file` reads from here,
    // path-sanitized to within it (epic #137).
    checkout_root: &Path,
    // Live status projection (RFC-0007 slice 5), `Some` only when the operator enabled the status API.
    // When set, the loop sink is teed through a `StatusSink` (behaviour-neutral) and token usage is fed
    // from the telemetry side-channel; `None` keeps today's path exactly (no wrapping, no feed).
    status: Option<&StatusHandle>,
) -> anyhow::Result<ReviewOutcome> {
    // ── Model client (ADR-0039) ──────────────────────────────────────────────────────────────────
    let chat = model_client::build_chat_client(review, attribution, task_id);
    let request_extra = chat.extra().clone();

    // ── Offered tool surface: diff gate + per-tier allowlist + ADR-0066 MCP discovery ───────────────
    let diff_present = diff.is_some();
    let (offered, dispatch_discovered) = tool_surface::resolve_offered_tools(
        review,
        diff_present,
        sast_config.is_some(),
        client,
        task_id,
    )
    .await;

    // ── Run-start telemetry (ADR-0034/0062/0066), recorded at run START ─────────────────────────────
    telemetry::submit_run_start_telemetry(client, task_id, review, &offered).await;

    // ── Seed the conversation ────────────────────────────────────────────────────────────────────
    let prompt_config = PromptConfig {
        system_prompt: review.system_prompt.clone(),
        max_diff_chars: review.max_diff_chars,
        context_window: review.context_window,
    };
    let diff_ref = diff.map(|pr| PrDiffRef {
        diff: &pr.diff,
        files: &pr.files,
    });
    let messages = prompt::build_messages(
        &prompt_config,
        command,
        diff_ref,
        repo_instructions,
        prior_reviews,
        repo_memory,
        // Retired/legacy path (`run_native_agent`, removed in a follow-up) — not worth wiring the new
        // ADR-0030 repo-config context block through code on its way out.
        None,
    );
    let initial_names: Vec<String> = offered
        .iter()
        .map(|spec| spec.function.name.clone())
        .collect();
    let conversation = Conversation::new(
        messages,
        RequestOptions {
            model: review.model.clone(),
            temperature: review.temperature,
            top_p: review.top_p,
            max_tokens: review.max_tokens,
            stream: review.stream.then_some(true),
            extra: request_extra,
        },
    )
    .with_filter(TurnFilter::only_names(initial_names));

    // Durable replay (ADR-0087) is opt-in and OFF by default, so prod keeps running `Passthrough`
    // (today's behavior). When enabled, the host runs `CheckpointRuntime` and declares replay-capable
    // caps so a completed write is journaled per call and served (not re-run) on resume.
    let durable_replay = durable_replay_enabled();
    let runtime_caps = if durable_replay {
        RuntimeCaps {
            replays_completed_steps: true,
            per_call_dedup: true,
        }
    } else {
        RuntimeCaps::default()
    };

    // ── Tool registry (built-ins + discovered + run_sast) ───────────────────────────────────────
    // Shared feed the `run_sast` tool pushes leads into as it scans (ADR-0073); `SastAnchorGate` (#305)
    // drains it mid-loop. Built once here so the same handle reaches both the tool (via `tool_registry`)
    // and the gate (via `params.sast_leads`, below).
    let sast_leads: lci_review_agent::policies::SastLeadSink = Arc::new(Mutex::new(Vec::new()));
    let sast_tool_config = sast_config.map(|config| SastToolConfig {
        config: config.clone(),
        changed_files: diff.map(|pr| pr.files.clone()).unwrap_or_default(),
        leads: Arc::clone(&sast_leads),
    });
    let registry = tool_registry(
        Arc::new(client.clone()),
        Arc::new(embedder.clone()),
        dispatch_discovered,
        runtime_caps,
        sast_tool_config,
        // Retired/legacy path (`run_native_agent`, removed in a follow-up) — not worth wiring the new
        // ADR-0030 severity filter through code on its way out.
        None,
    )
    .context("assembling review tool registry")?;

    let params = ReviewRunParams {
        max_turns: review.max_turns,
        max_batch_size: review.max_batch_size.max(1),
        max_batches: review.max_batches,
        max_files_read: review.max_files_read,
        max_searches: review.max_searches,
        max_coverage_bounces: review.max_coverage_bounces,
        circuit_breaker_threshold: review.resilience.circuit_breaker_threshold,
        context_window: review.context_window,
        diff_present,
        diff_files: diff.map(|pr| pr.files.clone()).unwrap_or_default(),
        sast_leads,
    };
    let workspace = flows::eager_workspace(checkout_root.to_path_buf());
    let cx = ToolCx {
        task_id,
        workspace: &workspace,
    };

    // ── Drive the loop, capturing the transcript even on error ───────────────────────────────────
    let sink = JobSink::default();
    let sink_handle = sink.clone();
    // Live status projection (RFC-0007 slice 5): when the status API is on, tee the loop's events into
    // the shared status handle (turn / current tool / findings) via a `StatusSink` that forwards every
    // event UNCHANGED to `JobSink` — so the transcript reconstructed below is byte-identical whether or
    // not the tap is installed. `None` ⇒ the bare `JobSink`, today's exact path.
    let loop_sink: Box<dyn TranscriptSink> = match status {
        Some(handle) => Box::new(lci_agent_status::StatusSink::new(
            handle.clone(),
            Box::new(sink),
            [ADD_REVIEW_COMMENT],
        )),
        None => Box::new(sink),
    };
    // Each `Assistant` sink event carries its own turn's telemetry (ADR-0087: on the `AssistantTurn`
    // itself, not a side-channel — #411/#417), so a lightweight poller sums it straight off
    // `sink_handle` to mirror "tokens so far" into the status handle while the loop runs. Spawned only
    // when the status API is on; aborted right after the loop.
    let usage_poller = status.map(|handle| {
        let handle = handle.clone();
        let sink_handle = sink_handle.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            // After a stall don't fire a burst of catch-up ticks — one usage mirror per second is
            // enough; skip missed ticks rather than spin.
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                // Lock and read in place rather than `entries()` — this ticks every second for the
                // life of the run, so cloning the whole (growing) transcript each time is wasteful.
                let guard = sink_handle
                    .0
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let (prompt, completion) = telemetry::sum_usage(&guard);
                drop(guard);
                handle.observe_usage(prompt, completion);
            }
        })
    });
    // Runtime selection (ADR-0087): the same assembly runs over either `StepRuntime`. Off by default
    // (`Passthrough` = today's prod behavior); when `LCI_DURABLE_REPLAY` is set, a `CheckpointRuntime`
    // journals each step through the internal API so a requeued run resumes from storage. The move of
    // the shared inputs into exactly one branch is what makes this a clean either/or.
    let outcome = if durable_replay {
        tracing::info!(task_id = %task_id, "durable replay enabled: driving the loop under CheckpointRuntime (ADR-0087)");
        let store = ControlPlaneStepStore::new(client.clone(), task_id);
        flows::run_review(
            CheckpointRuntime::new(store),
            chat,
            loop_sink,
            &cx,
            registry,
            conversation,
            params,
            client,
        )
        .await
    } else {
        flows::run_review(
            Passthrough,
            chat,
            loop_sink,
            &cx,
            registry,
            conversation,
            params,
            client,
        )
        .await
    };

    // Stop the poller and take one final, authoritative token reading (so the terminal snapshot is
    // accurate even if the last turn landed between polls).
    if let Some(poller) = usage_poller {
        poller.abort();
    }
    // One lock for both reads below instead of `entries()` cloning the whole transcript twice.
    let final_events = sink_handle
        .0
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(handle) = status {
        let (prompt, completion) = telemetry::sum_usage(&final_events);
        handle.observe_usage(prompt, completion);
    }

    // Emit the per-turn proof-of-work log lines BEFORE propagating any loop error, so a failed run's
    // reasoning still reaches the logs (the observability surface — epic #459). Each `Assistant` event
    // carries its own telemetry (ADR-0087) — no separate side-channel to zip against.
    transcript::log_agent_turns(&final_events, task_id);
    drop(final_events);

    Ok(match outcome? {
        LoopOutcome::Finished => ReviewOutcome::Finished,
        LoopOutcome::Exhausted => ReviewOutcome::Exhausted,
        LoopOutcome::Aborted { reason } => ReviewOutcome::Aborted(reason),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Panic while holding `mutex`'s lock, on a background thread, then join it (discarding the
    /// panic) — leaves the mutex poisoned exactly as a real panic-while-locked would.
    fn poison<T: Send + 'static>(mutex: &Arc<Mutex<T>>) {
        let for_thread = Arc::clone(mutex);
        let _ = std::thread::spawn(move || {
            let _guard = for_thread.lock().unwrap();
            panic!("intentional poisoning for test");
        })
        .join();
        assert!(mutex.is_poisoned(), "the mutex should now be poisoned");
    }

    #[test]
    fn job_sink_recovers_from_a_poisoned_mutex() {
        let mut sink = JobSink::default();
        sink.record(TranscriptEvent::Policy {
            turn: 0,
            name: "pre-poison",
            detail: serde_json::Value::Null,
        });

        poison(&sink.0);

        // Both `record` (via `TranscriptSink`) and `entries` must recover instead of panicking, and
        // the state recorded before the poisoning must still be there.
        sink.record(TranscriptEvent::Policy {
            turn: 1,
            name: "post-poison",
            detail: serde_json::Value::Null,
        });
        let entries = sink.entries();
        assert_eq!(entries.len(), 2);
    }
}
