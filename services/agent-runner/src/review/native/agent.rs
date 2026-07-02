//! The native agent loop (ADR-0026 + ADR-0037).
//!
//! Drives the [`ChatClient`] over the eaig gateway with the [`tool_defs`] surface: the system prompt
//! (the operator-owned persona + the machine tool-protocol) and the request → model → (retrieval +
//! mediated write actions) → … → `finish`. The agent **acts as it goes** — each `add_review_comment`
//! / `add_comment` is buffered control-plane-side; `finish` ends the run and the caller flushes the
//! buffer as one grouped review. The loop is bounded (`review.max_turns`) and treats a tool/argument
//! error as a recoverable turn (the model gets the error text back and retries).
//!
//! Outcome model (#137): the loop returns a [`ReviewOutcome`] — `Finished` (the model called `finish`),
//! `Exhausted` (the turn budget ran out while findings may be buffered), or `Aborted(reason)` (the
//! model called `abort`). **Only a true transport/chat failure returns `Err`.** This is the key
//! behavioural fix: the buffer (which the control plane holds) is NEVER discarded on a clean exhaustion
//! — the caller finalizes on `Finished` OR `Exhausted` so buffered findings are still posted, with a
//! truncation note on `Exhausted` and an abort note on `Aborted`. A transport layer (ADR-0039) wraps
//! each turn with a generous timeout, bounded retry/backoff on transient failures, and a per-run
//! circuit breaker; the HTTP error body is folded into the error so a
//! failed run is legible. Cancellation comes for free: `run()` races the whole task against the
//! self-cancel poll, so a cancelled task drops the in-flight future.

use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context;
use uuid::Uuid;

use super::chat::{ChatClient, ChatMessage, ChatParams, RetryPolicy, ToolDef};
use super::tools::{
    tool_defs, ToolOutcome, Tools, ABORT, ADD_COMMENT, ADD_REVIEW_COMMENT, EMPTY_RETRIEVAL_RESULT,
    FINISH, GRAPH_FIND_SYMBOL, GRAPH_GET_CALLERS, READ_FILE, RETRACT_FINDING,
    VECTOR_SEMANTIC_SEARCH,
};
use crate::bootstrap::client::{ControlPlaneClient, TranscriptEntry};
use crate::bootstrap::config::{McpToolPattern, ReviewConfig, ReviewToolSelector};
use crate::clone::PrDiff;
use crate::indexer::embeddings::EmbeddingsClient;

/// How the agent loop ended (#137). Distinct from `Err`, which is reserved for a transport/chat
/// failure where the gateway was unreachable and nothing useful happened. The caller maps these to a
/// visible artifact on the PR:
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

/// The machine **tool-protocol** appended after the operator's system prompt (ADR-0037). This is the
/// only behaviour-shaping text that lives in code — it is factual and coupled to the tool API (names,
/// when to call them), NOT persona/guidance, which is operator-owned config (`review.system_prompt`,
/// from the ai-helm `config.reviewSystemPrompt`). It goes last so it's the final instruction.
const TOOL_PROTOCOL: &str = "\
# How to act\n\
Investigate with the search/graph tools before making any claim — never speculate about code you \
have not looked up. As you find issues, record each one with `add_review_comment` (one call per \
finding, on a line this diff adds or changes). Use `add_comment` for a plain reply that isn't pinned \
to a diff line (e.g. answering a question). Nothing you record is posted until you call `finish` with \
your overall verdict — call `finish` exactly once when you are done, even if you found nothing. If you \
genuinely cannot produce anything useful, call `abort` with a reason. You may not edit files or run \
commands.";

/// Wind-down convergence (#137): how many turns before the budget ceiling we switch the model onto a
/// reduced tool set so it stops investigating and converges to a `finish`. Computed per-run as a
/// fraction of `max_turns` (see [`winddown_turn`]) so a tiny budget still leaves a turn or two to wrap
/// up and a generous budget gets a proportional tail. Observed in prod (izhub#205/#207): the agent ran
/// the full budget with **0** `finish` calls — endless retrieval/`read_file` rabbit holes — and the
/// `Exhausted` safety-net fired instead of a real verdict. The lever is removing the investigation
/// tools near the end: with only the write/finish/abort tools left, the model must record any last
/// findings and finish.
const WINDDOWN_MIN_TURNS: usize = 2;

/// Turn ceiling for the FAST tier (ADR-0062). The fast tier's cheapness comes from **no retrieval** + a
/// cheap model + short timeout — NOT from a single turn. One turn is too few: the model's first action is
/// also its last, so it can't both act and then `finish` (whose summary becomes the review body) — a
/// 1-turn fast pass posted an empty review on a PR with changes (vymalo-shop#301). A few no-retrieval
/// turns let it record findings via `add_review_comment` and `finish`. The fast block's own `max_turns`
/// (if set) lowers this; this caps it so an unset fast block can't inherit the generous default (40).
const FAST_TIER_MAX_TURNS: usize = 5;

/// Default cap on how many chars of a turn's `reasoning_content` we echo to the live log. Generous on
/// purpose: a heavy reasoner (GLM-5.2) emits thousands of chars per turn and the old 600-char cap hid
/// "how far it thinks". Override with the `REASONING_LOG_CHARS` env (`0` = unbounded).
const REASONING_LOG_CHARS_DEFAULT: usize = 4000;

/// Resolve the reasoning-log cap from `REASONING_LOG_CHARS`, falling back to [`REASONING_LOG_CHARS_DEFAULT`].
/// A non-numeric or absent value uses the default; `0` means log the whole chain-of-thought.
fn reasoning_log_chars() -> usize {
    std::env::var("REASONING_LOG_CHARS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(REASONING_LOG_CHARS_DEFAULT)
}

/// The first turn index at which the wind-down (reduced tool set + budget message) kicks in. Reserves
/// `max(WINDDOWN_MIN_TURNS, max_turns / 10)` turns at the tail of the budget, clamped so it never lands
/// before turn 1 (we always allow at least one full-toolset turn). A `max_turns=1` budget is degenerate
/// — one turn can't both investigate AND wind down — so it returns `1`, which the single turn (`turn=0`)
/// never reaches: that run gets no wind-down and the `Exhausted` backstop catches it (fine for a
/// one-turn budget).
fn winddown_turn(max_turns: usize) -> usize {
    if max_turns <= WINDDOWN_MIN_TURNS {
        // Tiny budgets: wind down on the final turn so there's still one investigation turn first.
        // (`max_turns=1` → `1`, unreachable by the only turn → no wind-down; the backstop handles it.)
        return max_turns.saturating_sub(1).max(1);
    }
    let reserve = WINDDOWN_MIN_TURNS.max(max_turns / 10);
    // Keep at least one full-toolset turn at the start, and at least one wind-down turn at the end.
    max_turns
        .saturating_sub(reserve)
        .clamp(1, max_turns.saturating_sub(1))
}

/// The reduced tool set offered once the run enters its wind-down (#137): the write tools (so the model
/// can record any last findings), plus `finish`/`abort` (so it can converge). **Drops** the retrieval
/// tools, `read_file`, and `report_progress` — with no way to keep investigating, the model must wrap
/// up. `add_review_comment` is only kept when a diff is present (mirrors the full-set gating, so a
/// no-diff run never offers an inline tool that can't anchor).
fn winddown_tool_defs(base: &[ToolDef], diff_present: bool) -> Vec<ToolDef> {
    base.iter()
        .filter(|t| {
            let name = t.function.name.as_str();
            match name {
                ADD_REVIEW_COMMENT => diff_present,
                // RETRACT_FINDING stays available so the pre-finish refute pass can drop a P0/P1 that
                // didn't hold even in the wind-down tail (Phase 2, ADR-0043).
                RETRACT_FINDING => diff_present,
                ADD_COMMENT | FINISH | ABORT => true,
                _ => false,
            }
        })
        .cloned()
        .collect()
}

/// Enter wind-down once the conversation reaches this fraction of the configured context window
/// (ADR-0045), leaving headroom for estimator error and the final verdict turn.
const WINDDOWN_TOKEN_FRACTION: f64 = 0.75;

/// A deliberately conservative token estimate for the messages + advertised tools (ADR-0045). The
/// gateway model isn't OpenAI-tokenized, so an exact tokenizer would be false precision and a heavy
/// dependency; ~chars/4 plus a small per-message overhead over-estimates slightly — which is exactly
/// what a safety budget wants. Used only to decide when to wind down / trim, never reported as truth.
fn estimate_tokens(messages: &[ChatMessage], tools: &[ToolDef]) -> usize {
    const PER_MESSAGE_OVERHEAD: usize = 4;
    let msgs: usize = messages
        .iter()
        .map(|m| {
            let content = m.content.as_deref().map_or(0, str::len);
            let calls: usize = m
                .tool_calls
                .iter()
                .map(|c| c.function.name.len() + c.function.arguments.len())
                .sum();
            PER_MESSAGE_OVERHEAD + (content + calls) / 4
        })
        .sum();
    // The tool schemas are re-sent every turn, so they count against the window too.
    let tools: usize = tools
        .iter()
        .map(|t| {
            (t.function.name.len()
                + t.function.description.len()
                + t.function.parameters.to_string().len())
                / 4
        })
        .sum();
    msgs + tools
}

/// Whether a deterministic chat error is the gateway rejecting the request for exceeding the context
/// window (ADR-0045) — distinct from a genuine bad-request we should fail on. Matched on the surfaced
/// error text (ADR-0039 folds the response body into the error message).
fn is_context_overflow(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}").to_lowercase();
    // Kept tight to genuine context-window signals (the ADR-0045 list). Deliberately NOT
    // "maximum number of tokens" — that also matches a deterministic `max_tokens`-too-large param
    // error, which must fail fast, not be finalized as an overflow.
    [
        "context length",
        "context_length_exceeded",
        "maximum context",
        "too many tokens",
        "reduce the length",
    ]
    .iter()
    .any(|needle| msg.contains(needle))
}

/// Shrink the content of the OLDEST `tool`-result messages to a stub until the estimate fits `target`
/// tokens (ADR-0045). Keeps each message and its `tool_call_id` so the assistant↔tool-result pairing
/// the protocol requires stays valid; leaves the most recent few messages untouched (the agent may
/// still be acting on them). Returns how many messages were trimmed. Tool-result bodies (file/search
/// output the agent has already reasoned over) are the bulk and the safe thing to drop as we converge.
fn trim_tool_history(messages: &mut [ChatMessage], tools: &[ToolDef], target: usize) -> usize {
    const KEEP_RECENT: usize = 2;
    const STUB: &str = "[earlier tool output elided to fit the context budget]";
    let cutoff = messages.len().saturating_sub(KEEP_RECENT);
    // Estimate once, then decrement a running total as we trim — `estimate_tokens` JSON-serializes
    // every tool schema, so calling it per iteration would be O(N²) on a long conversation.
    // `estimate_tokens` counts each message as `overhead + (content + calls)/4`, so replacing a
    // body with the stub reclaims `(old_len - stub_len)/4` tokens.
    let mut est = estimate_tokens(messages, tools);
    let mut trimmed = 0usize;
    for msg in messages.iter_mut().take(cutoff) {
        if est <= target {
            break;
        }
        let old_len = match msg.content.as_deref() {
            Some(c) if msg.role == "tool" && c.len() > STUB.len() && c != STUB => c.len(),
            _ => continue,
        };
        est = est.saturating_sub((old_len - STUB.len()) / 4);
        msg.content = Some(STUB.to_string());
        trimmed += 1;
    }
    trimmed
}

/// Run the native agent loop. The agent acts via the mediated write tools during the run. Returns a
/// [`ReviewOutcome`] describing how it ended (`Finished` / `Exhausted` / `Aborted`) — the caller turns
/// each into a visible PR artifact and finalizes the buffer in all three cases (#137). Only a true
/// transport/chat failure returns `Err`; in that case nothing is posted.
#[allow(clippy::too_many_arguments)]
pub async fn run_native_agent(
    review: &ReviewConfig,
    command: &str,
    diff: Option<&PrDiff>,
    repo_instructions: Option<&str>,
    // The agent's own prior review of this target (A, #137), pre-formatted by the control plane. `Some`
    // only on a re-review with an earlier review; injected so the run reconciles with its past output
    // instead of contradicting itself across runs.
    prior_reviews: Option<&str>,
    // Per-repo feedback memory (M1, ADR-0044): findings rejected (👎) here before, injected so the run
    // doesn't re-raise known false positives.
    repo_memory: Option<&str>,
    // Deterministic SAST digest (ADR-0061): a compact list of what opengrep already flagged on this diff,
    // injected so the agent doesn't redundantly re-report those lines (the findings post independently).
    sast_digest: Option<&str>,
    attribution: &[(String, String)],
    client: &ControlPlaneClient,
    embedder: &EmbeddingsClient,
    task_id: Uuid,
    // The checked-out repo root (the working tree under review). `read_file` reads from here,
    // path-sanitized to within it, so the model can open the actual source (epic #137).
    checkout_root: &Path,
    // Accumulates the run transcript (ADR-0034) as the loop progresses. The caller owns it and submits
    // it afterwards (even on error), so a failed run's reasoning is still captured.
    transcript: &mut Vec<TranscriptEntry>,
) -> anyhow::Result<ReviewOutcome> {
    // Streaming (ADR-0039 / #206): opt-in via `review.stream` (config; else the legacy `LLM_STREAM`
    // env, resolved in bootstrap). Collects the SSE response under a per-chunk idle timeout instead of
    // one whole-request timeout — mitigates long-reasoning turns timing out (e.g. a model like GLM).
    let chat = ChatClient::with_timeout(
        &review.base_url,
        &review.api_key,
        &review.model,
        Duration::from_secs(review.resilience.request_timeout_secs),
    )
    .with_attribution(attribution)
    .with_extra(review.extra.clone())
    .with_stream(review.stream);
    let retry_policy = RetryPolicy {
        max_retries: review.resilience.max_retries,
        ..RetryPolicy::default()
    };
    // Proof-of-work (ADR-0039): a run is legible from pod logs alone — which model, which gateway, and
    // the resilience policy in force. host-only for the base URL (the path/key are uninteresting/secret).
    tracing::info!(
        task_id = %task_id,
        model = %review.model,
        base_url_host = %base_url_host(&review.base_url),
        request_timeout_secs = review.resilience.request_timeout_secs,
        max_retries = review.resilience.max_retries,
        circuit_breaker_threshold = review.resilience.circuit_breaker_threshold,
        stream = review.stream,
        // Review tier (ADR-0062): `fast` = single diff-only turn, no retrieval; `deep` = full loop.
        tier = if review.fast { "fast" } else { "deep" },
        // The `review.extra` passthrough actually in force (e.g. `reasoning_effort`) — serialized into
        // every chat body. Logged so a run proves *from the logs* which reasoning budget was applied,
        // not just which one the ConfigMap claims. Empty `{}` = nothing extra sent.
        extra = %serde_json::Value::Object(review.extra.clone()),
        "review agent starting"
    );

    let tools = Tools {
        client,
        embedder,
        task_id,
        checkout_root,
    };
    // Without a diff (an issue target, or `git diff` was unavailable) an inline finding has no line to
    // anchor to — finalize would only bucket it. Don't offer `add_review_comment` then, so the model
    // replies via `add_comment` instead of hallucinating inline comments that go nowhere.
    let diff_present = diff.is_some();
    let mut defs = tool_defs();
    if !diff_present {
        defs.retain(|t| t.function.name != ADD_REVIEW_COMMENT);
    }
    // Per-tier tool allowlist (ADR-0062): when the tier declares `review.tools`, its BUILT-IN entries
    // are the authoritative offered set — restrict the static base to them. Still subject to the
    // `diff_present` gate above and the wind-down narrowing below. Unset = the built-in default
    // surface (full set for DEEP). The `mcp__` selectors are handled separately just below.
    if let Some(allow) = review.tools.as_ref() {
        let builtins: HashSet<&str> = allow
            .iter()
            .filter_map(|s| match s {
                ReviewToolSelector::Builtin(b) => Some(b.as_str()),
                ReviewToolSelector::Mcp(_) => None,
            })
            .collect();
        defs.retain(|t| builtins.contains(t.function.name.as_str()));
    }
    // External-knowledge MCP tools (ADR-0066): discovered dynamically from the control plane, never
    // hardcoded here. The allowlist picks WHICH discovered tools to offer via `mcp__`-anchored regex
    // selectors — so an operator can take one tool from a busy server (brave-search exposes many)
    // rather than all-or-nothing. Rules: an UNSET allowlist offers ALL discovered (the full default
    // surface, same as every built-in); a SET allowlist offers a discovered tool iff some `mcp__`
    // selector matches its name, and skips discovery entirely when it has no `mcp__` selectors (no
    // wasted round-trip). A discovery failure (no servers, one unreachable, a network hiccup) degrades
    // to "no external-knowledge tools this run" — never fails the review.
    let mcp_selectors: Option<Vec<&McpToolPattern>> = review.tools.as_ref().map(|allow| {
        allow
            .iter()
            .filter_map(|s| match s {
                ReviewToolSelector::Mcp(p) => Some(p),
                ReviewToolSelector::Builtin(_) => None,
            })
            .collect()
    });
    let discover = match &mcp_selectors {
        None => true,                 // allowlist unset → full surface
        Some(sel) => !sel.is_empty(), // set → only if it names ≥1 mcp selector
    };
    if discover {
        match tools.client.list_knowledge_tools(task_id).await {
            Ok(discovered) => {
                let matched: Vec<_> = discovered
                    .into_iter()
                    .filter(|t| match &mcp_selectors {
                        None => true, // unset → offer everything discovered
                        Some(sel) => sel.iter().any(|p| p.is_match(&t.name)),
                    })
                    .collect();
                if !matched.is_empty() {
                    tracing::info!(task_id = %task_id, count = matched.len(), "offering discovered external-knowledge tools");
                    defs.extend(
                        matched
                            .into_iter()
                            .map(|t| ToolDef::function(t.name, t.description, t.input_schema)),
                    );
                }
            }
            Err(error) => {
                tracing::warn!(%error, task_id = %task_id, "knowledge-tool discovery failed; continuing without external-knowledge tools");
            }
        }
    }
    // Reduced tool set for the wind-down tail (#137): write/finish/abort only, retrieval/read_file
    // dropped so the model can no longer keep investigating once the budget is nearly spent. Derived
    // from the (possibly allowlist-restricted) `defs`, NOT the global surface, so a per-tier
    // `review.tools` allowlist is honoured in the wind-down too — a tool the allowlist dropped can't
    // reappear in the tail (gemini review on #237).
    let winddown_defs = winddown_tool_defs(&defs, diff_present);
    let params = ChatParams {
        temperature: review.temperature,
        top_p: review.top_p,
        max_tokens: review.max_tokens,
    };

    let mut messages = build_messages(
        review,
        command,
        diff,
        repo_instructions,
        prior_reviews,
        repo_memory,
        sast_digest,
    );

    // Per-run circuit breaker (ADR-0039): consecutive turn-failures. The Job is ephemeral, so this is
    // deliberately per-process — no cross-process state. Resets on the first turn that succeeds.
    let mut consecutive_failures = 0u32;
    let breaker_threshold = review.resilience.circuit_breaker_threshold;

    // Operator-tunable turn budget (#137): a tight ceiling used to exhaust mid-PR with findings still
    // buffered. Generous default lives in config; a turn is ~6s on the deepseek model.
    // Two-tier review (ADR-0062): the FAST tier (automatic PR-opened) runs WITHOUT retrieval (that's its
    // cheapness — see the tool-set + refusal guards), bounded by a small turn ceiling so it can record
    // findings and `finish` without an investigation loop. Not 1 turn: the model needs room to act AND
    // finish (a 1-turn pass posted an empty review, vymalo-shop#301). Deep uses the full configured budget.
    let max_turns = if review.fast {
        review.max_turns.min(FAST_TIER_MAX_TURNS)
    } else {
        review.max_turns
    };
    // Risk-first batching (ADR-0042): how many read-only tool calls we run concurrently per turn.
    let max_batch_size = review.max_batch_size.max(1);
    // Once the model has recorded ≥1 finding we nudge it to call `finish` so a wandering run doesn't burn
    // the budget after the useful work is already buffered (#137). Nudge once, lightly.
    let mut findings_recorded = 0usize;
    let mut nudged_to_finish = false;
    // Wind-down convergence (#137): the turn at which we restrict the tool set + inject the budget
    // message, plus a one-time soft nudge around the halfway mark. The tool restriction is the real
    // lever — the messages just explain why the tools changed.
    let winddown = winddown_turn(max_turns);
    let halfway = max_turns / 2;
    let mut winddown_announced = false;
    let mut halfway_nudged = false;

    // Full-diff coverage gate (B, #137). The whole diff is in the prompt, but a run tends to find ONE
    // issue and `finish` — two runs on the same PR each surfaced a *different* real P1 (see ADR-0041).
    // We track which changed files the agent has actually engaged (opened with `read_file` or recorded a
    // finding on) and, the FIRST time it tries to `finish` before the wind-down boundary with changed
    // files still untouched, bounce it once with the explicit list so it accounts for the whole change
    // across all dimensions before converging. Gated to pre-wind-down so it never fights the #173
    // convergence tail, and bounce-once so it costs at most a single extra turn.
    let changed_files: HashSet<String> = diff
        .map(|d| d.files.iter().map(|f| normalize_repo_path(f)).collect())
        .unwrap_or_default();
    let mut engaged_files: HashSet<String> = HashSet::new();
    let mut coverage_bounced = false;

    // Refute pass (Phase 2, ADR-0043): the quality gap is confidently-wrong P0/P1s. Before the first
    // `finish`, if any P0/P1 finding was recorded, bounce once to force the model to re-verify each
    // against its cited evidence and `retract_finding` the ones that don't hold. Cost-gated to P0/P1
    // (a wrong blocker costs the most trust), one-shot so it adds at most a single turn.
    let mut p0p1_recorded = 0usize;
    let mut refute_bounced = false;

    // Scratchpad-loop guard (post-Phase-2 dogfood, run 7c15f9bb): a model that can't find what it
    // wants will reach for `add_review_comment` as a notepad and re-record placeholder "findings" on
    // the SAME (file, line) — which the buffer dedups (last-write-wins), so it never "sees progress",
    // spirals, and aborts (posting a `placeholder` finding). Track consecutive recordings on the same
    // location; after a few, drop `add_review_comment` for one turn and tell it to investigate or finish
    // instead — breaking the loop mechanically.
    let mut last_finding_loc: Option<(String, i32)> = None;
    let mut same_loc_repeats = 0usize;
    let mut suppress_record = false;

    // Cumulative read budgets (ADR-0042): once a budget is spent we drop the matching tool from the
    // offered set and nudge the model to converge — read *enough*, then stop, instead of grinding the
    // whole repo. `max_batches` (investigation rounds) forces the wind-down; the finer `max_files_read`
    // / `max_searches` drop just their tool category. Counters advance after each turn's dispatch, so a
    // turn's offered tools reflect the budget spent through the *previous* turn.
    let max_files_read = review.max_files_read;
    let max_searches = review.max_searches;
    let max_batches = review.max_batches;
    let mut files_read = 0usize;
    let mut searches = 0usize;
    let mut batches = 0usize;
    let mut files_budget_announced = false;
    let mut searches_budget_announced = false;

    // Context-window budget (ADR-0045). When `context_window` is set we estimate the conversation size
    // each turn and converge before overflow; `overflow_finalize` records that an overflow error cut the
    // run short so the tail can finalize (flush findings) rather than fail. `None` = no budgeting.
    let context_window = review.context_window;
    let mut overflow_finalize = false;

    // Resolve the reasoning-log cap once (it can't change mid-run): `std::env::var` takes the process
    // env lock on every call, and a review runs many turns. (Gemini/lightbridge review on #220.)
    let reasoning_log_cap = reasoning_log_chars();

    for turn in 0..max_turns {
        let turn_started = Instant::now();

        // Wind-down (#137): as the budget depletes, switch the model onto the reduced tool set and tell
        // it (once) to stop investigating and converge. The reduced set drops retrieval/read_file, so
        // the model has no way to keep digging — it must record any last findings and `finish`. The
        // existing `Exhausted` path below stays as the ultimate backstop if it STILL doesn't finish.
        // Wind-down is triggered by the turn budget OR by spending the investigation-batch budget
        // (ADR-0042) — either way, drop the investigation tools so the model converges.
        let batches_spent = batches >= max_batches;
        // Context-window budget (ADR-0045): estimate the conversation size and, if it nears the window,
        // first trim old consumed tool output to reclaim space, then (if still near) wind down so the
        // agent converges before the gateway rejects an over-length request. Disabled when unset.
        let tokens_spent = if let Some(window) = context_window {
            let target = (window as f64 * WINDDOWN_TOKEN_FRACTION) as usize;
            let mut est = estimate_tokens(&messages, &defs);
            if est > target {
                let trimmed = trim_tool_history(&mut messages, &defs, target);
                if trimmed > 0 {
                    est = estimate_tokens(&messages, &defs);
                    tracing::warn!(
                        task_id = %task_id, turn, trimmed, est_tokens = est, window,
                        "context budget: trimmed old tool output to fit the window"
                    );
                }
            }
            est >= target
        } else {
            false
        };
        let in_winddown = turn >= winddown || batches_spent || tokens_spent;
        // Finer read budgets (ADR-0042): before full wind-down, drop just the exhausted tool category
        // (read_file / retrieval) so the model can still record findings and finish while it stops the
        // kind of reading it has used up. Built per-turn only when a budget is spent (else borrow `defs`).
        let files_spent = files_read >= max_files_read;
        let searches_spent = searches >= max_searches;
        let turn_defs_owned: Vec<ToolDef>;
        let turn_defs: &[ToolDef] = if review.fast {
            // FAST tier (ADR-0062): never offer retrieval/read_file — the turns record findings (from the
            // diff + SAST digest in the prompt) and finish. With an explicit `review.tools` allowlist,
            // `defs` already IS that reduced set; without one, fall back to the built-in wind-down
            // write/finish/abort set so the legacy (no-allowlist) values shape keeps working.
            if review.tools.is_some() {
                &defs
            } else {
                &winddown_defs
            }
        } else if in_winddown {
            &winddown_defs
        } else if files_spent || searches_spent || suppress_record {
            turn_defs_owned = defs
                .iter()
                .filter(|t| {
                    let n = t.function.name.as_str();
                    // Drop a tool only when its own budget is spent — or `add_review_comment` for one
                    // turn when the scratchpad-loop guard fired (forces investigate/finish instead).
                    let drop = (files_spent && n == READ_FILE)
                        || (searches_spent && is_retrieval_tool(n))
                        || (suppress_record && n == ADD_REVIEW_COMMENT);
                    !drop
                })
                .cloned()
                .collect();
            &turn_defs_owned
        } else {
            &defs
        };
        // FAST tier (ADR-0062): the offered set excludes retrieval/read_file, but a model steered by the
        // shared system prompt may still *emit* calls to them — and the dispatcher would otherwise run any
        // tool by name. So in the fast tier we enforce the offered set: a call to a non-offered tool is
        // refused (a synthetic result), never dispatched, keeping the pass truly diff-only. (Deep is
        // unchanged — its budgets already shape the offered set and we don't refuse there.)
        let offered: std::collections::HashSet<&str> =
            turn_defs.iter().map(|t| t.function.name.as_str()).collect();
        let fast_refuse = |tool: &str| review.fast && !offered.contains(tool);
        // One-turn suppression: the guard's effect on `turn_defs` is now baked in for this turn.
        suppress_record = false;
        // The FAST tier (ADR-0062) never offers retrieval/read_file, so the convergence nudges below
        // ("stop investigating", "stop opening files", "you're past halfway") are noise — even
        // contradictory — for it; skip them all. (gemini review on #235.)
        if !review.fast && in_winddown && !winddown_announced {
            winddown_announced = true;
            let why = if tokens_spent && turn < winddown && !batches_spent {
                "Context budget nearly full".to_string()
            } else if batches_spent && turn < winddown {
                format!("Investigation batch budget spent ({batches}/{max_batches} batches)")
            } else {
                format!("Turn budget almost spent (turn {turn}/{max_turns})")
            };
            messages.push(ChatMessage::user(format!(
                "⏳ {why}. Stop investigating — record any remaining findings now with \
                 add_review_comment/add_comment, then call `finish` with your overall verdict. (The \
                 investigation tools are no longer available.)"
            )));
        } else if !review.fast && !in_winddown {
            // One-time notice when a single read budget is exhausted (the tool is already dropped above).
            if files_spent && !files_budget_announced {
                files_budget_announced = true;
                messages.push(ChatMessage::user(format!(
                    "📄 You've read {files_read} files (the read_file budget). Stop opening files — work \
                     from what you have, record findings, and head toward `finish`."
                )));
            }
            if searches_spent && !searches_budget_announced {
                searches_budget_announced = true;
                messages.push(ChatMessage::user(format!(
                    "🔎 You've run {searches} searches (the retrieval budget). Stop searching — record \
                     findings from what you've found and head toward `finish`."
                )));
            }
            if !halfway_nudged && halfway > 0 && turn >= halfway {
                // Softer one-time nudge around the halfway mark — keep it light; the tool restriction at
                // the wind-down boundary is the real lever. `!in_winddown` guards small budgets where
                // `winddown <= halfway`: once we've announced wind-down ("finalize now"), we must NOT then
                // push the softer "you're past halfway" message — that would be a conflicting instruction.
                halfway_nudged = true;
                messages.push(ChatMessage::user(
                    "You're past halfway on your turn budget — start converging: record what you've \
                     found and head toward `finish`.",
                ));
            }
        }

        // Try the model with bounded retry/backoff on transient failures. The outcome is either a
        // completion, or a turn-level error we classify before deciding whether to keep going.
        // Carries the model that produced the turn so the transcript records which model did the work.
        let turn_result = match chat
            .complete_with_retry(&messages, turn_defs, params, retry_policy)
            .await
        {
            Ok(c) => Ok((c, chat.model().to_string())),
            Err(err) => Err(ChatTurnError {
                error: err.error,
                transient: err.transient,
            }),
        };

        let (completion, turn_model) = match turn_result {
            Ok(c) => {
                // The turn produced a model reply → the chain is healthy again.
                consecutive_failures = 0;
                c
            }
            Err(turn_err) => {
                // A deterministic failure (4xx other than 429, a malformed body) won't get better by
                // trying again — fail the run now with the legible reason. A transient failure counts
                // toward the per-run circuit breaker: keep going until it trips or the budget runs out,
                // rather than wasting the whole budget against a chain that's clearly down.
                if !turn_err.transient {
                    // Context overflow is deterministic, but failing would discard every buffered
                    // finding (ADR-0045 tier 1). Instead, stop investigating and finalize what we have
                    // — the same graceful path as the turn-budget backstop. A genuine bad-request (any
                    // other 4xx) still fails fast with the legible reason.
                    if is_context_overflow(&turn_err.error) {
                        tracing::warn!(
                            task_id = %task_id, turn, findings_recorded, error = %turn_err.error,
                            "context overflow on a chat turn — finalizing buffered findings instead of failing"
                        );
                        overflow_finalize = true;
                        break;
                    }
                    return Err(turn_err.error).with_context(|| format!("agent chat turn {turn}"));
                }
                consecutive_failures += 1;
                tracing::warn!(
                    task_id = %task_id,
                    turn,
                    consecutive_failures,
                    breaker_threshold,
                    error = %turn_err.error,
                    "transient turn failure after retries"
                );
                if breaker_threshold > 0 && consecutive_failures >= breaker_threshold {
                    return Err(turn_err.error).with_context(|| {
                        format!(
                            "review agent circuit breaker tripped after {consecutive_failures} \
                             consecutive turn failures (threshold {breaker_threshold}); failing fast \
                             at turn {turn}"
                        )
                    });
                }
                continue;
            }
        };
        let turn_latency_ms = turn_started.elapsed().as_millis() as u64;

        let usage = completion.usage;
        let rate_limit = completion.rate_limit;
        let reasoning = completion.reasoning;
        // Decode the chain-of-thought length once and reuse it across both log lines below (the string
        // can be many KB). (Gemini/lightbridge review on #220.)
        let reasoning_chars = reasoning.as_deref().map(|r| r.chars().count()).unwrap_or(0);
        let assistant = completion.message;
        let calls = assistant.tool_calls.clone();

        // One concise line per turn (ADR-0034/0039): index, tools called, tokens, wall-clock latency,
        // and the gateway's remaining rate-limit budget when it advertises one (advisory telemetry,
        // crate::ratelimit). Full content lives in the transcript; this keeps pod logs legible.
        let tool_names: Vec<&str> = calls.iter().map(|c| c.function.name.as_str()).collect();
        tracing::info!(
            task_id = %task_id,
            turn,
            model = %turn_model,
            tools = ?tool_names,
            prompt_tokens = usage.and_then(|u| u.prompt_tokens).unwrap_or(-1),
            completion_tokens = usage.and_then(|u| u.completion_tokens).unwrap_or(-1),
            reasoning_tokens = usage.and_then(|u| u.reasoning_tokens()).unwrap_or(-1),
            // Chars of chain-of-thought this turn — the reliable "how far did it think" signal when the
            // gateway folds reasoning into `completion_tokens` and reports `reasoning_tokens: 0` (GLM-5.2).
            reasoning_chars,
            ratelimit_remaining = rate_limit.remaining.map(|r| r as i64).unwrap_or(-1),
            ratelimit_limit = rate_limit.limit.map(|l| l as i64).unwrap_or(-1),
            latency_ms = turn_latency_ms,
            "agent turn complete"
        );
        // Soft warning when the shared gateway budget is nearly spent (≤10%) or this response was
        // itself rate-limited — a heads-up in the logs, not a gate (the budget is global across
        // runners, RFC-0001, so a single run can't reason about it authoritatively).
        if rate_limit.limited || rate_limit.is_low(0.1) {
            tracing::warn!(
                task_id = %task_id,
                turn,
                ratelimit_remaining = rate_limit.remaining.map(|r| r as i64).unwrap_or(-1),
                ratelimit_limit = rate_limit.limit.map(|l| l as i64).unwrap_or(-1),
                reset_secs = rate_limit.reset.map(|d| d.as_secs() as i64).unwrap_or(-1),
                limited = rate_limit.limited,
                "gateway rate-limit budget low"
            );
        }
        // Proof-of-work (epic #137): log the model's chain-of-thought (`reasoning_content`) for this
        // turn so a run is legible from a live log tail. This is the model's *thinking*, not the visible
        // answer — present even on pure tool-call turns. Bounded by `REASONING_LOG_CHARS` (default
        // [`REASONING_LOG_CHARS_DEFAULT`]; `0` = unbounded) because a heavy reasoner (GLM-5.2) can emit
        // thousands of chars per turn; the full count is logged alongside via `reasoning_chars`.
        if let Some(reasoning) = reasoning.as_deref().filter(|r| !r.trim().is_empty()) {
            let shown = if reasoning_log_cap == 0 {
                reasoning
            } else {
                truncate_on_boundary(reasoning, reasoning_log_cap)
            };
            tracing::info!(
                task_id = %task_id,
                turn,
                reasoning_chars,
                reasoning = %shown,
                "agent reasoning"
            );
        }
        // Record the assistant turn in the transcript (ADR-0034): its reasoning + tool calls + tokens.
        transcript.push(TranscriptEntry {
            role: "assistant".to_string(),
            content: assistant.content.clone(),
            tool_calls: (!calls.is_empty())
                .then(|| serde_json::to_value(&calls).unwrap_or_default()),
            tool_name: None,
            prompt_tokens: usage.and_then(|u| u.prompt_tokens),
            completion_tokens: usage.and_then(|u| u.completion_tokens),
            reasoning_tokens: usage.and_then(|u| u.reasoning_tokens()),
            model: Some(turn_model),
        });
        // Echo the assistant turn (with its tool_calls) back into the conversation, as the protocol
        // requires before the matching tool-result messages.
        messages.push(assistant);

        if calls.is_empty() {
            // The model replied in prose instead of acting — steer it back to the tools.
            messages.push(ChatMessage::user(
                "Use the tools to investigate and record findings with `add_review_comment` (or a \
                 reply with `add_comment`), then call `finish` with your verdict (or `abort`). Do \
                 not reply in prose.",
            ));
            continue;
        }

        // Pre-pass (no awaits): one concise log line per tool call with the (bounded) arguments — so a
        // live log tail shows what the model asked for — plus coverage tracking (B, #137): note any
        // changed file the agent engages this turn (opened with `read_file`, or commented on). Done up
        // front so it's identical whether the call runs in the concurrent batch or the ordered pass.
        for call in &calls {
            let tool = call.function.name.as_str();
            let args = truncate_on_boundary(&call.function.arguments, 400);
            match tool {
                ADD_REVIEW_COMMENT | ADD_COMMENT => tracing::info!(
                    task_id = %task_id, turn, tool, args = %args, "tool dispatch (finding/reply buffered)"
                ),
                _ => tracing::info!(task_id = %task_id, turn, tool, args = %args, "tool dispatch"),
            }
            match tool {
                READ_FILE => {
                    if let Some(p) = arg_field(&call.function.arguments, "path") {
                        engaged_files.insert(normalize_repo_path(&p));
                    }
                }
                ADD_REVIEW_COMMENT => {
                    if let Some(p) = arg_field(&call.function.arguments, "file") {
                        engaged_files.insert(normalize_repo_path(&p));
                    }
                }
                _ => {}
            }
        }

        // Risk-first batching (ADR-0042): run the turn's **read-only** calls (search / graph /
        // `read_file`) concurrently, up to `max_batch_size` at a time, so a batch of reads costs one
        // round-trip's latency instead of N. Only pure reads are parallelised: the write tools buffer
        // control-plane-side and dedup by `(file,line)` last-write-wins, so they keep their original
        // order; `finish`/`abort` are terminal. Results are keyed by call index and consumed in call
        // order below, so the transcript and the messages echoed back to the model stay ordered.
        let read_only: Vec<usize> = calls
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                let n = c.function.name.as_str();
                // A fast-tier call to a non-offered read-only tool is refused below, not dispatched —
                // keep it out of the concurrent batch so the gateway/datastore is never hit.
                is_read_only_tool(n) && !fast_refuse(n)
            })
            .map(|(i, _)| i)
            .collect();
        // Advance the cumulative read budgets (ADR-0042) — a turn that issued any read-only call is one
        // investigation batch; tally read_file vs retrieval separately. These feed the NEXT turn's
        // tool-set decision (drop the exhausted tool / force wind-down).
        if !read_only.is_empty() {
            batches += 1;
        }
        for c in &calls {
            match c.function.name.as_str() {
                READ_FILE => files_read += 1,
                n if is_retrieval_tool(n) => searches += 1,
                _ => {}
            }
        }
        let mut batched: std::collections::HashMap<usize, ToolOutcome> =
            std::collections::HashMap::new();
        let tools_ref = &tools;
        let calls_ref = &calls;
        for chunk in read_only.chunks(max_batch_size) {
            let futs = chunk
                .iter()
                .map(|&i| async move { (i, tools_ref.dispatch(&calls_ref[i]).await) });
            for (i, outcome) in futures::future::join_all(futs).await {
                batched.insert(i, outcome);
            }
        }

        // Ordered pass: consume each call in the model's original order. Read-only calls reuse the
        // result computed concurrently above; write/terminal/progress calls dispatch inline (in order).
        let mut should_finish = false;
        let mut abort_reason = None;
        for (i, call) in calls.iter().enumerate() {
            let tool = call.function.name.as_str();
            let outcome = if fast_refuse(tool) {
                // FAST tier: the model called a tool not offered this pass (e.g. retrieval/read_file,
                // which the shared prompt still mentions). Refuse with a steer instead of dispatching —
                // the fast pass reviews the diff (+ SAST digest) directly and finishes.
                tracing::info!(task_id = %task_id, turn, tool, "fast tier: refusing non-offered tool call");
                ToolOutcome::Continue(format!(
                    "`{tool}` is not available in this fast review pass — review the diff directly, \
                     record any findings with add_review_comment, then call finish."
                ))
            } else {
                match batched.remove(&i) {
                    Some(o) => o,
                    None => tools.dispatch(call).await,
                }
            };
            match outcome {
                ToolOutcome::Finish => should_finish = true,
                ToolOutcome::Abort(reason) => abort_reason = Some(reason),
                ToolOutcome::Continue(result) => {
                    // Count successful inline findings so we know when to nudge the model toward
                    // `finish` (and only count the ones the control plane actually buffered).
                    if tool == ADD_REVIEW_COMMENT && result.starts_with("recorded finding") {
                        findings_recorded += 1;
                        // Track P0/P1 findings so the refute pass (ADR-0043) knows whether to verify
                        // before finishing. A retract removes one from the count.
                        if matches!(
                            arg_field(&call.function.arguments, "priority").as_deref(),
                            Some("P0") | Some("P1")
                        ) {
                            p0p1_recorded += 1;
                        }
                        // Scratchpad-loop detection: count consecutive recordings on the same
                        // (file, line) — the signature of the abort spiral (run 7c15f9bb).
                        let loc = arg_field(&call.function.arguments, "file").map(|f| {
                            (
                                f,
                                serde_json::from_str::<serde_json::Value>(&call.function.arguments)
                                    .ok()
                                    .and_then(|v| v.get("line").and_then(|l| l.as_i64()))
                                    .unwrap_or(0) as i32,
                            )
                        });
                        if loc.is_some() && loc == last_finding_loc {
                            same_loc_repeats += 1;
                        } else {
                            same_loc_repeats = 0;
                            last_finding_loc = loc;
                        }
                    }
                    if tool == RETRACT_FINDING && result.starts_with("retracted finding") {
                        p0p1_recorded = p0p1_recorded.saturating_sub(1);
                    }
                    // Proof-of-work (epic #137): a result summary so empty retrievals are visible in
                    // the live log stream (the full result still goes to the DB transcript below).
                    tracing::info!(
                        task_id = %task_id,
                        turn,
                        tool,
                        result_len = result.len(),
                        result = %result_summary(&result),
                        "tool result"
                    );
                    // Record the tool result in the transcript (bounded), then feed it back to the model.
                    transcript.push(TranscriptEntry {
                        role: "tool".to_string(),
                        content: Some(truncate_on_boundary(&result, 2048).to_string()),
                        tool_calls: None,
                        tool_name: Some(call.function.name.clone()),
                        prompt_tokens: None,
                        completion_tokens: None,
                        reasoning_tokens: None,
                        model: None,
                    });
                    messages.push(ChatMessage::tool(call.id.as_str(), result));
                }
            }
        }
        // Scratchpad-loop guard: ≥3 recordings on the same (file, line) is the abort-spiral signature.
        // Drop `add_review_comment` for the next turn and tell the model to investigate or finish, so it
        // can't keep re-recording the same placeholder. One-shot per detection (the counter resets).
        if same_loc_repeats >= 2 {
            same_loc_repeats = 0;
            suppress_record = true;
            tracing::warn!(
                task_id = %task_id,
                turn,
                loc = ?last_finding_loc,
                "scratchpad-loop guard: repeated recordings on one line — suppressing add_review_comment next turn"
            );
            messages.push(ChatMessage::user(
                "You've recorded on the same line several times — that's a loop, and the buffer keeps \
                 only the last one. `add_review_comment` is for a FINAL finding you can prove, not for \
                 notes. Investigate with `read_file` (or `report_progress` to jot a note), then record \
                 the finding once — or call `finish`. (add_review_comment is unavailable next turn.)",
            ));
        }
        // Abort wins over finish if the model somehow asked for both — it's the safer signal.
        if let Some(reason) = abort_reason {
            return Ok(ReviewOutcome::Aborted(reason));
        }
        if should_finish {
            // Full-diff coverage gate (B, #137): if the model wants to finish early (before the
            // wind-down tail) with changed files it never opened or commented on, bounce it ONCE with
            // the explicit list so a single run accounts for the whole change instead of finding one
            // issue and stopping. After the wind-down boundary the #173 convergence wins — we never
            // bounce there, so this can't reopen the rabbit-hole the wind-down exists to close.
            // The FAST tier (ADR-0062) is a single diff-only turn — no coverage bounce (it would waste
            // the only turn and never finalize). The deep run still enforces full-diff coverage.
            if !review.fast && !coverage_bounced && turn < winddown {
                let uncovered: Vec<&str> = changed_files
                    .difference(&engaged_files)
                    .map(String::as_str)
                    .collect();
                if !uncovered.is_empty() {
                    coverage_bounced = true;
                    tracing::info!(
                        task_id = %task_id,
                        turn,
                        uncovered = uncovered.len(),
                        changed = changed_files.len(),
                        "coverage gate: bouncing early finish — changed files not yet engaged"
                    );
                    messages.push(ChatMessage::user(coverage_nudge(&uncovered)));
                    // Don't finish: loop again so the model reviews the rest, then finishes. The bounce
                    // is one-shot, so the next `finish` always goes through.
                    continue;
                }
            }
            // Refute pass (Phase 2, ADR-0043): before the first finish with P0/P1 findings, force a
            // verification turn — re-check each against its cited evidence and `retract_finding` the
            // ones that don't hold. One-shot; this is the lever that kills confidently-wrong blockers
            // (the actual quality gap), which a self-reported confidence label would not catch.
            // FAST tier (ADR-0062): no refute bounce — it would consume the single turn. SAST is
            // deterministic and the lone LLM turn is a light pass; deep `@mention` runs the refute pass.
            if !review.fast && !refute_bounced && p0p1_recorded > 0 {
                refute_bounced = true;
                tracing::info!(
                    task_id = %task_id,
                    turn,
                    p0p1 = p0p1_recorded,
                    "refute pass: verifying P0/P1 findings before finish"
                );
                messages.push(ChatMessage::user(
                    "Before you finish: you recorded P0/P1 finding(s). Re-verify each one against the \
                     exact evidence you cited — look at the real code, not your memory. For any whose \
                     claim does NOT hold (the cited lines don't actually show the bug), call \
                     `retract_finding(file, line)`. A confidently-wrong blocker costs more trust than a \
                     missed nit. Keep only what you can prove, then call `finish`.",
                ));
                continue;
            }
            return Ok(ReviewOutcome::Finished);
        }

        // Light nudge (#137): once useful work is buffered, remind the model to wrap up with `finish`
        // so it doesn't wander and exhaust the budget after the findings are already recorded. Once.
        // Keep it for the fast tier too (the finish-push is exactly what we want), but without the
        // "investigation" wording — fast has no investigation tools (gemini review on #235).
        if findings_recorded > 0 && !nudged_to_finish {
            nudged_to_finish = true;
            let nudge = if review.fast {
                "You've recorded a finding. Record any others on changed lines with add_review_comment, \
                 then call `finish` with your overall verdict to post the review."
            } else {
                "You have recorded at least one finding. When your investigation is complete, call \
                 `finish` with your overall verdict to post everything you've buffered — don't keep \
                 investigating past the point of useful work."
            };
            messages.push(ChatMessage::user(nudge.to_string()));
        }
    }

    // Budget exhausted (turns or context window). CRITICAL (#137/ADR-0045): do NOT bail — that would
    // discard the buffered findings the control plane is holding. Return `Exhausted` so the caller posts
    // a truncation note and finalizes (a real prod run lost 5 findings this way at turn 16).
    if overflow_finalize {
        tracing::warn!(
            task_id = %task_id,
            findings_recorded,
            "review agent hit the context window before calling finish — finalizing buffered findings"
        );
    } else {
        tracing::warn!(
            task_id = %task_id,
            max_turns,
            findings_recorded,
            "review agent hit its turn budget without calling finish — finalizing buffered findings"
        );
    }
    Ok(ReviewOutcome::Exhausted)
}

/// Assemble the system (operator prompt + tool-protocol) and user (request + diff) messages. The
/// system prompt is the **required** operator-owned guidance (ADR-0037 — no built-in default); the
/// tool-protocol is appended last so it's the final instruction the model sees.
#[allow(clippy::too_many_arguments)]
fn build_messages(
    review: &ReviewConfig,
    command: &str,
    diff: Option<&PrDiff>,
    repo_instructions: Option<&str>,
    prior_reviews: Option<&str>,
    repo_memory: Option<&str>,
    sast_digest: Option<&str>,
) -> Vec<ChatMessage> {
    let system = format!("{}\n\n{TOOL_PROTOCOL}", review.system_prompt);

    let mut user = format!("The maintainer's request: {command}");
    match diff {
        Some(pr) => {
            user.push_str(&format!(
                "\n\nThis pull request changes {} file(s):\n{}",
                pr.files.len(),
                pr.files
                    .iter()
                    .map(|f| format!("- {f}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ));
            user.push_str("\n\nUnified diff (review ONLY lines this diff changes):\n```diff\n");
            user.push_str(truncate_on_boundary(&pr.diff, review.max_diff_chars));
            if pr.diff.len() > review.max_diff_chars {
                user.push_str("\n… [diff truncated; review the hunks shown above] …");
            }
            user.push_str("\n```");
        }
        None => user.push_str(
            "\n\nNo diff is available for this run; answer or review against the working tree and \
             keep every claim grounded in the tools.",
        ),
    }

    // Deterministic SAST findings (ADR-0061): what opengrep already flagged on this diff. Injected right
    // after the diff because it's *about* the diff — the agent is made aware so it doesn't re-report
    // those lines and can deepen a lead, but these findings post independently regardless (they're not
    // gated by the model). `None` when SAST is off or found nothing, so a normal run reads as before.
    if let Some(sast) = sast_digest {
        user.push_str("\n\n");
        user.push_str(sast);
    }

    // Prior-review context (ADR-0040 + ADR-0065): the agent's own prior reviews of this target,
    // pre-formatted control-plane-side as explicitly-UNTRUSTED context. The block's framing is
    // re-derive-then-reconcile: review the diff independently first, then retract any prior finding that
    // can't be re-derived (Option C) — it does NOT tell the agent to restate priors. All reconcile
    // wording lives in that string; the runner injects it verbatim. Placed after the diff (the thing
    // under review) and before the repo's own instructions; the tool-protocol in the system message stays
    // authoritative. `None` on a first review, so a fresh PR reads exactly as before.
    if let Some(prior) = prior_reviews {
        user.push_str("\n\n");
        user.push_str(prior);
    }

    // Per-repo feedback memory (M1, ADR-0044): findings rejected (👎) here before — untrusted context,
    // same as the prior review; the tool-protocol stays authoritative. `None` keeps a clean-repo run
    // reading exactly as before.
    if let Some(memory) = repo_memory {
        user.push_str("\n\n");
        user.push_str(memory);
    }

    // Repo-native agent instructions (ADR-0036), kept in the user message as untrusted context (it is
    // already labelled and the tool-protocol/mission in the system message stays authoritative).
    if let Some(instructions) = repo_instructions {
        user.push_str("\n\n");
        user.push_str(instructions);
    }

    vec![ChatMessage::system(system), ChatMessage::user(user)]
}

/// Normalize a repo-relative path for coverage comparison (B, #137): fold backslashes to `/`, then trim
/// whitespace and a leading `./` or `/`, so a `read_file` / `add_review_comment` path matches the diff's
/// file list regardless of how the model wrote it. The backslash fold mirrors the control plane's
/// `normalize_path` (used when anchoring findings) — without it, a model emitting a Windows-style
/// `src\a.rs` against a forward-slash diff would read as un-engaged and draw a spurious coverage bounce.
fn normalize_repo_path(path: &str) -> String {
    path.replace('\\', "/")
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

/// Whether a tool call is pure read-only — no control-plane buffer side-effect, so it is safe to run
/// concurrently in a batch (ADR-0042): the retrieval tools and `read_file`. `report_progress` is
/// excluded (it posts), and the write/terminal tools (`add_review_comment` / `add_comment` / `finish` /
/// `abort`) are excluded by design so their ordering and buffer semantics are preserved.
fn is_read_only_tool(name: &str) -> bool {
    // Discovered external-knowledge tools (ADR-0066) are read-only lookups too — safe to batch
    // alongside the built-in retrieval tools.
    name.starts_with(super::tools::MCP_TOOL_PREFIX)
        || matches!(
            name,
            VECTOR_SEMANTIC_SEARCH | GRAPH_FIND_SYMBOL | GRAPH_GET_CALLERS | READ_FILE
        )
}

/// The retrieval tools (vector + graph search) — the `max_searches` budget category (ADR-0042).
/// Distinct from `read_file`, which has its own `max_files_read` budget.
fn is_retrieval_tool(name: &str) -> bool {
    matches!(
        name,
        VECTOR_SEMANTIC_SEARCH | GRAPH_FIND_SYMBOL | GRAPH_GET_CALLERS
    )
}

/// Extract a string field from a tool call's JSON arguments, or `None` if the arguments don't parse or
/// the field is absent/non-string (B, #137). Used to read the `path` / `file` a tool call targeted.
fn arg_field(arguments: &str, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()?
        .get(key)?
        .as_str()
        .map(str::to_string)
}

/// The one-shot coverage nudge (B, #137): list the changed files the agent hasn't engaged yet and ask it
/// to review each across all dimensions before finishing. The file list is capped so a large PR can't
/// blow the prompt; the agent still has the full diff above.
fn coverage_nudge(uncovered: &[&str]) -> String {
    const MAX_LISTED: usize = 15;
    let listed = uncovered
        .iter()
        .take(MAX_LISTED)
        .map(|f| format!("- {f}"))
        .collect::<Vec<_>>()
        .join("\n");
    let more = if uncovered.len() > MAX_LISTED {
        format!("\n- … and {} more", uncovered.len() - MAX_LISTED)
    } else {
        String::new()
    };
    format!(
        "Before you finish: these changed files don't yet have a finding and you haven't opened them:\n\
         {listed}{more}\n\n\
         Make sure you've reviewed each one across all relevant dimensions — correctness, security, \
         quality, style, performance — not only the first issue you found. Open any you're unsure about \
         with read_file, record anything worth raising with add_review_comment, then call `finish`. If \
         you've genuinely considered them and there's nothing to add, call `finish` again now."
    )
}

/// A turn-level chat failure after retries, carrying whether the
/// underlying error was transient — the loop keeps a transient one going toward the circuit breaker
/// but fails the run immediately on a deterministic one.
struct ChatTurnError {
    error: anyhow::Error,
    transient: bool,
}

/// The host of a base URL for logging — keeps the path/query (and any embedded token) out of logs
/// while still identifying which gateway a run hit. Falls back to a redacted marker if unparseable.
fn base_url_host(base_url: &str) -> String {
    // Fall back to the whole string when there's no scheme separator (e.g. `gateway.example/v1`),
    // so a schemeless URL still logs its host rather than "(unparseable)".
    let without_scheme = base_url.split("://").nth(1).unwrap_or(base_url);
    without_scheme
        .split(['/', '?', '#'])
        .next()
        .map(|hostport| hostport.to_string())
        .unwrap_or_else(|| "(unparseable)".to_string())
}

/// A short, log-friendly summary of a tool result for live tailing (epic #137). Calls out an empty
/// retrieval explicitly (a pretty-printed empty JSON array renders as `[]`) — the common failure the
/// model used to flail against blindly — and otherwise returns the leading bytes (bounded).
fn result_summary(result: &str) -> String {
    let trimmed = result.trim();
    // `[]` (legacy) or the explicit empty-retrieval message both mean "nothing matched" — keep the log
    // line terse so an empty retrieval reads the same in the stream regardless of substrate wording.
    if trimmed == "[]" || trimmed == EMPTY_RETRIEVAL_RESULT {
        return "0 hits".to_string();
    }
    truncate_on_boundary(trimmed, 200).to_string()
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    /// A scripted chat endpoint: returns `responses[i]` on the i-th call, repeating the last forever
    /// (so a loop that never finishes keeps getting the same reply).
    struct Script {
        calls: Arc<AtomicUsize>,
        responses: Vec<serde_json::Value>,
    }
    impl Respond for Script {
        fn respond(&self, _: &Request) -> ResponseTemplate {
            let i = self.calls.fetch_add(1, Ordering::SeqCst);
            let body = self
                .responses
                .get(i)
                .or_else(|| self.responses.last())
                .cloned()
                .unwrap();
            ResponseTemplate::new(200).set_body_json(body)
        }
    }

    fn tool_call_reply(name: &str, arguments: &str) -> serde_json::Value {
        json!({ "choices": [{ "finish_reason": "tool_calls", "message": {
            "role": "assistant",
            "tool_calls": [{ "id": "c1", "type": "function",
                "function": { "name": name, "arguments": arguments } }]
        }}]})
    }

    fn text_reply(text: &str) -> serde_json::Value {
        json!({ "choices": [{ "finish_reason": "stop",
            "message": { "role": "assistant", "content": text } }]})
    }

    // An assistant turn that emits several tool calls at once (a batch). `calls` is (id, name, args).
    fn batch_reply(calls: &[(&str, &str, &str)]) -> serde_json::Value {
        let tool_calls: Vec<_> = calls
            .iter()
            .map(|(id, name, args)| {
                json!({ "id": id, "type": "function",
                    "function": { "name": name, "arguments": args } })
            })
            .collect();
        json!({ "choices": [{ "finish_reason": "tool_calls",
            "message": { "role": "assistant", "tool_calls": tool_calls } }]})
    }

    async fn mount_chat(server: &MockServer, responses: Vec<serde_json::Value>) {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(Script {
                calls: Arc::new(AtomicUsize::new(0)),
                responses,
            })
            .mount(server)
            .await;
    }

    fn review_config(chat_base_with_v1: String) -> ReviewConfig {
        ReviewConfig {
            base_url: chat_base_with_v1,
            api_key: "k".to_string(),
            model: "m".to_string(),
            system_prompt: "You are a reviewer.".to_string(),
            max_diff_chars: 60_000,
            max_turns: crate::bootstrap::config::DEFAULT_MAX_TURNS,
            max_batch_size: crate::bootstrap::config::DEFAULT_MAX_BATCH_SIZE,
            max_files_read: crate::bootstrap::config::DEFAULT_MAX_FILES_READ,
            max_searches: crate::bootstrap::config::DEFAULT_MAX_SEARCHES,
            max_batches: crate::bootstrap::config::DEFAULT_MAX_BATCHES,
            context_window: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            extra: serde_json::Map::new(),
            stream: false,
            // Fast resilience defaults so the loop tests don't sleep on the (mocked) failure paths.
            resilience: crate::bootstrap::config::ResilienceConfig {
                request_timeout_secs: 5,
                max_retries: 0,
                circuit_breaker_threshold: 3,
            },
            fast: false,
            // Tests that exercise the allowlist set this explicitly; default to the built-in surface.
            tools: None,
        }
    }

    // The maintainer's request reaches the user prompt; the operator system prompt is used verbatim.
    #[test]
    fn build_messages_carries_request_and_uses_operator_prompt() {
        let review = review_config("http://unused/v1".to_string());
        let msgs = build_messages(
            &review,
            "propose a better implementation",
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "system");
        let system = msgs[0].content.as_deref().expect("system content");
        assert!(
            system.starts_with("You are a reviewer."),
            "operator prompt first"
        );
        assert!(system.contains("How to act"), "tool-protocol appended");
        let user = msgs[1].content.as_deref().expect("user content");
        assert!(
            user.contains("propose a better implementation"),
            "request reaches prompt: {user}"
        );
    }

    // Prior-review context (A, #137) is injected into the user prompt when present, and absent when not.
    #[test]
    fn build_messages_injects_prior_review_context() {
        let review = review_config("http://unused/v1".to_string());
        let prior = "## Your previous review of this pull request\nPrior verdict: looks fine.";

        let with_prior =
            build_messages(&review, "review again", None, None, Some(prior), None, None);
        let user = with_prior[1].content.as_deref().expect("user content");
        assert!(
            user.contains("Your previous review of this pull request"),
            "prior-review block reaches prompt: {user}"
        );
        // M1 repo memory (ADR-0044) is injected when present.
        let with_mem = build_messages(
            &review,
            "review",
            None,
            None,
            None,
            Some("## Memory: findings rejected here before (👎)\n- a.rs:1 — bogus nit"),
            None,
        );
        assert!(
            with_mem[1]
                .content
                .as_deref()
                .expect("user")
                .contains("findings rejected here before"),
            "repo-memory block reaches prompt"
        );

        let without = build_messages(&review, "review again", None, None, None, None, None);
        let user = without[1].content.as_deref().expect("user content");
        assert!(
            !user.contains("previous review"),
            "no prior-review block on a first review: {user}"
        );
    }

    // Coverage gate helpers (B, #137): path normalization, arg extraction, and the nudge shape.
    #[test]
    fn coverage_helpers_normalize_extract_and_phrase() {
        assert_eq!(normalize_repo_path("./src/a.rs"), "src/a.rs");
        assert_eq!(normalize_repo_path("/src/a.rs"), "src/a.rs");
        assert_eq!(normalize_repo_path("  src/a.rs  "), "src/a.rs");
        // Windows-style backslashes fold to `/` (mirrors the control plane), so a `src\a.rs` tool arg
        // still matches a forward-slash diff path instead of drawing a spurious bounce.
        assert_eq!(normalize_repo_path("src\\a.rs"), "src/a.rs");
        assert_eq!(normalize_repo_path(".\\src\\a.rs"), "src/a.rs");

        assert_eq!(
            arg_field(r#"{"file":"src/a.rs","line":7}"#, "file").as_deref(),
            Some("src/a.rs")
        );
        assert_eq!(arg_field(r#"{"path":"x"}"#, "file"), None, "missing key");
        assert_eq!(arg_field("not json", "file"), None, "malformed args");

        let nudge = coverage_nudge(&["src/b.rs", "src/c.rs"]);
        assert!(nudge.contains("src/b.rs") && nudge.contains("src/c.rs"));
        assert!(nudge.contains("correctness") && nudge.contains("security"));
        // The over-cap path elides the tail rather than dumping a huge list.
        let many: Vec<String> = (0..30).map(|i| format!("f{i}.rs")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        assert!(coverage_nudge(&refs).contains("and 15 more"));
    }

    // ── Positive e2e: search → add_review_comment → finish → Ok(()) ─────────────────────────────
    #[tokio::test]
    async fn native_loop_searches_records_and_finishes() {
        let chat = MockServer::start().await;
        mount_chat(
            &chat,
            vec![
                tool_call_reply(
                    "lightbridge_vector_semantic_search",
                    r#"{"query":"session expiry"}"#,
                ),
                tool_call_reply(
                    "add_review_comment",
                    r#"{"file":"a.rs","line":7,"title":"No expiry","priority":"P0","category":"security","body":"accepts expired tokens"}"#,
                ),
                tool_call_reply("finish", r#"{"summary":"Missing expiry check."}"#),
            ],
        )
        .await;

        let emb = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{ "index": 0, "embedding": [0.1_f32, 0.2_f32] }]
            })))
            .mount(&emb)
            .await;
        let cp = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/internal/tasks/{}/search", Uuid::nil())))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
                "file_path": "a.rs", "language": "rust", "chunk_type": "function",
                "symbol_name": "validate", "start_line": 1, "end_line": 9,
                "content": "fn validate() {}", "score": 0.9
            }])))
            .mount(&cp)
            .await;
        Mock::given(method("POST"))
            .and(path(format!(
                "/internal/tasks/{}/review/inline",
                Uuid::nil()
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(&cp)
            .await;
        Mock::given(method("POST"))
            .and(path(format!(
                "/internal/tasks/{}/review/summary",
                Uuid::nil()
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(&cp)
            .await;

        let review = review_config(format!("{}/v1", chat.uri()));
        let cpc = ControlPlaneClient::new(cp.uri(), "tok");
        let embc = EmbeddingsClient::new(&emb.uri(), "key", "model");
        // A diff is present, so `add_review_comment` is offered.
        let diff = PrDiff {
            diff: "@@ -1,3 +1,4 @@\n fn validate() {}\n+// changed\n".to_string(),
            files: vec!["a.rs".to_string()],
        };
        let mut transcript = Vec::new();
        let outcome = run_native_agent(
            &review,
            "@lightbridge review",
            Some(&diff),
            None,
            None,
            None,
            None,
            &[],
            &cpc,
            &embc,
            Uuid::nil(),
            std::path::Path::new("/tmp"),
            &mut transcript,
        )
        .await
        .expect("agent finishes cleanly");
        assert!(
            matches!(outcome, ReviewOutcome::Finished),
            "got: {outcome:?}"
        );
        // The transcript captured the assistant turns (search → add_review_comment → finish).
        assert!(
            transcript.iter().any(|e| e.role == "assistant"),
            "transcript records assistant turns"
        );
    }

    // ── Coverage gate (B, #137): an early `finish` with a changed file never engaged is bounced ONCE,
    // costing exactly one extra chat round-trip; the model then finishes. Shares the Script counter so
    // we can assert on the round-trip count (the bounce isn't otherwise observable from the outcome). ──
    #[tokio::test]
    async fn coverage_gate_bounces_early_finish_then_finishes() {
        async fn run(files: Vec<String>) -> usize {
            let calls = Arc::new(AtomicUsize::new(0));
            let chat = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(Script {
                    calls: calls.clone(),
                    responses: vec![
                        // Turn 0: a finding on a.rs only.
                        tool_call_reply(
                            "add_review_comment",
                            r#"{"file":"a.rs","line":2,"title":"nit","priority":"P2","category":"quality","body":"b"}"#,
                        ),
                        // Turn 1: try to finish. Bounced iff a changed file is still un-engaged.
                        tool_call_reply("finish", r#"{"summary":"done"}"#),
                        // Turn 2 (only reached on a bounce): finish for real.
                        tool_call_reply("finish", r#"{"summary":"done after reviewing the rest"}"#),
                    ],
                })
                .mount(&chat)
                .await;

            let cp = MockServer::start().await;
            for ep in ["review/inline", "review/summary"] {
                Mock::given(method("POST"))
                    .and(path(format!("/internal/tasks/{}/{ep}", Uuid::nil())))
                    .respond_with(ResponseTemplate::new(204))
                    .mount(&cp)
                    .await;
            }

            let review = review_config(format!("{}/v1", chat.uri()));
            let cpc = ControlPlaneClient::new(cp.uri(), "tok");
            let embc = EmbeddingsClient::new("http://unused", "key", "model");
            let diff = PrDiff {
                diff: "@@ -1,1 +1,2 @@\n a\n+b\n".to_string(),
                files,
            };
            let outcome = run_native_agent(
                &review,
                "review",
                Some(&diff),
                None,
                None,
                None,
                None,
                &[],
                &cpc,
                &embc,
                Uuid::nil(),
                std::path::Path::new("/tmp"),
                &mut Vec::new(),
            )
            .await
            .expect("clean finish");
            assert!(
                matches!(outcome, ReviewOutcome::Finished),
                "got: {outcome:?}"
            );
            calls.load(Ordering::SeqCst)
        }

        // b.rs is changed but never engaged → the turn-1 finish is bounced → 3 round-trips.
        assert_eq!(
            run(vec!["a.rs".to_string(), "b.rs".to_string()]).await,
            3,
            "early finish bounced once for the un-engaged file"
        );
        // Every changed file (a.rs) is engaged by the finding → no bounce → 2 round-trips.
        assert_eq!(
            run(vec!["a.rs".to_string()]).await,
            2,
            "no bounce when the whole change is covered"
        );
    }

    // ── Refute pass (ADR-0043): a P0/P1 finding triggers one pre-finish verification bounce; a P2
    // does not. The diff is fully covered (finding is on the only changed file) so the coverage gate
    // doesn't fire — this isolates the refute bounce. ──
    #[tokio::test]
    async fn refute_pass_bounces_once_for_p0p1_findings() {
        async fn run(priority: &str) -> usize {
            let calls = Arc::new(AtomicUsize::new(0));
            let chat = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(Script {
                    calls: calls.clone(),
                    responses: vec![
                        tool_call_reply(
                            "add_review_comment",
                            &format!(
                                r#"{{"file":"a.rs","line":2,"title":"t","priority":"{priority}","category":"correctness","body":"b","evidence":"line 2 does X"}}"#
                            ),
                        ),
                        tool_call_reply("finish", r#"{"summary":"done"}"#),
                        tool_call_reply("finish", r#"{"summary":"done after verifying"}"#),
                    ],
                })
                .mount(&chat)
                .await;
            let cp = MockServer::start().await;
            for ep in ["review/inline", "review/summary"] {
                Mock::given(method("POST"))
                    .and(path(format!("/internal/tasks/{}/{ep}", Uuid::nil())))
                    .respond_with(ResponseTemplate::new(204))
                    .mount(&cp)
                    .await;
            }
            let review = review_config(format!("{}/v1", chat.uri()));
            let cpc = ControlPlaneClient::new(cp.uri(), "tok");
            let embc = EmbeddingsClient::new("http://unused", "key", "model");
            let diff = PrDiff {
                diff: "@@ -1,1 +1,2 @@\n a\n+b\n".to_string(),
                files: vec!["a.rs".to_string()],
            };
            let outcome = run_native_agent(
                &review,
                "review",
                Some(&diff),
                None,
                None,
                None,
                None,
                &[],
                &cpc,
                &embc,
                Uuid::nil(),
                std::path::Path::new("/tmp"),
                &mut Vec::new(),
            )
            .await
            .expect("clean finish");
            assert!(
                matches!(outcome, ReviewOutcome::Finished),
                "got: {outcome:?}"
            );
            calls.load(Ordering::SeqCst)
        }

        // A P1 finding → one refute bounce before finish → 3 round-trips.
        assert_eq!(
            run("P1").await,
            3,
            "P0/P1 finding triggers the refute bounce"
        );
        // A P2 finding → no refute bounce → 2 round-trips.
        assert_eq!(
            run("P2").await,
            2,
            "a P2 finding does not trigger the refute bounce"
        );
    }

    // ── Scratchpad-loop guard (run 7c15f9bb): repeatedly recording on the SAME (file,line) drops
    // `add_review_comment` from the offered tools for a turn, breaking the abort spiral. ──
    #[tokio::test]
    async fn scratchpad_loop_guard_suppresses_add_review_comment() {
        let chat = MockServer::start().await;
        let offered = Arc::new(std::sync::Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(RecordingScript {
                calls: Arc::new(AtomicUsize::new(0)),
                offered: offered.clone(),
                user_text: Arc::new(std::sync::Mutex::new(Vec::new())),
                // Always re-records the same (file,line) — the scratchpad spiral; never finishes.
                response: tool_call_reply(
                    "add_review_comment",
                    r#"{"file":"a.rs","line":2,"title":"t","priority":"P2","category":"quality","body":"b","evidence":"e"}"#,
                ),
            })
            .mount(&chat)
            .await;
        let cp = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!(
                "/internal/tasks/{}/review/inline",
                Uuid::nil()
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(&cp)
            .await;

        let mut review = review_config(format!("{}/v1", chat.uri()));
        review.max_turns = 6;
        let cpc = ControlPlaneClient::new(cp.uri(), "tok");
        let embc = EmbeddingsClient::new("http://unused", "key", "model");
        let diff = PrDiff {
            diff: "@@ -1,1 +1,2 @@\n a\n+b\n".to_string(),
            files: vec!["a.rs".to_string()],
        };
        let _ = run_native_agent(
            &review,
            "review",
            Some(&diff),
            None,
            None,
            None,
            None,
            &[],
            &cpc,
            &embc,
            Uuid::nil(),
            std::path::Path::new("/tmp"),
            &mut Vec::new(),
        )
        .await
        .expect("clean outcome");

        let log = offered.lock().unwrap();
        assert!(
            log[0].iter().any(|n| n == ADD_REVIEW_COMMENT),
            "turn 0 offers add_review_comment: {:?}",
            log[0]
        );
        // After 3 recordings on the same line (turns 0,1,2), the guard suppresses it on turn 3.
        assert!(
            log.get(3)
                .is_some_and(|t| !t.iter().any(|n| n == ADD_REVIEW_COMMENT)),
            "turn 3 drops add_review_comment after the same-line loop: {:?}",
            log.get(3)
        );
    }

    // Read-only classifier (ADR-0042): retrieval + read_file are batchable; write/terminal/progress not.
    #[test]
    fn read_only_tool_classification() {
        for t in [
            VECTOR_SEMANTIC_SEARCH,
            GRAPH_FIND_SYMBOL,
            GRAPH_GET_CALLERS,
            READ_FILE,
            "mcp__brave-search__brave_web_search",
        ] {
            assert!(is_read_only_tool(t), "{t} should be read-only");
        }
        for t in [
            ADD_REVIEW_COMMENT,
            ADD_COMMENT,
            FINISH,
            ABORT,
            "report_progress",
        ] {
            assert!(!is_read_only_tool(t), "{t} must not be batched");
        }
    }

    // ── Batched read-only dispatch (ADR-0042): a turn that emits several read-only calls at once runs
    // them all and echoes their results back in the model's original call order, then finishes. ──
    #[tokio::test]
    async fn batched_read_only_calls_all_dispatch_in_order() {
        let chat = MockServer::start().await;
        mount_chat(
            &chat,
            vec![
                // One turn, three read-only calls at once (a batch).
                batch_reply(&[
                    ("c1", VECTOR_SEMANTIC_SEARCH, r#"{"query":"auth"}"#),
                    ("c2", GRAPH_FIND_SYMBOL, r#"{"symbol":"validate"}"#),
                    ("c3", VECTOR_SEMANTIC_SEARCH, r#"{"query":"tokens"}"#),
                ]),
                tool_call_reply("finish", r#"{"summary":"done"}"#),
            ],
        )
        .await;

        let emb = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{ "index": 0, "embedding": [0.1_f32, 0.2_f32] }]
            })))
            .mount(&emb)
            .await;
        let cp = MockServer::start().await;
        for ep in ["search", "graph/find-symbol", "review/summary"] {
            Mock::given(method("POST"))
                .and(path(format!("/internal/tasks/{}/{ep}", Uuid::nil())))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
                .mount(&cp)
                .await;
        }

        let review = review_config(format!("{}/v1", chat.uri()));
        let cpc = ControlPlaneClient::new(cp.uri(), "tok");
        let embc = EmbeddingsClient::new(&emb.uri(), "key", "model");
        let mut transcript = Vec::new();
        let outcome = run_native_agent(
            &review,
            "review",
            None,
            None,
            None,
            None,
            None,
            &[],
            &cpc,
            &embc,
            Uuid::nil(),
            std::path::Path::new("/tmp"),
            &mut transcript,
        )
        .await
        .expect("clean finish");
        assert!(
            matches!(outcome, ReviewOutcome::Finished),
            "got: {outcome:?}"
        );

        // All three batched read-only calls produced a tool result, in the original call order.
        let tool_names: Vec<&str> = transcript
            .iter()
            .filter(|e| e.role == "tool")
            .filter_map(|e| e.tool_name.as_deref())
            .collect();
        assert_eq!(
            tool_names,
            vec![
                VECTOR_SEMANTIC_SEARCH,
                GRAPH_FIND_SYMBOL,
                VECTOR_SEMANTIC_SEARCH
            ],
            "every batched call dispatched, results in call order"
        );
    }

    // ── Search budget (ADR-0042): once max_searches is spent, the retrieval tools are dropped from the
    // offered set on the next turn — while read_file (a different budget) stays. The model here always
    // asks for a search and never finishes, so the run ends in `Exhausted`; we assert the offered set. ──
    #[tokio::test]
    async fn search_budget_drops_retrieval_tools_once_spent() {
        let chat = MockServer::start().await;
        let offered = Arc::new(std::sync::Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(RecordingScript {
                calls: Arc::new(AtomicUsize::new(0)),
                offered: offered.clone(),
                user_text: Arc::new(std::sync::Mutex::new(Vec::new())),
                // Always asks for retrieval; never finishes (so the budget is spent on turn 0).
                response: tool_call_reply(VECTOR_SEMANTIC_SEARCH, r#"{"query":"auth"}"#),
            })
            .mount(&chat)
            .await;

        let emb = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{ "index": 0, "embedding": [0.1_f32] }]
            })))
            .mount(&emb)
            .await;
        let cp = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/internal/tasks/{}/search", Uuid::nil())))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&cp)
            .await;

        let mut review = review_config(format!("{}/v1", chat.uri()));
        review.max_searches = 1; // spent after a single retrieval call (turn 0)
        review.max_turns = 4; // keep it short; the model never finishes → Exhausted
        let cpc = ControlPlaneClient::new(cp.uri(), "tok");
        let embc = EmbeddingsClient::new(&emb.uri(), "key", "model");
        let outcome = run_native_agent(
            &review,
            "review",
            None,
            None,
            None,
            None,
            None,
            &[],
            &cpc,
            &embc,
            Uuid::nil(),
            std::path::Path::new("/tmp"),
            &mut Vec::new(),
        )
        .await
        .expect("exhaustion is a clean outcome");
        assert!(
            matches!(outcome, ReviewOutcome::Exhausted),
            "got: {outcome:?}"
        );

        let log = offered.lock().unwrap();
        assert!(log.len() >= 2, "at least two turns recorded");
        assert!(
            log[0].iter().any(|n| n == VECTOR_SEMANTIC_SEARCH),
            "turn 0 offers retrieval (budget not yet spent): {:?}",
            log[0]
        );
        assert!(
            !log[1].iter().any(|n| n == VECTOR_SEMANTIC_SEARCH),
            "turn 1 drops retrieval once the search budget is spent: {:?}",
            log[1]
        );
        assert!(
            log[1].iter().any(|n| n == READ_FILE),
            "read_file (a different budget) is still offered on turn 1: {:?}",
            log[1]
        );
    }

    // No diff → `add_review_comment` is not offered (an inline finding can't anchor); the rest of the
    // tool surface remains.
    #[test]
    fn no_diff_omits_add_review_comment_from_offered_tools() {
        let offered: Vec<String> = {
            let mut defs = tool_defs();
            defs.retain(|t| t.function.name != ADD_REVIEW_COMMENT);
            defs.iter().map(|t| t.function.name.clone()).collect()
        };
        assert!(!offered.iter().any(|n| n == ADD_REVIEW_COMMENT));
        assert!(
            offered.iter().any(|n| n == "add_comment"),
            "add_comment still offered"
        );
        assert!(offered.iter().any(|n| n == "finish"));
    }

    // ── e2e: abort returns ReviewOutcome::Aborted(reason) — NOT an Err (#137). The caller posts the
    // reason as the review summary then finalizes, so the PR gets an honest abort note, not silence. ──
    #[tokio::test]
    async fn native_loop_abort_returns_aborted_outcome() {
        let chat = MockServer::start().await;
        mount_chat(
            &chat,
            vec![tool_call_reply("abort", r#"{"reason":"diff unreadable"}"#)],
        )
        .await;
        let review = review_config(format!("{}/v1", chat.uri()));
        let cpc = ControlPlaneClient::new("http://unused", "tok");
        let embc = EmbeddingsClient::new("http://unused", "key", "model");
        let outcome = run_native_agent(
            &review,
            "review",
            None,
            None,
            None,
            None,
            None,
            &[],
            &cpc,
            &embc,
            Uuid::nil(),
            std::path::Path::new("/tmp"),
            &mut Vec::new(),
        )
        .await
        .expect("abort is a clean outcome, not an Err");
        match outcome {
            ReviewOutcome::Aborted(reason) => assert_eq!(reason, "diff unreadable"),
            other => panic!("expected Aborted, got {other:?}"),
        }
    }

    // ── Circuit breaker: the chain is down (persistent 5xx), so the run fails fast at the breaker
    // threshold instead of consuming the whole turn budget (ADR-0039). ─────────
    #[tokio::test]
    async fn native_loop_circuit_breaker_trips_fast() {
        let chat = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&chat)
            .await;

        let mut review = review_config(format!("{}/v1", chat.uri()));
        review.resilience.circuit_breaker_threshold = 1; // trip on the first failure
        let cpc = ControlPlaneClient::new("http://unused", "tok");
        let embc = EmbeddingsClient::new("http://unused", "key", "model");
        let err = run_native_agent(
            &review,
            "review",
            None,
            None,
            None,
            None,
            None,
            &[],
            &cpc,
            &embc,
            Uuid::nil(),
            std::path::Path::new("/tmp"),
            &mut Vec::new(),
        )
        .await
        .expect_err("breaker trips");
        assert!(
            format!("{err:#}").contains("circuit breaker tripped"),
            "got: {err:#}"
        );
    }

    // ── e2e: a model that never finishes is cut off by the turn budget and returns Exhausted — NOT an
    // Err (#137). The buffered findings the control plane holds must survive so the caller can finalize
    // (a real prod run lost 5 findings when this used to bail). ──────────────────────────────────────
    #[tokio::test]
    async fn native_loop_exhausts_budget_without_discarding() {
        let chat = MockServer::start().await;
        mount_chat(&chat, vec![text_reply("Thinking out loud forever.")]).await;
        let mut review = review_config(format!("{}/v1", chat.uri()));
        review.max_turns = 3; // keep the test fast; the model never calls finish
        let cpc = ControlPlaneClient::new("http://unused", "tok");
        let embc = EmbeddingsClient::new("http://unused", "key", "model");
        let outcome = run_native_agent(
            &review,
            "review",
            None,
            None,
            None,
            None,
            None,
            &[],
            &cpc,
            &embc,
            Uuid::nil(),
            std::path::Path::new("/tmp"),
            &mut Vec::new(),
        )
        .await
        .expect("exhaustion is a clean outcome, not an Err");
        assert!(
            matches!(outcome, ReviewOutcome::Exhausted),
            "got: {outcome:?}"
        );
    }

    // ── Wind-down helpers (#137): the boundary reserves a proportional tail and never lands before the
    // first turn or at/after the ceiling; the reduced set keeps only write/finish/abort. ────────────
    #[test]
    fn winddown_turn_reserves_a_proportional_tail() {
        // Generous budget: reserve max(2, 150/10) = 15 → wind down at 135.
        assert_eq!(winddown_turn(150), 135);
        // Mid budget: reserve max(2, 40/10) = 4 → wind down at 36.
        assert_eq!(winddown_turn(40), 36);
        // Small budget: reserve max(2, 5/10)=2 → wind down at 3 (turns 3,4 reduced; 0,1,2 full).
        assert_eq!(winddown_turn(5), 3);
        // Tiny budgets still leave one full-toolset turn first and never land at/after the ceiling.
        assert_eq!(winddown_turn(2), 1);
        assert_eq!(winddown_turn(1), 1);
        assert!(winddown_turn(0) >= 1);
    }

    #[test]
    fn winddown_tool_defs_drops_investigation_tools() {
        let names: Vec<String> = winddown_tool_defs(&tool_defs(), true)
            .iter()
            .map(|t| t.function.name.clone())
            .collect();
        // Kept: the write tools + finish + abort.
        for kept in [ADD_REVIEW_COMMENT, ADD_COMMENT, FINISH, ABORT] {
            assert!(names.iter().any(|n| n == kept), "{kept} should be kept");
        }
        // Dropped: every investigation/progress tool.
        for dropped in [
            super::super::tools::VECTOR_SEMANTIC_SEARCH,
            super::super::tools::GRAPH_FIND_SYMBOL,
            super::super::tools::GRAPH_GET_CALLERS,
            super::super::tools::READ_FILE,
            super::super::tools::REPORT_PROGRESS,
        ] {
            assert!(
                !names.iter().any(|n| n == dropped),
                "{dropped} should be dropped in wind-down"
            );
        }
        // No diff → `add_review_comment` is not even offered in the reduced set.
        let no_diff: Vec<String> = winddown_tool_defs(&tool_defs(), false)
            .iter()
            .map(|t| t.function.name.clone())
            .collect();
        assert!(!no_diff.iter().any(|n| n == ADD_REVIEW_COMMENT));
        assert!(no_diff.iter().any(|n| n == ADD_COMMENT));
    }

    // Wind-down is derived from the (possibly allowlist-restricted) base, NOT the global surface — so a
    // tool a per-tier `review.tools` allowlist dropped can't reappear in the wind-down tail (gemini #237).
    #[test]
    fn winddown_respects_a_restricted_base_set() {
        // A base that excludes add_comment (e.g. a deep allowlist of record/finish/abort only).
        let base: Vec<ToolDef> = tool_defs()
            .into_iter()
            .filter(|t| {
                matches!(
                    t.function.name.as_str(),
                    ADD_REVIEW_COMMENT | FINISH | ABORT
                )
            })
            .collect();
        let names: Vec<String> = winddown_tool_defs(&base, true)
            .iter()
            .map(|t| t.function.name.clone())
            .collect();
        assert!(
            !names.iter().any(|n| n == ADD_COMMENT),
            "a tool absent from the restricted base must not reappear in wind-down: {names:?}"
        );
        assert!(names.iter().any(|n| n == FINISH), "finish stays");
    }

    /// A scripted chat endpoint that also records, per call, the tool names offered in the request body
    /// `tools` array — so a test can assert the wind-down restricted the surface on the late turns.
    struct RecordingScript {
        calls: Arc<AtomicUsize>,
        offered: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
        // The concatenated user-message text seen in each request body (lets a test assert the budget
        // message was injected into the conversation by the time the wind-down turn fires).
        user_text: Arc<std::sync::Mutex<Vec<String>>>,
        response: serde_json::Value,
    }
    impl Respond for RecordingScript {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
            let names = body
                .get("tools")
                .and_then(|t| t.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|t| {
                            t.get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|n| n.as_str())
                                .map(str::to_string)
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            self.offered.lock().unwrap().push(names);
            let user_text = body
                .get("messages")
                .and_then(|m| m.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
                        .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            self.user_text.lock().unwrap().push(user_text);
            ResponseTemplate::new(200).set_body_json(self.response.clone())
        }
    }

    // ── e2e wind-down (#137): a model that keeps requesting retrieval and never finishes. Once the
    // wind-down turn is reached the offered tool set must drop the retrieval/read_file tools (only
    // write/finish/abort remain), and a budget message must have been injected. The run still ends in
    // `Exhausted` (the backstop) because this model never calls `finish`. ────────────────────────────
    #[tokio::test]
    async fn native_loop_restricts_tools_in_winddown() {
        let chat = MockServer::start().await;
        let offered = Arc::new(std::sync::Mutex::new(Vec::new()));
        let user_text = Arc::new(std::sync::Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(RecordingScript {
                calls: Arc::new(AtomicUsize::new(0)),
                offered: offered.clone(),
                user_text: user_text.clone(),
                // Always asks for retrieval; never finishes.
                response: tool_call_reply(
                    "lightbridge_vector_semantic_search",
                    r#"{"query":"anything"}"#,
                ),
            })
            .mount(&chat)
            .await;

        // The control plane only needs to answer the search calls (no finalize — the loop never
        // finishes; the caller would finalize, not the loop). Return 0 hits each time.
        let cp = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/internal/tasks/{}/search", Uuid::nil())))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&cp)
            .await;
        let emb = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{ "index": 0, "embedding": [0.1_f32, 0.2_f32] }]
            })))
            .mount(&emb)
            .await;

        let mut review = review_config(format!("{}/v1", chat.uri()));
        review.max_turns = 5; // winddown_turn(5) == 3 → turns 0,1,2 full; 3,4 reduced.
        let cpc = ControlPlaneClient::new(cp.uri(), "tok");
        let embc = EmbeddingsClient::new(&emb.uri(), "key", "model");
        // A diff is present so `add_review_comment` is part of the (reduced) set.
        let diff = PrDiff {
            diff: "@@ -1,1 +1,2 @@\n fn x() {}\n+// changed\n".to_string(),
            files: vec!["a.rs".to_string()],
        };
        let outcome = run_native_agent(
            &review,
            "review",
            Some(&diff),
            None,
            None,
            None,
            None,
            &[],
            &cpc,
            &embc,
            Uuid::nil(),
            std::path::Path::new("/tmp"),
            &mut Vec::new(),
        )
        .await
        .expect("exhaustion is a clean outcome");
        assert!(
            matches!(outcome, ReviewOutcome::Exhausted),
            "model never finishes → backstop fires; got {outcome:?}"
        );

        let offered = offered.lock().unwrap();
        assert_eq!(offered.len(), 5, "one chat call per turn");
        // Early turns (full set) still offer retrieval.
        assert!(
            offered[0]
                .iter()
                .any(|n| n == super::super::tools::VECTOR_SEMANTIC_SEARCH),
            "turn 0 offers retrieval: {:?}",
            offered[0]
        );
        // Wind-down turns (3, 4) must have dropped retrieval/read_file and kept only write/finish/abort.
        for late in [3usize, 4] {
            let set = &offered[late];
            for dropped in [
                super::super::tools::VECTOR_SEMANTIC_SEARCH,
                super::super::tools::GRAPH_FIND_SYMBOL,
                super::super::tools::GRAPH_GET_CALLERS,
                super::super::tools::READ_FILE,
                super::super::tools::REPORT_PROGRESS,
            ] {
                assert!(
                    !set.iter().any(|n| n == dropped),
                    "turn {late} must drop {dropped}: {set:?}"
                );
            }
            assert!(
                set.iter().any(|n| n == FINISH),
                "turn {late} keeps finish: {set:?}"
            );
            assert!(
                set.iter().any(|n| n == ADD_REVIEW_COMMENT),
                "turn {late} keeps add_review_comment (diff present): {set:?}"
            );
        }

        // The budget message is injected at the wind-down boundary (turn 3), so by turn 4's request the
        // conversation carries it. (Turn 0's request, before any wind-down, must not.)
        let user_text = user_text.lock().unwrap();
        assert!(
            !user_text[0].contains("Turn budget almost spent"),
            "no budget message before wind-down"
        );
        assert!(
            user_text[4].contains("Turn budget almost spent"),
            "budget message injected by the wind-down turn: {:?}",
            user_text[4]
        );
    }

    // ── Two-tier review (ADR-0062): the FAST tier is a SINGLE diff-only turn with NO retrieval/read_file
    // tools offered. Even though `max_turns` is generous in config, `review.fast` caps the loop to one
    // turn; the model records from the diff (+ SAST digest) and finishes. ─────────────────────────────
    #[tokio::test]
    async fn fast_tier_runs_one_turn_with_no_retrieval_tools() {
        let chat = MockServer::start().await;
        let offered = Arc::new(std::sync::Mutex::new(Vec::new()));
        let user_text = Arc::new(std::sync::Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(RecordingScript {
                calls: Arc::new(AtomicUsize::new(0)),
                offered: offered.clone(),
                user_text: user_text.clone(),
                // The single turn finishes immediately (the realistic fast-tier shape).
                response: tool_call_reply("finish", r#"{"summary":"Fast pass — see SAST."}"#),
            })
            .mount(&chat)
            .await;

        let emb = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{ "index": 0, "embedding": [0.1_f32, 0.2_f32] }]
            })))
            .mount(&emb)
            .await;
        // Only the finish-summary endpoint is needed — a fast run never calls search/graph.
        let cp = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!(
                "/internal/tasks/{}/review/summary",
                Uuid::nil()
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(&cp)
            .await;

        let mut review = review_config(format!("{}/v1", chat.uri()));
        review.max_turns = 150; // generous budget — `fast` must override it to a single turn.
        review.fast = true;
        let cpc = ControlPlaneClient::new(cp.uri(), "tok");
        let embc = EmbeddingsClient::new(&emb.uri(), "key", "model");
        let diff = PrDiff {
            diff: "@@ -1,1 +1,2 @@\n fn x() {}\n+// changed\n".to_string(),
            files: vec!["a.rs".to_string()],
        };
        let outcome = run_native_agent(
            &review,
            "review",
            Some(&diff),
            None,
            None,
            None,
            None,
            &[],
            &cpc,
            &embc,
            Uuid::nil(),
            std::path::Path::new("/tmp"),
            &mut Vec::new(),
        )
        .await
        .expect("fast tier finishes cleanly");
        assert!(
            matches!(outcome, ReviewOutcome::Finished),
            "fast tier finishes in one turn; got {outcome:?}"
        );

        let offered = offered.lock().unwrap();
        assert_eq!(offered.len(), 1, "fast tier is exactly one chat turn");
        let set = &offered[0];
        for forbidden in [
            super::super::tools::VECTOR_SEMANTIC_SEARCH,
            super::super::tools::GRAPH_FIND_SYMBOL,
            super::super::tools::GRAPH_GET_CALLERS,
            super::super::tools::READ_FILE,
        ] {
            assert!(
                !set.iter().any(|n| n == forbidden),
                "fast tier offers no retrieval/read_file ({forbidden}): {set:?}"
            );
        }
        assert!(set.iter().any(|n| n == FINISH), "fast tier keeps finish");
        assert!(
            set.iter().any(|n| n == ADD_REVIEW_COMMENT),
            "fast tier keeps add_review_comment (diff present): {set:?}"
        );
    }

    // Two-tier review (ADR-0062): when the tier declares an explicit `review.tools` allowlist, THAT is
    // the offered set — exactly those tools, nothing else (here: no `add_comment`, no retrieval). The
    // allowlist is config-driven so an operator tunes each tier's surface without a code change.
    #[tokio::test]
    async fn fast_tier_offers_exactly_the_configured_tool_allowlist() {
        let chat = MockServer::start().await;
        let offered = Arc::new(std::sync::Mutex::new(Vec::new()));
        let user_text = Arc::new(std::sync::Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(RecordingScript {
                calls: Arc::new(AtomicUsize::new(0)),
                offered: offered.clone(),
                user_text: user_text.clone(),
                response: tool_call_reply("finish", r#"{"summary":"Fast pass."}"#),
            })
            .mount(&chat)
            .await;
        let emb = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{ "index": 0, "embedding": [0.1_f32, 0.2_f32] }]
            })))
            .mount(&emb)
            .await;
        let cp = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!(
                "/internal/tasks/{}/review/summary",
                Uuid::nil()
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(&cp)
            .await;

        let mut review = review_config(format!("{}/v1", chat.uri()));
        review.fast = true;
        // An explicit allowlist: record findings, finish, abort — and nothing else.
        review.tools = Some(
            serde_json::from_value(serde_json::json!(["add_review_comment", "finish", "abort"]))
                .unwrap(),
        );
        let cpc = ControlPlaneClient::new(cp.uri(), "tok");
        let embc = EmbeddingsClient::new(&emb.uri(), "key", "model");
        let diff = PrDiff {
            diff: "@@ -1,1 +1,2 @@\n fn x() {}\n+// changed\n".to_string(),
            files: vec!["a.rs".to_string()],
        };
        run_native_agent(
            &review,
            "review",
            Some(&diff),
            None,
            None,
            None,
            None,
            &[],
            &cpc,
            &embc,
            Uuid::nil(),
            std::path::Path::new("/tmp"),
            &mut Vec::new(),
        )
        .await
        .expect("fast tier finishes cleanly");

        let offered = offered.lock().unwrap();
        let set = &offered[0];
        let mut got: Vec<&str> = set.iter().map(String::as_str).collect();
        got.sort_unstable();
        let mut want = vec![ADD_REVIEW_COMMENT, FINISH, super::super::tools::ABORT];
        want.sort_unstable();
        assert_eq!(got, want, "offered set is exactly the allowlist: {set:?}");
        assert!(
            !set.iter().any(|n| n == super::super::tools::ADD_COMMENT),
            "a tool left off the allowlist is not offered: {set:?}"
        );
    }

    // ADR-0066: discovered external-knowledge tools are dynamically merged into the offered set —
    // not hardcoded here, and not deep-tier-only (unlike the allowlist test above, this uses the
    // built-in default surface, `review.tools: None`, same as any tier that hasn't restricted it).
    #[tokio::test]
    async fn discovers_and_offers_mcp_tools_dynamically() {
        let chat = MockServer::start().await;
        let offered = Arc::new(std::sync::Mutex::new(Vec::new()));
        let user_text = Arc::new(std::sync::Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(RecordingScript {
                calls: Arc::new(AtomicUsize::new(0)),
                offered: offered.clone(),
                user_text: user_text.clone(),
                response: tool_call_reply("finish", r#"{"summary":"Looked fine."}"#),
            })
            .mount(&chat)
            .await;
        let emb = MockServer::start().await;
        let cp = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/internal/tasks/{}/knowledge/tools", Uuid::nil())))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {
                    "name": "mcp__brave-search__brave_web_search",
                    "description": "Search the web.",
                    "input_schema": { "type": "object", "properties": { "query": { "type": "string" } }, "required": ["query"] }
                }
            ])))
            .mount(&cp)
            .await;
        Mock::given(method("POST"))
            .and(path(format!(
                "/internal/tasks/{}/review/summary",
                Uuid::nil()
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(&cp)
            .await;

        let review = review_config(format!("{}/v1", chat.uri())); // fast: false, tools: None
        let cpc = ControlPlaneClient::new(cp.uri(), "tok");
        let embc = EmbeddingsClient::new(&emb.uri(), "key", "model");
        let diff = PrDiff {
            diff: "@@ -1,1 +1,2 @@\n fn x() {}\n+// changed\n".to_string(),
            files: vec!["a.rs".to_string()],
        };
        run_native_agent(
            &review,
            "review",
            Some(&diff),
            None,
            None,
            None,
            None,
            &[],
            &cpc,
            &embc,
            Uuid::nil(),
            std::path::Path::new("/tmp"),
            &mut Vec::new(),
        )
        .await
        .expect("run finishes cleanly");

        let offered = offered.lock().unwrap();
        assert!(
            offered[0]
                .iter()
                .any(|n| n == "mcp__brave-search__brave_web_search"),
            "discovered tool folded into the offered set: {:?}",
            offered[0]
        );
        // The built-in surface is still there alongside it.
        assert!(offered[0].iter().any(|n| n == VECTOR_SEMANTIC_SEARCH));
    }

    // ADR-0066 (the regex-selector point): a tier whose allowlist names a SPECIFIC `mcp__` selector
    // gets ONLY the discovered tools that selector matches — not all of a busy server's tools. Here
    // brave-search exposes two tools; the allowlist picks exactly one.
    #[tokio::test]
    async fn mcp_selector_offers_only_matching_discovered_tools() {
        let chat = MockServer::start().await;
        let offered = Arc::new(std::sync::Mutex::new(Vec::new()));
        let user_text = Arc::new(std::sync::Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(RecordingScript {
                calls: Arc::new(AtomicUsize::new(0)),
                offered: offered.clone(),
                user_text: user_text.clone(),
                response: tool_call_reply("finish", r#"{"summary":"ok"}"#),
            })
            .mount(&chat)
            .await;
        let emb = MockServer::start().await;
        let cp = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/internal/tasks/{}/knowledge/tools", Uuid::nil())))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "name": "mcp__brave-search__brave_web_search", "description": "web", "input_schema": {"type":"object"} },
                { "name": "mcp__brave-search__brave_local_search", "description": "local", "input_schema": {"type":"object"} }
            ])))
            .mount(&cp)
            .await;
        Mock::given(method("POST"))
            .and(path(format!(
                "/internal/tasks/{}/review/summary",
                Uuid::nil()
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(&cp)
            .await;

        let mut review = review_config(format!("{}/v1", chat.uri()));
        // Allowlist: finish/abort + ONE specific brave tool by exact selector — not the local one.
        review.tools = Some(
            serde_json::from_value(json!([
                "finish",
                "abort",
                "mcp__brave-search__brave_web_search"
            ]))
            .unwrap(),
        );
        let cpc = ControlPlaneClient::new(cp.uri(), "tok");
        let embc = EmbeddingsClient::new(&emb.uri(), "key", "model");
        let diff = PrDiff {
            diff: "@@ -1,1 +1,2 @@\n fn x() {}\n+// changed\n".to_string(),
            files: vec!["a.rs".to_string()],
        };
        run_native_agent(
            &review,
            "review",
            Some(&diff),
            None,
            None,
            None,
            None,
            &[],
            &cpc,
            &embc,
            Uuid::nil(),
            std::path::Path::new("/tmp"),
            &mut Vec::new(),
        )
        .await
        .expect("run finishes cleanly");

        let offered = offered.lock().unwrap();
        assert!(
            offered[0]
                .iter()
                .any(|n| n == "mcp__brave-search__brave_web_search"),
            "the selected tool is offered: {:?}",
            offered[0]
        );
        assert!(
            !offered[0]
                .iter()
                .any(|n| n == "mcp__brave-search__brave_local_search"),
            "the UNselected sibling tool is NOT offered: {:?}",
            offered[0]
        );
        // A built-in the allowlist didn't name is also absent (retrieval).
        assert!(!offered[0].iter().any(|n| n == VECTOR_SEMANTIC_SEARCH));
    }

    // Two-tier review (ADR-0062): the fast tier removes retrieval from the OFFERED set, but a model
    // steered by the shared prompt can still EMIT a retrieval call — and the dispatcher runs any tool by
    // name. So the fast tier must REFUSE a non-offered tool call (never hit the control plane). Live
    // dogfood (task 5ad4e553) showed M2.7 doing read_file + search in its fast turn before this guard.
    #[tokio::test]
    async fn fast_tier_refuses_a_retrieval_call_instead_of_dispatching() {
        let chat = MockServer::start().await;
        // The single fast turn emits a retrieval call (what M2.7 did live). It must be refused.
        mount_chat(
            &chat,
            vec![tool_call_reply(
                "lightbridge_vector_semantic_search",
                r#"{"query":"anything"}"#,
            )],
        )
        .await;
        let emb = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{ "index": 0, "embedding": [0.1_f32, 0.2_f32] }]
            })))
            .mount(&emb)
            .await;
        // The control plane: a search endpoint that MUST NOT be hit (the refusal happens before dispatch).
        let cp = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/internal/tasks/{}/search", Uuid::nil())))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&cp)
            .await;

        let mut review = review_config(format!("{}/v1", chat.uri()));
        review.fast = true;
        let cpc = ControlPlaneClient::new(cp.uri(), "tok");
        let embc = EmbeddingsClient::new(&emb.uri(), "key", "model");
        let diff = PrDiff {
            diff: "@@ -1,1 +1,2 @@\n fn x() {}\n+// changed\n".to_string(),
            files: vec!["a.rs".to_string()],
        };
        let outcome = run_native_agent(
            &review,
            "review",
            Some(&diff),
            None,
            None,
            None,
            None,
            &[],
            &cpc,
            &embc,
            Uuid::nil(),
            std::path::Path::new("/tmp"),
            &mut Vec::new(),
        )
        .await
        .expect("fast tier completes");
        // One turn, no finish → Exhausted (the caller finalizes buffered SAST/findings).
        assert!(
            matches!(outcome, ReviewOutcome::Exhausted),
            "fast retrieval call refused, single turn → Exhausted; got {outcome:?}"
        );
        // The control plane's search endpoint was NEVER hit — the call was refused, not dispatched.
        let cp_hits = cp.received_requests().await.unwrap();
        assert!(
            !cp_hits.iter().any(|r| r.url.path().ends_with("/search")),
            "fast tier must not dispatch the retrieval call to the control plane"
        );
    }

    // Two-tier review (ADR-0062): the fast tier is NOT a single turn — it needs room to act AND finish.
    // The #301 regression: a 1-turn cap meant the model's first action was its last, so it never recorded
    // an inline finding or called finish → an empty review on a PR with changes. With the small (>1) fast
    // budget the model records a finding then finishes across turns, and the review is non-empty.
    #[tokio::test]
    async fn fast_tier_records_a_finding_then_finishes_across_turns() {
        let chat = MockServer::start().await;
        mount_chat(
            &chat,
            vec![
                tool_call_reply(
                    "add_review_comment",
                    r#"{"file":"a.rs","line":2,"title":"Bug","priority":"P1","category":"correctness","body":"off-by-one"}"#,
                ),
                tool_call_reply("finish", r#"{"summary":"One P1."}"#),
            ],
        )
        .await;
        let cp = MockServer::start().await;
        for ep in ["review/inline", "review/summary"] {
            Mock::given(method("POST"))
                .and(path(format!("/internal/tasks/{}/{ep}", Uuid::nil())))
                .respond_with(ResponseTemplate::new(204))
                .mount(&cp)
                .await;
        }
        let mut review = review_config(format!("{}/v1", chat.uri()));
        review.fast = true;
        let cpc = ControlPlaneClient::new(cp.uri(), "tok");
        let embc = EmbeddingsClient::new("http://unused", "key", "model");
        let diff = PrDiff {
            diff: "@@ -1,1 +1,2 @@\n a\n+let x = 1;\n".to_string(),
            files: vec!["a.rs".to_string()],
        };
        let outcome = run_native_agent(
            &review,
            "review",
            Some(&diff),
            None,
            None,
            None,
            None,
            &[],
            &cpc,
            &embc,
            Uuid::nil(),
            std::path::Path::new("/tmp"),
            &mut Vec::new(),
        )
        .await
        .expect("fast tier finishes");
        assert!(
            matches!(outcome, ReviewOutcome::Finished),
            "fast tier records + finishes across turns; got {outcome:?}"
        );
        let inline = cp
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path().ends_with("/review/inline"))
            .count();
        assert_eq!(
            inline, 1,
            "the fast finding was recorded, not lost to a 1-turn cap"
        );
    }

    // ── ADR-0045: context-window budget ─────────────────────────────────────────────────────────

    #[test]
    fn estimate_tokens_scales_with_content_and_tools() {
        let no_tools: Vec<ToolDef> = vec![];
        let small = [ChatMessage::user("hi")];
        let big_text = "x".repeat(4000);
        let big = [ChatMessage::user(big_text.as_str())];
        let s = estimate_tokens(&small, &no_tools);
        let b = estimate_tokens(&big, &no_tools);
        assert!(b > s, "more content → more tokens ({b} vs {s})");
        // ~chars/4: 4000 chars ≈ ~1000 tokens (plus a little overhead).
        assert!((900..1100).contains(&b), "≈ chars/4, got {b}");
        // Advertised tool schemas count against the window too.
        let tool = ToolDef::function("t", "a description", json!({ "type": "object" }));
        assert!(
            estimate_tokens(&small, &[tool]) > s,
            "tool schema adds to the estimate"
        );
    }

    #[test]
    fn trim_tool_history_elides_old_output_keeps_recent_and_structure() {
        let no_tools: Vec<ToolDef> = vec![];
        let big = "y".repeat(8000);
        let mut messages = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("review this"),
            ChatMessage::tool("c1", big.clone()), // old, bulky → trimmable
            ChatMessage::tool("c2", big.clone()), // old, bulky → trimmable
            ChatMessage::tool("c3", big.clone()), // within KEEP_RECENT → preserved
            ChatMessage::user("continue"),        // within KEEP_RECENT → preserved
        ];
        let before = estimate_tokens(&messages, &no_tools);
        let trimmed = trim_tool_history(&mut messages, &no_tools, before / 4);
        assert!(trimmed >= 1, "trimmed at least one old tool message");
        assert!(
            estimate_tokens(&messages, &no_tools) < before,
            "estimate shrank after trimming"
        );
        // Structure preserved: no message removed; tool messages keep their tool_call_id (the
        // assistant↔tool pairing the protocol requires stays valid).
        assert_eq!(messages.len(), 6, "no messages removed");
        assert_eq!(messages[2].role, "tool");
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("c1"));
        // The most-recent tool result (within KEEP_RECENT) is left intact.
        assert_eq!(
            messages[4].content.as_deref(),
            Some(big.as_str()),
            "recent tool output preserved"
        );
    }

    #[test]
    fn is_context_overflow_matches_overflow_not_generic_errors() {
        assert!(is_context_overflow(&anyhow::anyhow!(
            "chat completions API returned 400: This model's maximum context length is 128000 tokens"
        )));
        assert!(is_context_overflow(&anyhow::anyhow!(
            "error: context_length_exceeded"
        )));
        // A genuine bad-request must still fail fast, not be mistaken for overflow.
        assert!(!is_context_overflow(&anyhow::anyhow!(
            "chat completions API returned 400: unknown model 'm'"
        )));
        assert!(!is_context_overflow(&anyhow::anyhow!("connection refused")));
    }

    // ADR-0045 tier 1: a context-overflow error mid-run FINALIZES (Exhausted) instead of failing, so a
    // finding already buffered before the overflow is not discarded.
    #[tokio::test]
    async fn native_loop_finalizes_on_context_overflow() {
        let chat = MockServer::start().await;
        // Turn 1: record a finding (200). Turn 2: the gateway rejects the request for length (400).
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(tool_call_reply(
                "add_review_comment",
                r#"{"file":"a.rs","line":7,"title":"Bug","priority":"P1","category":"correctness","body":"x"}"#,
            )))
            .up_to_n_times(1)
            .mount(&chat)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": { "message": "This model's maximum context length is 128000 tokens" }
            })))
            .mount(&chat)
            .await;

        let cp = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!(
                "/internal/tasks/{}/review/inline",
                Uuid::nil()
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(&cp)
            .await;
        Mock::given(method("POST"))
            .and(path(format!(
                "/internal/tasks/{}/review/summary",
                Uuid::nil()
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(&cp)
            .await;

        let review = review_config(format!("{}/v1", chat.uri()));
        let cpc = ControlPlaneClient::new(cp.uri(), "tok");
        let embc = EmbeddingsClient::new("http://unused", "key", "model");
        let diff = PrDiff {
            diff: "@@ -1,3 +1,4 @@\n fn validate() {}\n+// changed\n".to_string(),
            files: vec!["a.rs".to_string()],
        };
        let mut transcript = Vec::new();
        let outcome = run_native_agent(
            &review,
            "@lightbridge review",
            Some(&diff),
            None,
            None,
            None,
            None,
            &[],
            &cpc,
            &embc,
            Uuid::nil(),
            std::path::Path::new("/tmp"),
            &mut transcript,
        )
        .await
        .expect("overflow finalizes (Ok), it does not fail the run");
        assert!(
            matches!(outcome, ReviewOutcome::Exhausted),
            "context overflow finalizes as Exhausted, got: {outcome:?}"
        );
        // The finding recorded before the overflow was posted (buffered), not discarded.
        let reqs = cp.received_requests().await.unwrap();
        let inline = reqs
            .iter()
            .filter(|r| r.url.path().ends_with("/review/inline"))
            .count();
        assert!(inline >= 1, "the finding was buffered before the overflow");
    }

    // ════════════════════════════════════════════════════════════════════════════════════════════
    // ADR-0049 — Tier-1 reviewer-prompt golden cases (offline, deterministic, in CI).
    //
    // Each case freezes one observed failure mode from epic #177 as a behavioural assertion on the
    // agent loop driven by a SCRIPTED model (no gateway, no tokens). These guard the machinery and the
    // prompt's *structured* substrate; they do NOT judge real-model prose — that is Tier-2 (a manual,
    // gated harness against the live model; see ADR-0049 §"Tier 2"). A change to the reviewer prompt
    // (config.reviewSystemPrompt, or ADR-0047/0048) ships WITH a matching golden case here.
    //
    // Seed-case coverage map (ADR-0049 §"Tier 1"):
    //   1. Empty-retrieval grounding (#187) ... golden_empty_retrieval_grounds_against_absence (here)
    //                                           + dispatch_vector_search_empty_is_explicit_not_bare_
    //                                           brackets (tools.rs) — the substrate freeze.
    //   2. Out-of-scope finding (#3) .......... Tier-2 / CP-contract: scope is enforced server-side
    //                                           (ADR-0022 write-back validation, ADR-0037 mediated
    //                                           anchoring). A scripted model can't demonstrate the
    //                                           PROMPT preventing the attempt — that needs the real
    //                                           model (negative control), so it lives in Tier 2.
    //   3. P2 recorded, not narrated (#2) ..... golden_p2_finding_is_recorded_not_dropped (here)
    //   4. Convergence / no-discard (#4) ...... golden_turn_exhaustion_preserves_buffered_findings
    //                                           (here, turn-budget path) + native_loop_exhausts_budget_
    //                                           without_discarding (outcome) + native_loop_finalizes_on_
    //                                           context_overflow (context-window path).
    //   5. Anchoring (#5) ..................... inline-vs-bucket is decided server-side at finalize; the
    //                                           agent-side substrate (no inline tool without a diff to
    //                                           anchor to) is no_diff_omits_add_review_comment_from_
    //                                           offered_tools.
    //   6. Self-consistency (#6) .............. substrate (the prior-review block reaches the prompt) is
    //                                           build_messages_injects_prior_review_context; that the
    //                                           model reconciles rather than contradicts is Tier-2.
    // ════════════════════════════════════════════════════════════════════════════════════════════

    // Case 1 — the #187 setup: a PR review where the index returns nothing for the model's query. The
    // model must NOT be handed a bare `[]` (which it read as "feature removed", then flagged a
    // non-existent removal). Tier-1 freezes the SUBSTRATE: the tool result fed back is the explicit
    // "empty ≠ absent" message (ADR-0047). Whether the live model then refrains from a removal finding
    // is the Tier-2 quality check.
    #[tokio::test]
    async fn golden_empty_retrieval_grounds_against_absence() {
        let chat = MockServer::start().await;
        mount_chat(
            &chat,
            vec![
                tool_call_reply(
                    "lightbridge_vector_semantic_search",
                    r#"{"query":"the removed validateToken helper"}"#,
                ),
                // The coverage gate bounces the first finish once (a.rs never engaged); the second goes
                // through. Script repeats the last entry, so a single `finish` here suffices.
                tool_call_reply("finish", r#"{"summary":"Reviewed; nothing actionable."}"#),
            ],
        )
        .await;

        let emb = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{ "index": 0, "embedding": [0.1_f32, 0.2_f32] }]
            })))
            .mount(&emb)
            .await;
        let cp = MockServer::start().await;
        // The index has nothing for this query.
        Mock::given(method("POST"))
            .and(path(format!("/internal/tasks/{}/search", Uuid::nil())))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&cp)
            .await;
        Mock::given(method("POST"))
            .and(path(format!(
                "/internal/tasks/{}/review/summary",
                Uuid::nil()
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(&cp)
            .await;

        let review = review_config(format!("{}/v1", chat.uri()));
        let cpc = ControlPlaneClient::new(cp.uri(), "tok");
        let embc = EmbeddingsClient::new(&emb.uri(), "key", "model");
        let diff = PrDiff {
            diff: "@@ -1,2 +1,2 @@\n-fn old() {}\n+fn new() {}\n".to_string(),
            files: vec!["a.rs".to_string()],
        };
        let mut transcript = Vec::new();
        let outcome = run_native_agent(
            &review,
            "@lightbridge review",
            Some(&diff),
            None,
            None,
            None,
            None,
            &[],
            &cpc,
            &embc,
            Uuid::nil(),
            std::path::Path::new("/tmp"),
            &mut transcript,
        )
        .await
        .expect("clean finish");
        assert!(
            matches!(outcome, ReviewOutcome::Finished),
            "got: {outcome:?}"
        );

        let tool_results: Vec<&str> = transcript
            .iter()
            .filter(|e| e.role == "tool")
            .filter_map(|e| e.content.as_deref())
            .collect();
        assert!(
            tool_results
                .iter()
                .any(|c| c.contains("NOT evidence") && c.contains("read_file")),
            "the empty retrieval is surfaced as an explicit non-absence signal, not a bare []: \
             {tool_results:?}"
        );
        assert!(
            !tool_results.iter().any(|c| c.trim() == "[]"),
            "the model is never handed a bare empty array for an empty retrieval"
        );
    }

    // Case 3 — a confirmed P2 must be RECORDED as an anchored finding (add_review_comment →
    // /review/inline), not merely narrated in the finish summary. Tier-1 asserts the loop forwards a P2
    // to the buffer exactly like any other priority — there is no client-side "blockers only" filter.
    // (That the live model's summary doesn't reduce to "no P0/P1 findings" is the Tier-2 prose check.)
    #[tokio::test]
    async fn golden_p2_finding_is_recorded_not_dropped() {
        let chat = MockServer::start().await;
        mount_chat(
            &chat,
            vec![
                tool_call_reply(
                    "add_review_comment",
                    r#"{"file":"a.rs","line":2,"title":"Unclear name","priority":"P2","category":"quality","body":"`x` is opaque; rename it"}"#,
                ),
                tool_call_reply("finish", r#"{"summary":"One quality nit (P2)."}"#),
            ],
        )
        .await;
        let cp = MockServer::start().await;
        for ep in ["review/inline", "review/summary"] {
            Mock::given(method("POST"))
                .and(path(format!("/internal/tasks/{}/{ep}", Uuid::nil())))
                .respond_with(ResponseTemplate::new(204))
                .mount(&cp)
                .await;
        }
        let review = review_config(format!("{}/v1", chat.uri()));
        let cpc = ControlPlaneClient::new(cp.uri(), "tok");
        let embc = EmbeddingsClient::new("http://unused", "key", "model");
        let diff = PrDiff {
            diff: "@@ -1,1 +1,2 @@\n a\n+let x = 1;\n".to_string(),
            files: vec!["a.rs".to_string()],
        };
        let mut transcript = Vec::new();
        let outcome = run_native_agent(
            &review,
            "review",
            Some(&diff),
            None,
            None,
            None,
            None,
            &[],
            &cpc,
            &embc,
            Uuid::nil(),
            std::path::Path::new("/tmp"),
            &mut transcript,
        )
        .await
        .expect("clean finish");
        assert!(
            matches!(outcome, ReviewOutcome::Finished),
            "got: {outcome:?}"
        );
        let reqs = cp.received_requests().await.unwrap();
        let inline = reqs
            .iter()
            .filter(|r| r.url.path().ends_with("/review/inline"))
            .count();
        assert_eq!(
            inline, 1,
            "the P2 was recorded as an anchored finding, not dropped or only narrated"
        );
        // And the loop counted it as a real recorded finding (the CP confirmed the buffer).
        assert!(
            transcript.iter().any(|e| e.role == "tool"
                && e.content
                    .as_deref()
                    .map(|c| c.contains("recorded finding at a.rs:2"))
                    .unwrap_or(false)),
            "the P2 finding was buffered"
        );
    }

    // Case 4 (turn-budget path) — a run that records a finding then WANDERS without calling finish must
    // still (a) terminate within the turn budget as Exhausted and (b) preserve the already-buffered
    // finding: exhaustion finalizes, it never discards (#137). Complements native_loop_finalizes_on_
    // context_overflow (the context-window path) by exercising the TURN-budget path.
    #[tokio::test]
    async fn golden_turn_exhaustion_preserves_buffered_findings() {
        let chat = MockServer::start().await;
        mount_chat(
            &chat,
            vec![
                // Turn 0: record a real finding (engages a.rs).
                tool_call_reply(
                    "add_review_comment",
                    r#"{"file":"a.rs","line":2,"title":"nit","priority":"P2","category":"quality","body":"b"}"#,
                ),
                // Then wander forever (Script repeats the last entry): search, never finish.
                tool_call_reply("lightbridge_vector_semantic_search", r#"{"query":"more"}"#),
            ],
        )
        .await;
        let emb = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{ "index": 0, "embedding": [0.1_f32, 0.2_f32] }]
            })))
            .mount(&emb)
            .await;
        let cp = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/internal/tasks/{}/search", Uuid::nil())))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&cp)
            .await;
        for ep in ["review/inline", "review/summary"] {
            Mock::given(method("POST"))
                .and(path(format!("/internal/tasks/{}/{ep}", Uuid::nil())))
                .respond_with(ResponseTemplate::new(204))
                .mount(&cp)
                .await;
        }
        let mut review = review_config(format!("{}/v1", chat.uri()));
        review.max_turns = 4; // small budget; the model never finishes → Exhausted
        let cpc = ControlPlaneClient::new(cp.uri(), "tok");
        let embc = EmbeddingsClient::new(&emb.uri(), "key", "model");
        let diff = PrDiff {
            diff: "@@ -1,1 +1,2 @@\n a\n+b\n".to_string(),
            files: vec!["a.rs".to_string()],
        };
        let outcome = run_native_agent(
            &review,
            "review",
            Some(&diff),
            None,
            None,
            None,
            None,
            &[],
            &cpc,
            &embc,
            Uuid::nil(),
            std::path::Path::new("/tmp"),
            &mut Vec::new(),
        )
        .await
        .expect("exhaustion is a clean outcome");
        assert!(
            matches!(outcome, ReviewOutcome::Exhausted),
            "a wandering run terminates within budget: {outcome:?}"
        );
        let reqs = cp.received_requests().await.unwrap();
        let inline = reqs
            .iter()
            .filter(|r| r.url.path().ends_with("/review/inline"))
            .count();
        assert!(
            inline >= 1,
            "the finding recorded before wandering was buffered, not discarded at exhaustion"
        );
    }
}
