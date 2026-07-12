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
//! all three so buffered findings are never discarded. The transcript is reconstructed from the loop's
//! sink + the model client's telemetry side-channel and submitted by the caller regardless of outcome
//! (a failed run's reasoning is the most useful to inspect).

pub mod instructions;

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use uuid::Uuid;

use lci_agent_clients::{
    CheckpointRuntime, ControlPlaneClient, ControlPlaneStepStore, EmbeddingsClient, TranscriptEntry,
};
use lci_agent_loop::{Conversation, LoopOutcome, RequestOptions, TranscriptEvent, TranscriptSink};
use lci_agent_step::Passthrough;
use lci_agent_tools::{RuntimeCaps, ToolCx, TurnFilter};
use lci_agent_types::{ToolOutcome, ToolSpec};
use lci_review_agent::flows::{self, ReviewRunParams};
use lci_review_agent::model::{ChatClient, RetryPolicy, TurnTelemetry};
use lci_review_agent::prompt::{self, PrDiffRef, PromptConfig};
use lci_review_agent::tools::{
    ABORT, ADD_COMMENT, ADD_REVIEW_COMMENT, FINISH, MCP_TOOL_PREFIX, RETRACT_FINDING, tool_defs,
    tool_registry,
};

use crate::bootstrap::config::{McpToolPattern, ReviewConfig, ReviewToolSelector};
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

/// A [`TranscriptSink`] that captures the loop's events so the host can reconstruct the ADR-0034
/// transcript afterwards (even on error). Cloneable handle: grab a clone before boxing it into the
/// loop, read [`entries`](Self::entries) once the run returns.
#[derive(Clone, Default)]
struct JobSink(Arc<Mutex<Vec<TranscriptEvent>>>);

impl JobSink {
    fn entries(&self) -> Vec<TranscriptEvent> {
        self.0.lock().expect("job sink mutex").clone()
    }
}

impl TranscriptSink for JobSink {
    fn record(&mut self, entry: TranscriptEvent) {
        self.0.lock().expect("job sink mutex").push(entry);
    }
}

/// Run the native agent loop. The agent acts via the mediated write tools during the run. Returns a
/// [`ReviewOutcome`] describing how it ended (`Finished` / `Exhausted` / `Aborted`) — the caller turns
/// each into a visible PR artifact and finalizes the buffer in all three cases (#137). Only a true
/// transport/loop failure returns `Err`; in that case nothing is posted (but the partial transcript is
/// still accumulated for submission).
#[allow(clippy::too_many_arguments)]
pub async fn run_native_agent(
    review: &ReviewConfig,
    command: &str,
    diff: Option<&PrDiff>,
    // Repo-native agent instructions (ADR-0036), prior reviews (A, #137), per-repo feedback memory (M1,
    // ADR-0044), and the deterministic SAST digest (ADR-0061) — all injected into the prompt as
    // untrusted context; `None` when absent.
    repo_instructions: Option<&str>,
    prior_reviews: Option<&str>,
    repo_memory: Option<&str>,
    sast_digest: Option<&str>,
    attribution: &[(String, String)],
    client: &ControlPlaneClient,
    embedder: &EmbeddingsClient,
    task_id: Uuid,
    // The checked-out repo root (the working tree under review). `read_file` reads from here,
    // path-sanitized to within it (epic #137).
    checkout_root: &Path,
    // Accumulates the run transcript (ADR-0034). The caller owns it and submits it afterwards (even on
    // error), so a failed run's reasoning is still captured.
    transcript: &mut Vec<TranscriptEntry>,
) -> anyhow::Result<ReviewOutcome> {
    // ── Model client (ADR-0039) ──────────────────────────────────────────────────────────────────
    // Streaming (ADR-0039 / #206): opt-in via `review.stream`. `with_extra` strips reserved structural
    // keys; the sanitized map is carried into the conversation's `RequestOptions` below (the engine
    // flattens the request `extra` from there). `with_retry_policy` preserves the per-turn retry the
    // legacy `complete_with_retry` applied before the loop's circuit breaker sees a transient failure.
    let chat = ChatClient::with_timeout(
        &review.base_url,
        &review.api_key,
        &review.model,
        Duration::from_secs(review.resilience.request_timeout_secs),
    )
    .with_attribution(attribution)
    .with_extra(review.extra.clone())
    .with_stream(review.stream)
    .with_retry_policy(RetryPolicy {
        max_retries: review.resilience.max_retries,
        ..RetryPolicy::default()
    });
    let request_extra = chat.extra().clone();

    tracing::info!(
        task_id = %task_id,
        model = %review.model,
        base_url_host = %base_url_host(&review.base_url),
        request_timeout_secs = review.resilience.request_timeout_secs,
        max_retries = review.resilience.max_retries,
        circuit_breaker_threshold = review.resilience.circuit_breaker_threshold,
        stream = review.stream,
        tier = if review.fast { "fast" } else { "deep" },
        extra = %serde_json::Value::Object(review.extra.clone()),
        "review agent starting"
    );

    // ── Offered tool surface: diff gate + per-tier allowlist + ADR-0066 MCP discovery ───────────────
    // Without a diff an inline finding has no line to anchor to, so `add_review_comment` isn't offered.
    let diff_present = diff.is_some();
    let mut offered = tool_defs();
    if !diff_present {
        offered.retain(|spec| spec.function.name != ADD_REVIEW_COMMENT);
    }
    // Per-tier tool allowlist (ADR-0062): its BUILT-IN entries are the authoritative offered set.
    if let Some(allow) = review.tools.as_ref() {
        let builtins: HashSet<&str> = allow
            .iter()
            .filter_map(|selector| match selector {
                ReviewToolSelector::Builtin(builtin) => Some(builtin.as_str()),
                ReviewToolSelector::Mcp(_) => None,
            })
            .collect();
        offered.retain(|spec| builtins.contains(spec.function.name.as_str()));
    }
    // External-knowledge MCP tools (ADR-0066): discovered dynamically. An UNSET allowlist offers ALL
    // discovered; a SET allowlist offers a discovered tool iff some `mcp__` selector matches, and skips
    // discovery entirely when it has none. A discovery failure degrades to "no external tools".
    let mcp_selectors: Option<Vec<&McpToolPattern>> = review.tools.as_ref().map(|allow| {
        allow
            .iter()
            .filter_map(|selector| match selector {
                ReviewToolSelector::Mcp(pattern) => Some(pattern),
                ReviewToolSelector::Builtin(_) => None,
            })
            .collect()
    });
    let discover = match &mcp_selectors {
        None => true,
        Some(selectors) => !selectors.is_empty(),
    };
    let mut dispatch_discovered: Vec<ToolSpec> = Vec::new();
    if discover {
        match client.list_knowledge_tools(task_id).await {
            Ok(discovered) => {
                let matched: Vec<_> = discovered
                    .into_iter()
                    .filter(|tool| match &mcp_selectors {
                        None => true,
                        Some(selectors) => {
                            selectors.iter().any(|pattern| pattern.is_match(&tool.name))
                        }
                    })
                    .collect();
                if !matched.is_empty() {
                    tracing::info!(task_id = %task_id, count = matched.len(), "offering discovered external-knowledge tools");
                    let specs: Vec<ToolSpec> = matched
                        .into_iter()
                        .map(|tool| {
                            ToolSpec::function(tool.name, tool.description, tool.input_schema)
                        })
                        .collect();
                    dispatch_discovered.extend(specs.iter().cloned());
                    offered.extend(specs);
                }
            }
            Err(error) => {
                tracing::warn!(%error, task_id = %task_id, "knowledge-tool discovery failed; continuing without external-knowledge tools");
            }
        }
    }

    // ── Run-start telemetry (ADR-0034/0062/0066), recorded at run START ─────────────────────────────
    // Snapshot what turn 0 will ACTUALLY offer: a FAST run without an allowlist runs every turn on the
    // wind-down write/finish set (the `FastTierGuard` narrows to it), so snapshotting the full surface
    // there would claim retrieval/read_file tools the model is never given.
    let winddown_defs = winddown_tool_defs(&offered, diff_present);
    let start_defs = run_start_tool_defs(review, &offered, &winddown_defs);
    let offered_tools_json = serde_json::Value::Array(
        start_defs
            .iter()
            .map(|spec| {
                let source = if spec.function.name.starts_with(MCP_TOOL_PREFIX) {
                    "mcp"
                } else {
                    "builtin"
                };
                serde_json::json!({ "name": spec.function.name, "source": source })
            })
            .collect(),
    );
    let offered_tool_names: Vec<&str> = start_defs
        .iter()
        .map(|spec| spec.function.name.as_str())
        .collect();
    tracing::info!(
        task_id = %task_id,
        tier = if review.fast { "fast" } else { "deep" },
        model = %review.model,
        tool_count = offered_tool_names.len(),
        tools = ?offered_tool_names,
        "review run: offered tools"
    );
    if let Err(error) = client
        .submit_review_telemetry(task_id, &offered_tools_json, &review.redacted_config_b64())
        .await
    {
        tracing::warn!(%error, task_id = %task_id, "submitting review telemetry failed (non-fatal)");
    }

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
        sast_digest,
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

    // ── Tool registry (built-ins + discovered) ──────────────────────────────────────────────────
    let registry = tool_registry(
        Arc::new(client.clone()),
        Arc::new(embedder.clone()),
        dispatch_discovered,
        runtime_caps,
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
        fast: review.fast,
        diff_present,
        diff_files: diff.map(|pr| pr.files.clone()).unwrap_or_default(),
    };
    let workspace = flows::eager_workspace(checkout_root.to_path_buf());
    let cx = ToolCx {
        task_id,
        workspace: &workspace,
    };

    // ── Drive the loop, capturing the transcript even on error ───────────────────────────────────
    let sink = JobSink::default();
    let sink_handle = sink.clone();
    let telemetry = chat.telemetry_handle();
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
            Box::new(sink),
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
            Box::new(sink),
            &cx,
            registry,
            conversation,
            params,
            client,
        )
        .await
    };

    // Reconstruct the ADR-0034 transcript from the sink events + the model client's per-turn telemetry
    // side-channel BEFORE propagating any loop error, so the caller still submits a failed run's
    // reasoning.
    append_transcript(
        transcript,
        &sink_handle.entries(),
        &telemetry.lock().expect("telemetry mutex"),
        task_id,
    );

    Ok(match outcome? {
        LoopOutcome::Finished => ReviewOutcome::Finished,
        LoopOutcome::Exhausted => ReviewOutcome::Exhausted,
        LoopOutcome::Aborted { reason } => ReviewOutcome::Aborted(reason),
    })
}

/// Reconstruct the ADR-0034 transcript rows from the loop's sink events + the model client's per-turn
/// telemetry (sequential model calls ⇒ index == the Nth assistant event). Assistant turns carry their
/// tokens/model from the telemetry side-channel; tool results carry the (bounded) outcome text — the
/// finish/abort terminal outcomes record no tool row, matching the legacy loop. Policy events are not
/// transcript rows.
fn append_transcript(
    transcript: &mut Vec<TranscriptEntry>,
    events: &[TranscriptEvent],
    telemetry: &[TurnTelemetry],
    task_id: Uuid,
) {
    let mut assistant_index = 0usize;
    for event in events {
        match event {
            TranscriptEvent::Assistant { turn, message } => {
                let telemetry = telemetry.get(assistant_index);
                assistant_index += 1;
                // Proof-of-work (epic #137): one concise per-turn line, including the chain-of-thought
                // length (the reliable "how far did it think" signal even when the gateway folds
                // reasoning into `completion_tokens`).
                tracing::info!(
                    task_id = %task_id,
                    turn,
                    model = telemetry.map(|entry| entry.model.as_str()).unwrap_or("?"),
                    prompt_tokens = telemetry.and_then(|entry| entry.prompt_tokens).unwrap_or(-1),
                    completion_tokens = telemetry
                        .and_then(|entry| entry.completion_tokens)
                        .unwrap_or(-1),
                    reasoning_tokens = telemetry
                        .and_then(|entry| entry.reasoning_tokens)
                        .unwrap_or(-1),
                    reasoning_chars = telemetry
                        .and_then(|entry| entry.reasoning.as_deref())
                        .map(|reasoning| reasoning.chars().count())
                        .unwrap_or(0),
                    "agent turn complete"
                );
                transcript.push(TranscriptEntry {
                    role: "assistant".to_string(),
                    content: message.content.clone(),
                    tool_calls: (!message.tool_calls.is_empty())
                        .then(|| serde_json::to_value(&message.tool_calls).unwrap_or_default()),
                    tool_name: None,
                    prompt_tokens: telemetry.and_then(|entry| entry.prompt_tokens),
                    completion_tokens: telemetry.and_then(|entry| entry.completion_tokens),
                    reasoning_tokens: telemetry.and_then(|entry| entry.reasoning_tokens),
                    model: telemetry.map(|entry| entry.model.clone()),
                });
            }
            TranscriptEvent::Tool { call, outcome, .. } => {
                if let ToolOutcome::Continue(result) = outcome {
                    transcript.push(TranscriptEntry {
                        role: "tool".to_string(),
                        content: Some(truncate_on_boundary(result, 2048).to_string()),
                        tool_calls: None,
                        tool_name: Some(call.function.name.clone()),
                        prompt_tokens: None,
                        completion_tokens: None,
                        reasoning_tokens: None,
                        model: None,
                    });
                }
            }
            TranscriptEvent::Policy { .. } => {}
        }
    }
}

/// The reduced tool set offered once a run enters wind-down (#137), used ONLY for the run-start
/// telemetry snapshot of a FAST run: the write tools + `finish`/`abort`, dropping retrieval/read_file.
/// `add_review_comment`/`retract_finding` are kept only when a diff is present (an inline tool can't
/// anchor without one). Mirrors the engine's convergence narrowing.
fn winddown_tool_defs(base: &[ToolSpec], diff_present: bool) -> Vec<ToolSpec> {
    base.iter()
        .filter(|spec| match spec.function.name.as_str() {
            ADD_REVIEW_COMMENT | RETRACT_FINDING => diff_present,
            ADD_COMMENT | FINISH | ABORT => true,
            _ => false,
        })
        .cloned()
        .collect()
}

/// The tool set turn 0 will ACTUALLY offer for the telemetry snapshot: a FAST run WITHOUT an explicit
/// allowlist runs every turn on the wind-down write/finish set (the `FastTierGuard` narrows to it), so
/// snapshotting the full surface there would record retrieval/read_file/MCP tools the model never gets.
fn run_start_tool_defs<'a>(
    review: &ReviewConfig,
    defs: &'a [ToolSpec],
    winddown_defs: &'a [ToolSpec],
) -> &'a [ToolSpec] {
    if review.fast && review.tools.is_none() {
        winddown_defs
    } else {
        defs
    }
}

/// Host of a base URL for logging (never the path/key). Falls back to the whole string when there's no
/// scheme separator, so a schemeless URL still logs its host rather than "(unparseable)".
fn base_url_host(base_url: &str) -> String {
    let without_scheme = base_url.split("://").nth(1).unwrap_or(base_url);
    without_scheme
        .split(['/', '?', '#'])
        .next()
        .map(|hostport| hostport.to_string())
        .unwrap_or_else(|| "(unparseable)".to_string())
}

/// `s` truncated to at most `max` bytes, never slicing through a multi-byte char.
fn truncate_on_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
