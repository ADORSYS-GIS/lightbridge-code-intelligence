//! The `run-once` host: do one task and exit (ADR-0085 host = `run-once`).
//!
//! This is the orchestration that used to live inline in `main.rs`; it is now a library entrypoint so
//! **both** binaries drive the identical substrate:
//!
//! - `agent-runner` (`main.rs`) calls [`run_once`]`(None)` — today's behaviour, byte-for-byte: the
//!   mode is *inferred* from the fetched task's `command` exactly as before.
//! - `agent-plane` (`bin/agent_plane.rs`) calls [`run_once`]`(Some(mode))` after parsing `--mode` /
//!   `--host` and passing the mode×host guard ([`crate::plane`]).
//!
//! The only behavioural seam the [`Mode`] override touches is the index-vs-review routing decision
//! ([`run`]'s `is_index`): with `mode == None` it collapses to the previous `context.command ==
//! "index"` check, so the deployed path — which passes no flag — is unchanged. Everything else
//! (checkout, indexing, the native review loop, status reporting, self-cancel poll) is verbatim.

use std::path::Path;

use anyhow::Context as _;
use tracing::Instrument;
use uuid::Uuid;

use crate::bootstrap::config::{
    EmbeddingsConfig, ReviewConfig, ReviewConfigs, RunnerConfig, SastConfig, resolve_sast_config,
};
use crate::clone;
use crate::plane::Mode;
use crate::{indexer, review};
use lci_agent_clients::{ControlPlaneClient, EmbeddingsClient, TaskContext};
use lci_agent_status::{Phase, StatusHandle, StatusServerConfig};

/// Do exactly one task and exit — the `run-once` host (ADR-0085).
///
/// `mode`:
/// - `None` — infer index-vs-review from the task's `command`, exactly as the runner always has.
///   This is what the `agent-runner` binary passes, so its behaviour is unchanged.
/// - `Some(Mode::Index | Mode::Review)` — force that mode (the `agent-plane` entrypoint, once the
///   dispatcher passes `--mode`). `Mode::Open` never reaches here: the plane guard rejects it.
///
/// Installs the JSON tracing subscriber (identical filter to the prior `main`) plus, when
/// `OTEL_EXPORTER_OTLP_ENDPOINT` is set, an OTLP→Tempo export layer (ticket #246) — then walks the
/// task lifecycle, returning the process exit code the caller should propagate.
///
/// A thin wrapper around [`run_once_body`]: the crypto-provider pin and tracing/OTel init happen
/// first, `run_once_body` runs entirely inside the `runner.task` root span (re-parented from the
/// Job's `TRACEPARENT` env var when present — the one cross-process trace handoff), and the OTel
/// guard is flushed **unconditionally** afterward. This is a short-lived process (a K8s Job): every
/// exit path — success, failure, SIGTERM, upstream cancellation — must reach the flush, or its
/// still-batched spans are lost. `run_once_body` returning (rather than this function early-returning
/// from inside the body) is what guarantees that; see its own doc comment for the SIGTERM/cancellation
/// detail this fixes.
pub async fn run_once(mode: Option<Mode>) -> std::process::ExitCode {
    lci_observability::install_default_crypto_provider();
    let otel_guard = lci_observability::init_tracing("agent-runner");

    let root_span = tracing::info_span!("runner.task");
    lci_observability::set_remote_parent(&root_span, std::env::var("TRACEPARENT").ok().as_deref());

    let exit_code = run_once_body(mode).instrument(root_span).await;
    otel_guard.shutdown().await;
    exit_code
}

/// The task lifecycle proper — everything [`run_once`] used to do inline. Split out so [`run_once`]
/// can flush the OTel guard unconditionally after this returns, regardless of *which* branch below
/// produced the exit code (previously, the SIGTERM and upstream-cancellation arms of the
/// `tokio::select!` below `return`ed directly from this function, which — had the flush lived here
/// instead of in the caller — would have skipped it on exactly the two paths a forcibly-terminated Job
/// most needs it covered).
async fn run_once_body(mode: Option<Mode>) -> std::process::ExitCode {
    let config = match RunnerConfig::from_env() {
        Ok(config) => config,
        Err(error) => {
            // No task id / callback wiring means we can't even report failure — just exit non-zero
            // so the Job is marked Failed and the dispatcher's reaper (Phase 2) can requeue it.
            tracing::error!(%error, "invalid runner configuration");
            return std::process::ExitCode::FAILURE;
        }
    };
    let client = ControlPlaneClient::new(&config.control_plane_url, &config.runner_token);

    // Optional JSON config file (ConfigMap-mounted); when absent, each config falls back to env. A
    // malformed file is a misconfiguration we surface as a failed task rather than silently ignore.
    let file_config = match crate::bootstrap::config::load_file_config() {
        Ok(file_config) => file_config,
        Err(error) => {
            let detail = error.to_string();
            tracing::error!(%detail, "invalid agent config file");
            report(&client, &config, "failed", Some(&detail)).await;
            return std::process::ExitCode::FAILURE;
        }
    };

    let embeddings_config = match EmbeddingsConfig::resolve(file_config.as_ref()) {
        Ok(cfg) => cfg,
        Err(error) => {
            // The task is already `running` at this point; report failed so the dispatcher
            // doesn't wait for a lease timeout before it can reschedule.
            let detail = error.to_string();
            tracing::error!(%detail, "invalid embeddings configuration");
            report(&client, &config, "failed", Some(&detail)).await;
            return std::process::ExitCode::FAILURE;
        }
    };
    // The embeddings client is built inside `run()` once the task context is known, so it can carry
    // the per-project attribution headers (epic #89).

    // Review is optional (no model → indexing-only). But if it's half-configured, surface it. Named
    // presets (ADR-0103): resolve every configured preset up front; the runner picks one per task by
    // its resolved preset name.
    let review_configs = match ReviewConfig::resolve_presets(file_config.as_ref()) {
        Ok(cfgs) => cfgs,
        Err(error) => {
            let detail = error.to_string();
            tracing::error!(%detail, "invalid review (LLM) configuration");
            report(&client, &config, "failed", Some(&detail)).await;
            return std::process::ExitCode::FAILURE;
        }
    };

    // Deterministic SAST pass (ADR-0061): opt-in, best-effort. Resolve is infallible — a misconfigured
    // block falls back to defaults, and `None` simply means SAST is off — because SAST is an additive
    // signal whose absence must never fail a review.
    let sast_config = resolve_sast_config(file_config.as_ref());

    // Race the work against two stop signals; on either we exit promptly WITHOUT reporting a status
    // (the control plane already owns a cancelled row and we must not clobber it with `failed`):
    //  1. SIGTERM — Kubernetes sends it when the reaper deletes the Job. Without this the process
    //     runs until SIGKILL (~30s of wasted work).
    //  2. Upstream cancellation poll — the reaper only SIGTERMs us when it's running; if it's down
    //     (e.g. mid-deploy) a cancelled task's pod would otherwise run to completion. Polling our own
    //     status lets us self-cancel within ~10s regardless of the reaper.
    // 128 + SIGTERM(15) / a synthetic-but-equivalent exit code for a self-cancel — kept as a value
    // produced here rather than an early `return`, so the caller's OTel flush is never skipped (see
    // `run_once_body`'s doc comment).
    enum RunOnceOutcome {
        Ran(anyhow::Result<RunResult>),
        Terminated,
        CancelledUpstream,
    }
    let outcome = tokio::select! {
        result = run(mode, &config, &client, &embeddings_config, &review_configs, sast_config.as_ref()) => RunOnceOutcome::Ran(result),
        _ = terminated() => {
            tracing::warn!(task_id = %config.task_id, "received SIGTERM; aborting promptly");
            RunOnceOutcome::Terminated
        }
        _ = cancelled_upstream(&client, config.task_id) => {
            tracing::warn!(task_id = %config.task_id, "task no longer active upstream (cancelled); aborting promptly");
            RunOnceOutcome::CancelledUpstream
        }
    };
    match outcome {
        // The control plane already owns a cancelled row in both cases; reporting here would clobber
        // it with a status it doesn't have (matches the pre-existing behavior of the old early-`return`).
        RunOnceOutcome::Terminated | RunOnceOutcome::CancelledUpstream => {
            std::process::ExitCode::from(143)
        }
        RunOnceOutcome::Ran(Ok(RunResult {
            summary,
            review_detail,
        })) => {
            tracing::info!(task_id = %config.task_id, summary, "task succeeded");
            // Carry the review-failure/exhaustion/abort detail (if any) onto the FINAL terminal status,
            // not a mid-run `running` report (#137): the control plane clears `error_detail` on every
            // `running` transition (so retries start clean), which would erase a detail reported there.
            report(&client, &config, "succeeded", review_detail.as_deref()).await;
            std::process::ExitCode::SUCCESS
        }
        RunOnceOutcome::Ran(Err(error)) => {
            let detail = error.to_string();
            tracing::error!(task_id = %config.task_id, error = %detail, "task failed");
            report(&client, &config, "failed", Some(&detail)).await;
            std::process::ExitCode::FAILURE
        }
    }
}

/// Resolves when the process receives SIGTERM (Kubernetes' pod-termination signal). If the signal
/// can't be registered, it never resolves — the task then simply runs to completion. We run on Linux
/// (containers) / macOS (dev); the non-Unix arm falls back to Ctrl-C so the code still compiles.
#[cfg(unix)]
async fn terminated() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut sigterm) => {
            sigterm.recv().await;
        }
        Err(error) => {
            tracing::warn!(%error, "could not install SIGTERM handler; running uninterruptible");
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(not(unix))]
async fn terminated() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::warn!(%error, "could not install Ctrl-C handler; running uninterruptible");
        std::future::pending::<()>().await;
    }
}

/// Resolves once this task is no longer active upstream — e.g. it was cancelled because its PR
/// closed or the repo was removed. The runner polls its own status every 10s so it can stop promptly
/// even when the reaper (which would delete the Job and SIGTERM us) is down. Transient poll errors
/// are ignored — a control-plane blip must not abort a healthy run.
async fn cancelled_upstream(client: &ControlPlaneClient, task_id: uuid::Uuid) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
    tick.tick().await; // the first tick is immediate; skip it so we poll after one interval
    loop {
        tick.tick().await;
        match client.task_status(task_id).await {
            Ok(status) if is_terminal_status(&status) => return,
            Ok(_) => {}
            Err(error) => {
                tracing::debug!(%error, "cancellation poll failed (transient); continuing")
            }
        }
    }
}

/// A status the runner should stop on. While `run()` is in flight the only terminal state we can
/// observe is `cancelled` (we set the others ourselves, at the very end) — so this means "stop now".
fn is_terminal_status(status: &str) -> bool {
    matches!(status, "cancelled" | "failed" | "timed_out" | "succeeded")
}

/// What `run()` returns on success: a human summary, plus an optional review-failure/exhaustion/abort
/// detail to attach to the FINAL terminal status (#137). The review step is non-fatal (indexing already
/// landed), so its failure does NOT make the task `Err` — but the reason is still surfaced on the
/// terminal status rather than dropped or reported via a transient `running` (which the control plane clears).
struct RunResult {
    summary: String,
    review_detail: Option<String>,
}

/// The task lifecycle. Returns a [`RunResult`] on success; any error is reported as `failed`.
///
/// `mode` is the ADR-0085 plane mode when the caller forced one (`agent-plane --mode …`); `None`
/// means "infer from the task", the runner's historical behaviour. It touches exactly one decision —
/// `is_index` below — so a `None` mode leaves the whole path byte-identical to before.
async fn run(
    mode: Option<Mode>,
    config: &RunnerConfig,
    client: &ControlPlaneClient,
    embeddings_config: &EmbeddingsConfig,
    review_configs: &ReviewConfigs,
    sast_config: Option<&SastConfig>,
) -> anyhow::Result<RunResult> {
    // Mark that the runner actually started (the dispatcher already set `running` on claim; this
    // re-affirms it from the pod and is a no-op if already set).
    report(client, config, "running", None).await;

    let (status, _status_server) = start_status_server(config);

    let context = client.get_context(config.task_id).await?;
    tracing::info!(
        repo = format!("{}/{}", context.owner, context.name),
        command = context.command,
        target = format!("{}#{}", context.target_type, context.target_id),
        head_sha = context.head_sha.as_deref().unwrap_or("(default branch)"),
        "fetched task context"
    );

    // ADR-0085 mode selection. The plane mode, when the caller forced one, decides index-vs-review;
    // otherwise we fall back to the historical `command == "index"` inference. This is the ONLY place
    // the mode override changes behaviour, so `agent-runner` (which passes `None`) is unchanged.
    let is_index = match mode {
        Some(Mode::Index) => true,
        Some(Mode::Review) => false,
        // `Mode::Open` (slice 4, ADR-0088) is a *routing*-admitted cell (see `crate::plane`), but its
        // sandboxed host execution + the ticket→prompt pipeline are **dormant** — they land with the
        // (unwired) trigger, gated on a security sign-off. Refuse to run rather than silently degrade
        // to the review/index path: an `open` task must NEVER execute untrusted repo/LLM code outside
        // the hardened sandbox. The loop assembly + mediated egress + sandbox manifest exist as
        // dormant machinery (`lci-open-agent`, `outbox::enqueue_pr_open`, the `command == "open"` Job
        // spec); nothing here drives them yet.
        Some(Mode::Open) => anyhow::bail!(
            "open mode is routing-admitted but its run-once host execution is dormant (ADR-0088): \
             the write-capable loop, mediated PR-open egress, and hardened sandbox manifest exist, \
             but no ticket pipeline/trigger is wired and this host refuses to run open outside the \
             sandbox. Activation is gated on a security sign-off."
        ),
        None => context.command == "index",
    };

    // Gateway attribution headers (epic #89) for per-project token billing — added to the embeddings
    // + review LLM calls. Built here since they come from the fetched task context.
    let attribution = context.attribution_headers();
    let embedder = EmbeddingsClient::new(
        &embeddings_config.base_url,
        &embeddings_config.api_key,
        &embeddings_config.model,
    )
    .with_timeout(std::time::Duration::from_secs(
        embeddings_config.request_timeout_secs,
    ))
    .with_attribution(&attribution);

    let checkout = clone::checkout(&context, &config.workdir).await?;

    let (chunk_count, graph_summary) = perform_indexing(
        is_index,
        &context,
        &checkout,
        client,
        &embedder,
        status.as_ref(),
    )
    .await?;

    // ── Review: the native agent acts via mediated write tools (default, ADR-0026/0037), then the
    // control plane flushes the buffered findings/replies as one grouped review on finalize.
    // `REVIEW_AGENT=opencode` falls back to the legacy terminal-payload subprocess (retires in #140).
    // Runs only when the LLM is configured; non-fatal (indexing already landed). A standalone `index`
    // task (target_type `repository`, Epic #75) has no PR, so skip review regardless of LLM config.
    // Tracks an optional review-failure/exhaustion/abort detail to attach to the FINAL status (#137).
    let (review_summary, review_detail) = perform_review(
        is_index,
        review_configs,
        sast_config,
        &context,
        &checkout,
        client,
        config,
        embeddings_config,
        &attribution,
        config.task_id,
        status.as_ref(),
    )
    .await?;

    if let Some(status) = &status {
        status.set_phase(Phase::Done);
    }

    Ok(RunResult {
        summary: format!(
            "indexed {}/{} at {} — {chunk_count} chunks, {graph_summary}; {review_summary}",
            context.owner,
            context.name,
            context
                .head_sha
                .as_deref()
                .unwrap_or(&context.default_branch),
        ),
        review_detail,
    })
}

/// Live per-review status projection (RFC-0007 slice 5, ADR-0085): flag-gated, default OFF. When
/// `LCI_STATUS_API` is set, the run-once host runs a tiny read-only HTTP server alongside the loop
/// exposing live progress (turn, current tool name, findings so far, tokens, phase, elapsed). Unset ⇒
/// no handle, no sink wrapping, no server — byte-identical to today's path (prod-neutral, dormant). The
/// status mechanism is host-agnostic; the `serve` HOST topology stays gated on the measurement in #358.
///
/// The server is detached: it runs until the process exits (the run-once model). Dropping the returned
/// `JoinHandle` does not abort the task, but the caller keeps it alive for the duration of `run()`
/// anyway (today's exact behaviour).
fn start_status_server(
    config: &RunnerConfig,
) -> (Option<StatusHandle>, Option<tokio::task::JoinHandle<()>>) {
    let status_config = StatusServerConfig::from_env(&config.runner_token);
    let status = status_config
        .as_ref()
        .map(|_| StatusHandle::new(config.task_id));
    let server = match (status.clone(), status_config) {
        (Some(handle), Some(config)) => Some(lci_agent_status::spawn(handle, config)),
        _ => None,
    };
    (status, server)
}

/// Index when this is an `index` task (mode), or a cold repo with no base index yet. A review on an
/// already-indexed repo REUSES the base index (it searches related code via the MCP tools and has the
/// PR diff in its prompt), so we skip the costly full re-index — that re-index was why a review took
/// roughly as long as an index every time (ADR-0025). Returns `(chunk_count, graph_summary)`.
async fn perform_indexing(
    is_index: bool,
    context: &TaskContext,
    checkout: &Path,
    client: &ControlPlaneClient,
    embedder: &EmbeddingsClient,
    status: Option<&StatusHandle>,
) -> anyhow::Result<(usize, String)> {
    let needs_index = is_index || !context.repo_indexed;
    if !needs_index {
        tracing::info!(
            "repo already indexed — reusing the base index (skipping re-index for review)"
        );
        return Ok((0, "reused base index".to_string()));
    }
    if let Some(status) = status {
        status.set_phase(Phase::Indexing);
    }
    // ── Semantic index: tree-sitter → pgvector (epic #5, slice 2) ────────────────────────
    let chunks = indexer::index_checkout(context, checkout, client, embedder).await?;
    // ── Structural index: in-house lci-codegraph → Neo4j (epic #5, slice 3, ADR-0086) ─────
    // The structural graph is built in-process by the `lci-codegraph` crate (tree-sitter); it
    // replaced the retired Python Graphify CLI (ADR-0019) — no flag, no fallback.
    // Best-effort: the semantic index already landed, and the graph store may be unconfigured
    // (control plane returns 503). A graph failure is logged, not fatal — the task still succeeds.
    let graph_result = indexer::graph::index_graph(context, checkout, client).await;
    let graph = match graph_result {
        Ok((nodes, edges)) => format!("{nodes} nodes / {edges} edges"),
        Err(error) => {
            tracing::warn!(%error, "structural graph indexing failed (non-fatal)");
            "graph skipped".to_string()
        }
    };
    Ok((chunks, graph))
}

/// Apply the resolved repo/org model override (ADR-0110, story #501) to a preset-resolved
/// `ReviewConfig`, if any. Overrides `model` only, never tools/gates/budgets — so a repo/org override
/// changes which model runs without touching ADR-0103's "presets never diverge structurally"
/// guarantee. A `None`, empty, or all-whitespace override is treated as no override (the preset's own
/// configured model applies unchanged).
fn apply_model_override(mut review: ReviewConfig, model_override: Option<&str>) -> ReviewConfig {
    if let Some(model) = model_override.map(str::trim).filter(|m| !m.is_empty()) {
        tracing::info!(
            model_override = model,
            previous_model = %review.model,
            "applying repo/org model override (ADR-0110)"
        );
        review.model = model.to_string();
    }
    review
}

/// Run the review step for one task: the native agent (which may call `run_sast`, ADR-0073), then
/// finalize. Returns `(review_summary, review_detail)` — the summary always folds into the top-level
/// task summary; the
/// detail (when `Some`) is the review-failure/exhaustion/abort reason attached to the FINAL terminal
/// status (#137). `Ok(("review disabled", None))` when this is an `index` task or the task's preset has
/// no review model configured (a standalone `index` task — target_type `repository`, Epic #75 — has no
/// PR, so it skips review regardless of LLM config). Named presets (ADR-0103): the config is picked by
/// the task's resolved preset name; an unknown preset name fails the task rather than silently running
/// under another preset.
#[allow(clippy::too_many_arguments)]
async fn perform_review(
    is_index: bool,
    review_configs: &ReviewConfigs,
    // The resolved SAST config (ADR-0073), forwarded to the OpenCode host, which offers the `run_sast`
    // tool inside `lci-review-mcp` when it also clears the diff + per-tier-allowlist gate. This closes
    // the ADR-0097 slice-5 parity gap: SAST used to be native-only, so it surfaced as only-native in the
    // shadow and blocked go-live; `run_sast` now runs on the live OpenCode path.
    sast_config: Option<&SastConfig>,
    context: &TaskContext,
    checkout: &Path,
    client: &ControlPlaneClient,
    config: &RunnerConfig,
    embeddings_config: &EmbeddingsConfig,
    attribution: &[(String, String)],
    task_id: Uuid,
    status: Option<&StatusHandle>,
) -> anyhow::Result<(String, Option<String>)> {
    let resolved = if is_index {
        None
    } else {
        review_configs
            .for_preset(&context.preset)
            .with_context(|| format!("resolving review preset {:?}", context.preset))?
    };
    let Some(review) = resolved else {
        return Ok(("review disabled".to_string(), None));
    };
    // Repo/org model override (ADR-0110, story #501), applied as the FINAL step after `for_preset`
    // resolves the preset's complete base config.
    let review = apply_model_override(review.clone(), context.model_override.as_deref());
    let review = &review;

    // Repo-owned review config (`.lightbridge-code-review.jsonc`, ADR-0030): conventions/architecture/
    // instructions (prompt context), focus/ignore (diff filtering), and severity.min (finding filter,
    // threaded to the MCP subprocess below) all come from this one read.
    let repo_config = review::repo_config::read_repo_review_config(checkout).await;
    let diff_filter = match repo_config.as_ref().map(|c| c.diff_filter(checkout)) {
        Some(Ok(filter)) => filter,
        Some(Err(error)) => {
            tracing::warn!(%error, "repo config focus/ignore globs failed to compile; reviewing unfiltered");
            None
        }
        None => None,
    };
    let repo_config_context = repo_config.as_ref().and_then(|c| c.render_context_block());
    let min_priority = repo_config
        .as_ref()
        .and_then(|c| c.severity)
        .map(|s| s.min.as_str());

    // Scope to the PR's change set when we can compute it (best-effort; an unavailable base commit
    // just yields an unscoped run).
    let diff = clone::pr_diff(checkout, context, diff_filter.as_ref()).await;

    // Repo-native agent instructions (ADR-0036): read the repo's AGENTS.md/CLAUDE.md/… and fold them
    // into the prompt as untrusted context so the review respects house rules.
    let repo_instructions = review::instructions::read_agent_instructions(checkout).await;
    if let Some(status) = status {
        status.set_phase(Phase::Reviewing);
    }
    // ── The review runs on OpenCode (ADR-0097 slice 5 hard cutover) ──────────────────────────────
    // The supervisor spawns `opencode acp`, reuses the tuned coverage/refute gates + mediated tools,
    // and returns the same `ReviewOutcome` the native host did — so `finalize_review_outcome` below is
    // unchanged. `run_native_agent` remains in the tree (retired on this merge, removed in a follow-up).
    let mcp_env = review::opencode::McpEnv {
        control_plane_url: &config.control_plane_url,
        runner_token: &config.runner_token,
        checkout_root: checkout,
        embed_url: &embeddings_config.base_url,
        embed_key: &embeddings_config.api_key,
        embed_model: &embeddings_config.model,
        min_priority,
    };
    let outcome = review::opencode::run_opencode_agent(
        review,
        &context.command,
        diff.as_ref(),
        repo_instructions.as_deref(),
        context.prior_reviews.as_deref(),
        context.repo_memory.as_deref(),
        repo_config_context.as_deref(),
        sast_config,
        attribution,
        &mcp_env,
        task_id,
        // client,  // ← Not used (coverage disclosure no longer posted)
    )
    .await;
    if let Some(status) = status {
        status.set_phase(Phase::Finalizing);
    }

    finalize_review_outcome(outcome, review, &context.entry_point, client, task_id).await
}

/// Map a finished agent run onto a visible PR artifact and the top-level summary/detail (#137). Net
/// invariant: every review run leaves a VISIBLE artifact unless the gateway was unreachable. We
/// finalize on Finished AND Exhausted AND Aborted — finalize flushes the buffered findings, and its
/// empty-run backstop posts a clean "no issues" review for a PR when the buffer is empty. The old code
/// bailed on exhaustion and dropped the buffer; a real prod run lost 5 findings that way at turn 16.
/// Only a true transport `Err` posts nothing.
///
/// Finalize failure IS fatal (unlike the rest of review, which is best-effort): the review is ready and
/// the failure is almost always transient (GitHub/network), so the task fails + retries rather than
/// being silently marked succeeded with nothing posted. A retry re-runs the agent from a cleared
/// buffer; the single-artifact case re-posts cleanly, the rare mixed reply+review case may duplicate
/// the part that posted — proper fix is GitHub-side idempotency via posted IDs (ADR-0035).
async fn finalize_review_outcome(
    outcome: anyhow::Result<review::ReviewOutcome>,
    review: &ReviewConfig,
    entry_point: &str,
    client: &ControlPlaneClient,
    task_id: Uuid,
) -> anyhow::Result<(String, Option<String>)> {
    Ok(match outcome {
        Ok(review::ReviewOutcome::Finished) => {
            // "finished" is the only outcome the control plane may treat as a provably clean pass
            // (ADR-0068: zero findings → suppress the post, 👍 only).
            client.finalize_review(task_id, "finished").await?;
            ("review posted".to_string(), None)
        }
        Ok(review::ReviewOutcome::Exhausted) => {
            // Framing at exhaustion is presentation, not review behavior (ADR-0103 only mandates
            // gate/tool uniformity) — keyed on `entry_point`, NOT preset. A preset name is
            // operator-defined (ADR-0103) and can't be relied on to signal "this was the automatic
            // on-open pass" (story #495 / PR #529 made the same fix in
            // `control-plane/src/http/internal.rs`'s `context.entry_point == "pr_open"` check; this
            // was the one call site it missed).
            if entry_point == "pr_open" {
                // Automatic on-open pass: a run that ends without `finish` is normal, not "out of
                // budget." The quick-pass framing — the 🅵 banner + the "mention @handle for a deeper
                // review" pointer — is rendered CONTROL-PLANE-SIDE at finalize, where the real GitHub
                // App handle lives (the runner doesn't have it, and hardcoded the wrong `@lightbridge`
                // before). So DON'T set a summary here: an exhausted fast pass just finalizes, and
                // finalize composes the fast body from the task's entry point + whatever the run
                // buffered (inline findings still post). A finished fast run is the same — its `finish`
                // verdict becomes the summary the fast body wraps. The outcome is still "exhausted" —
                // honest — so a zero-findings exhausted fast pass POSTS its banner review rather than
                // 👍-ing an incomplete pass as clean (ADR-0068).
                client.finalize_review(task_id, "exhausted").await?;
                (
                    "review posted (fast pass)".to_string(),
                    Some("fast pass exhausted; framed control-plane-side".to_string()),
                )
            } else {
                // Any other entry point (`mention`/`a2a`/…): the honest truncation note with its real
                // budget.
                let note = format!(
                    "⚠️ Review hit its step budget ({} turns) — posting the findings gathered so \
                     far; some areas may be unreviewed.",
                    review.max_turns
                );
                if let Err(error) = client.set_review_summary(task_id, &note).await {
                    tracing::warn!(%error, "setting truncation summary failed (non-fatal)");
                }
                client.finalize_review(task_id, "exhausted").await?;
                (
                    "review posted (truncated at step budget)".to_string(),
                    Some(note),
                )
            }
        }
        Ok(review::ReviewOutcome::Aborted(reason)) => {
            // The model couldn't complete the review. An aborted run is incomplete and unverified —
            // its buffered findings never went through the refute pass — so clear them first and post
            // ONLY the honest note, never half-baked/placeholder findings (a `placeholder` P1 reached a
            // PR this way — run 7c15f9bb). Best-effort clear.
            let note = format!("Couldn't complete this review: {reason}");
            if let Err(error) = client.clear_findings(task_id).await {
                tracing::warn!(%error, "clearing findings on abort failed (non-fatal)");
            }
            if let Err(error) = client.set_review_summary(task_id, &note).await {
                tracing::warn!(%error, "setting abort summary failed (non-fatal)");
            }
            // "aborted" makes the control plane POST the note (never a silent misleading 👍) and react
            // 😕 (ADR-0068).
            client.finalize_review(task_id, "aborted").await?;
            ("review aborted (note posted)".to_string(), Some(note))
        }
        Err(error) => {
            // A true transport/chat failure — the gateway was unreachable and nothing useful happened.
            // Stays non-fatal (indexing already landed; nothing is posted), but carry the reason to the
            // FINAL terminal status (#137) rather than a mid-run `running` report — the control plane
            // clears `error_detail` on every `running` transition, so a detail reported there would be
            // erased before a human or retry could see it.
            let detail = format!("review run failed: {error:#}");
            tracing::warn!(%detail, "review run failed (non-fatal; nothing posted)");
            ("review failed".to_string(), Some(detail))
        }
    })
}

/// Best-effort status report: a failed report must not mask the task's real outcome, so we log and
/// move on rather than propagate (the lease/reaper recovers a task whose final report was lost).
async fn report(
    client: &ControlPlaneClient,
    config: &RunnerConfig,
    status: &str,
    detail: Option<&str>,
) {
    if let Err(error) = client.report_status(config.task_id, status, detail).await {
        tracing::warn!(%error, task_id = %config.task_id, status, "failed to report status");
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::bootstrap::config::ResilienceConfig;

    /// A minimal `ReviewConfig` — only `max_turns` is read by `finalize_review_outcome` (in the
    /// truncation-note branch); the rest are placeholder values a real config would never leave at.
    fn review_config() -> ReviewConfig {
        ReviewConfig {
            base_url: "https://gateway.internal/v1".to_string(),
            api_key: "key".to_string(),
            model: "m".to_string(),
            system_prompt: "You are a reviewer.".to_string(),
            max_diff_chars: 60_000,
            max_turns: 40,
            max_batch_size: 8,
            max_files_read: 30,
            max_searches: 15,
            max_batches: 6,
            max_coverage_bounces: 3,
            max_cycles: 8,
            context_window: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            extra: serde_json::Map::new(),
            stream: false,
            resilience: ResilienceConfig::default(),
            tools: None,
            opencode_overlay: None,
        }
    }

    async fn mock_finalize_and_summary() -> MockServer {
        let cp = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!(
                "/internal/tasks/{}/review/finalize",
                Uuid::nil()
            )))
            .respond_with(ResponseTemplate::new(200))
            .mount(&cp)
            .await;
        Mock::given(method("POST"))
            .and(path(format!(
                "/internal/tasks/{}/review/summary",
                Uuid::nil()
            )))
            .respond_with(ResponseTemplate::new(200))
            .mount(&cp)
            .await;
        cp
    }

    // The whole point of story #495 / PR #529's `internal.rs` fix, mirrored here: preset names are
    // operator-defined (ADR-0103) and can't signal "this was the automatic on-open pass" — only
    // `entry_point` can. A custom-named preset on the `pr_open` entry point must still get the
    // fast-style banner, never the deep-tier truncation note.
    #[tokio::test]
    async fn exhausted_pr_open_entry_point_gets_fast_banner_even_under_a_custom_preset_name() {
        let cp = mock_finalize_and_summary().await;
        let client = ControlPlaneClient::new(cp.uri(), "tok");
        let review = review_config();

        let (message, detail) = finalize_review_outcome(
            Ok(review::ReviewOutcome::Exhausted),
            &review,
            "pr_open",
            &client,
            Uuid::nil(),
        )
        .await
        .expect("finalize_review_outcome");

        assert_eq!(message, "review posted (fast pass)");
        assert_eq!(
            detail.as_deref(),
            Some("fast pass exhausted; framed control-plane-side")
        );
        // The fast path must NOT set a summary — the banner is composed control-plane-side at finalize.
        assert!(
            cp.received_requests()
                .await
                .unwrap()
                .iter()
                .all(|req| !req.url.path().ends_with("/review/summary")),
            "fast pass must not post a review summary"
        );
    }

    // Any other entry point (`mention`/`a2a`/…) keeps the honest truncation-note framing, regardless of
    // what the preset happens to be named.
    #[tokio::test]
    async fn exhausted_non_pr_open_entry_point_gets_truncation_note() {
        let cp = mock_finalize_and_summary().await;
        let client = ControlPlaneClient::new(cp.uri(), "tok");
        let review = review_config();

        let (message, detail) = finalize_review_outcome(
            Ok(review::ReviewOutcome::Exhausted),
            &review,
            "mention",
            &client,
            Uuid::nil(),
        )
        .await
        .expect("finalize_review_outcome");

        assert_eq!(message, "review posted (truncated at step budget)");
        let detail = detail.expect("truncation note");
        assert!(detail.contains("step budget"));
        assert!(detail.contains("40 turns"));
    }

    // ADR-0110 / story #501: `context.model_override` must reach the built `ReviewConfig.model` as
    // the final step, and must never touch any other field (tools/gates/budgets stay preset-defined).
    #[test]
    fn model_override_replaces_the_preset_model_and_nothing_else() {
        let base = review_config();
        let overridden = apply_model_override(base.clone(), Some("claude-opus-5"));
        assert_eq!(overridden.model, "claude-opus-5");
        assert_eq!(overridden.max_turns, base.max_turns);
        assert!(overridden.tools.is_none() && base.tools.is_none());
    }

    #[test]
    fn no_override_leaves_the_preset_model_unchanged() {
        let base = review_config();
        let unchanged = apply_model_override(base.clone(), None);
        assert_eq!(unchanged.model, base.model);
    }

    // An override that's empty or all-whitespace (shouldn't happen from the write-time-validated admin
    // API, but the runner doesn't trust the control plane blindly) is treated as no override.
    #[test]
    fn blank_override_is_treated_as_no_override() {
        let base = review_config();
        assert_eq!(
            apply_model_override(base.clone(), Some("")).model,
            base.model
        );
        assert_eq!(
            apply_model_override(base.clone(), Some("   ")).model,
            base.model
        );
    }
}
