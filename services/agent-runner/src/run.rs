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

use crate::bootstrap::config::{
    EmbeddingsConfig, ReviewConfig, ReviewConfigs, RunnerConfig, SastConfig,
};
use crate::clone;
use crate::plane::Mode;
use crate::{indexer, review, sast};
use lci_agent_clients::{ControlPlaneClient, EmbeddingsClient};
use lci_agent_status::{Phase, StatusHandle, StatusServerConfig};

/// Do exactly one task and exit — the `run-once` host (ADR-0085).
///
/// `mode`:
/// - `None` — infer index-vs-review from the task's `command`, exactly as the runner always has.
///   This is what the `agent-runner` binary passes, so its behaviour is unchanged.
/// - `Some(Mode::Index | Mode::Review)` — force that mode (the `agent-plane` entrypoint, once the
///   dispatcher passes `--mode`). `Mode::Open` never reaches here: the plane guard rejects it.
///
/// Installs the JSON tracing subscriber (identical filter to the prior `main`), then walks the task
/// lifecycle, returning the process exit code the caller should propagate.
pub async fn run_once(mode: Option<Mode>) -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

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

    // Review is optional (no model → indexing-only). But if it's half-configured, surface it. Two-tier
    // review (ADR-0062): resolve BOTH tiers up front; the runner picks one per task by its tier.
    let review_configs = match ReviewConfig::resolve_tiers(file_config.as_ref()) {
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
    let sast_config = SastConfig::resolve(file_config.as_ref());

    // Race the work against two stop signals; on either we exit promptly WITHOUT reporting a status
    // (the control plane already owns a cancelled row and we must not clobber it with `failed`):
    //  1. SIGTERM — Kubernetes sends it when the reaper deletes the Job. Without this the process
    //     runs until SIGKILL (~30s of wasted work).
    //  2. Upstream cancellation poll — the reaper only SIGTERMs us when it's running; if it's down
    //     (e.g. mid-deploy) a cancelled task's pod would otherwise run to completion. Polling our own
    //     status lets us self-cancel within ~10s regardless of the reaper.
    let outcome = tokio::select! {
        result = run(mode, &config, &client, &embeddings_config, &review_configs, sast_config.as_ref()) => result,
        _ = terminated() => {
            tracing::warn!(task_id = %config.task_id, "received SIGTERM; aborting promptly");
            return std::process::ExitCode::from(143); // 128 + SIGTERM(15)
        }
        _ = cancelled_upstream(&client, config.task_id) => {
            tracing::warn!(task_id = %config.task_id, "task no longer active upstream (cancelled); aborting promptly");
            return std::process::ExitCode::from(143);
        }
    };
    match outcome {
        Ok(RunResult {
            summary,
            review_detail,
        }) => {
            tracing::info!(task_id = %config.task_id, summary, "task succeeded");
            // Carry the review-failure/exhaustion/abort detail (if any) onto the FINAL terminal status,
            // not a mid-run `running` report (#137): the control plane clears `error_detail` on every
            // `running` transition (so retries start clean), which would erase a detail reported there.
            report(&client, &config, "succeeded", review_detail.as_deref()).await;
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
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

    // Live per-review status projection (RFC-0007 slice 5, ADR-0085): flag-gated, default OFF. When
    // `LCI_STATUS_API` is set, the run-once host runs a tiny read-only HTTP server alongside the loop
    // exposing live progress (turn, current tool name, findings so far, tokens, phase, elapsed).
    // Unset ⇒ no handle, no sink wrapping, no server — byte-identical to today's path (prod-neutral,
    // dormant). The status mechanism is host-agnostic; the `serve` HOST topology stays gated on the
    // measurement in #358.
    let status_config = StatusServerConfig::from_env(&config.runner_token);
    let status = status_config
        .as_ref()
        .map(|_| StatusHandle::new(config.task_id));
    // Detached: the server runs until the process exits (the run-once model). Dropping the returned
    // JoinHandle does not abort the task.
    let _status_server = match (status.clone(), status_config) {
        (Some(handle), Some(config)) => Some(lci_agent_status::spawn(handle, config)),
        _ => None,
    };

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

    // Index when this is an `index` task (mode), or a cold repo with no base index yet. A review on an
    // already-indexed repo REUSES the base index (it searches related code via the MCP tools and has
    // the PR diff in its prompt), so we skip the costly full re-index — that re-index was why a review
    // took roughly as long as an index every time (ADR-0025).
    let needs_index = is_index || !context.repo_indexed;
    let (chunk_count, graph_summary) = if needs_index {
        if let Some(status) = &status {
            status.set_phase(Phase::Indexing);
        }
        // ── Semantic index: tree-sitter → pgvector (epic #5, slice 2) ────────────────────────
        let chunks = indexer::index_checkout(&context, &checkout, client, &embedder).await?;
        // ── Structural index: Graphify → Neo4j (epic #5, slice 3, ADR-0019) ──────────────────
        // Best-effort: the semantic index already landed, and the graph store may be unconfigured
        // (control plane returns 503). A graph failure is logged, not fatal — the task still succeeds.
        // ADR-0086 slice 1: `LCI_CODEGRAPH_GRAPH` opts a run into the in-house Rust graph
        // (`lci-codegraph`) instead of Graphify; default (unset) keeps Graphify, so prod is unchanged.
        let graph_result = if indexer::codegraph_graph::enabled() {
            tracing::info!(
                "structural graph: using in-house lci-codegraph (LCI_CODEGRAPH_GRAPH set)"
            );
            indexer::codegraph_graph::index_graph(&context, &checkout, client).await
        } else {
            indexer::graph::index_graph(&context, &checkout, client).await
        };
        let graph = match graph_result {
            Ok((nodes, edges)) => format!("{nodes} nodes / {edges} edges"),
            Err(error) => {
                tracing::warn!(%error, "structural graph indexing failed (non-fatal)");
                "graph skipped".to_string()
            }
        };
        (chunks, graph)
    } else {
        tracing::info!(
            "repo already indexed — reusing the base index (skipping re-index for review)"
        );
        (0, "reused base index".to_string())
    };

    // ── Review: the native agent acts via mediated write tools (default, ADR-0026/0037), then the
    // control plane flushes the buffered findings/replies as one grouped review on finalize.
    // `REVIEW_AGENT=opencode` falls back to the legacy terminal-payload subprocess (retires in #140).
    // Runs only when the LLM is configured; non-fatal (indexing already landed). A standalone `index`
    // task (target_type `repository`, Epic #75) has no PR, so skip review regardless of LLM config.
    // Tracks an optional review-failure/exhaustion/abort detail to attach to the FINAL status (#137).
    let mut review_detail: Option<String> = None;
    // Two-tier review (ADR-0062): pick the per-tier config by the task's tier (`fast` → single diff-only
    // turn, no retrieval; `deep` → full run). An `index` task runs no review. The selected fast config
    // already carries the structural `fast` flag (set in `resolve_tiers`).
    let selected_review = (!is_index)
        .then(|| review_configs.for_tier(&context.tier))
        .flatten();
    let review_summary = match selected_review {
        Some(review) => {
            // Scope to the PR's change set when we can compute it (best-effort; an unavailable base
            // commit just yields an unscoped run).
            let diff = clone::pr_diff(&checkout, &context).await;
            // ── SAST (ADR-0061): a deterministic opengrep pass over the PR's changed files. Its findings
            // are buffered into the SAME review buffer (the control plane scopes + posts them in the one
            // grouped review — no second poster), and a digest is fed to the agent so it doesn't
            // re-report those lines. Opt-in (sast_config is None when disabled) and best-effort: a scan
            // failure is logged, never fatal. Needs the diff to scope to — without it, SAST is skipped.
            let sast_findings = match (sast_config, diff.as_ref()) {
                (Some(cfg), Some(d)) => {
                    if let Some(status) = &status {
                        status.set_phase(Phase::Sast);
                    }
                    match sast::scan(cfg, &checkout, &d.files).await {
                        Ok(findings) => findings,
                        Err(error) => {
                            tracing::warn!(%error, "sast: opengrep scan failed (non-fatal)");
                            Vec::new()
                        }
                    }
                }
                _ => Vec::new(),
            };
            // Buffer before the agent runs so a true (file, line) collision lets the agent's richer
            // finding win the upsert; the digest is what keeps such collisions rare (ADR-0061).
            if !sast_findings.is_empty() {
                sast::buffer(client, config.task_id, &sast_findings).await;
            }
            let sast_digest = sast::digest(&sast_findings);
            // Repo-native agent instructions (ADR-0036): read the repo's AGENTS.md/CLAUDE.md/… and
            // fold them into the prompt as untrusted context so the review respects house rules.
            let repo_instructions = review::instructions::read_agent_instructions(&checkout).await;
            let mut transcript = Vec::new();
            if let Some(status) = &status {
                status.set_phase(Phase::Reviewing);
            }
            let outcome = review::run_native_agent(
                review,
                &context.command,
                diff.as_ref(),
                repo_instructions.as_deref(),
                context.prior_reviews.as_deref(),
                context.repo_memory.as_deref(),
                sast_digest.as_deref(),
                &attribution,
                client,
                &embedder,
                config.task_id,
                &checkout,
                &mut transcript,
                status.as_ref(),
            )
            .await;
            if let Some(status) = &status {
                status.set_phase(Phase::Finalizing);
            }
            // Submit the transcript regardless of outcome (ADR-0034) — a failed run's reasoning is the
            // most useful to inspect. Best-effort: never let it change the task result.
            if !transcript.is_empty()
                && let Err(error) = client.submit_transcript(config.task_id, &transcript).await
            {
                tracing::warn!(%error, "submitting transcript failed (non-fatal)");
            }
            // Net invariant (#137): every review run leaves a VISIBLE artifact unless the gateway was
            // unreachable. We finalize on Finished AND Exhausted AND Aborted — finalize flushes the
            // buffered findings, and its empty-run backstop posts a clean "no issues" review for a PR
            // when the buffer is empty. The old code bailed on exhaustion and dropped the buffer; a real
            // prod run lost 5 findings that way at turn 16. Only a true transport `Err` posts nothing.
            //
            // Finalize failure IS fatal (unlike the rest of review, which is best-effort): the review is
            // ready and the failure is almost always transient (GitHub/network), so the task fails +
            // retries rather than being silently marked succeeded with nothing posted. A retry re-runs
            // the agent from a cleared buffer; the single-artifact case re-posts cleanly, the rare mixed
            // reply+review case may duplicate the part that posted — proper fix is GitHub-side idempotency
            // via posted IDs (ADR-0035).
            match outcome {
                Ok(review::ReviewOutcome::Finished) => {
                    // "finished" is the only outcome the control plane may treat as a provably clean
                    // pass (ADR-0068: zero findings → suppress the post, 👍 only).
                    client.finalize_review(config.task_id, "finished").await?;
                    "review posted".to_string()
                }
                Ok(review::ReviewOutcome::Exhausted) => {
                    if review.fast {
                        // FAST tier (ADR-0062): a fast run that ends without `finish` is normal, not "out
                        // of budget." The quick-pass framing — the 🅵 banner + the "mention @handle for a
                        // deeper review" pointer — is rendered CONTROL-PLANE-SIDE at finalize, where the
                        // real GitHub App handle lives (the runner doesn't have it, and hardcoded the wrong
                        // `@lightbridge` before). So DON'T set a summary here: an exhausted fast pass just
                        // finalizes, and finalize composes the fast body from the task tier + whatever the
                        // run buffered (inline findings still post). A finished fast run is the same — its
                        // `finish` verdict becomes the summary the fast body wraps. The outcome is still
                        // "exhausted" — honest — so a zero-findings exhausted fast pass POSTS its banner
                        // review rather than 👍-ing an incomplete pass as clean (ADR-0068).
                        client.finalize_review(config.task_id, "exhausted").await?;
                        review_detail =
                            Some("fast pass exhausted; framed control-plane-side".to_string());
                        "review posted (fast pass)".to_string()
                    } else {
                        // DEEP tier: the honest truncation note with its real budget.
                        let note = format!(
                            "⚠️ Review hit its step budget ({} turns) — posting the findings gathered so \
                             far; some areas may be unreviewed.",
                            review.max_turns
                        );
                        if let Err(error) = client.set_review_summary(config.task_id, &note).await {
                            tracing::warn!(%error, "setting truncation summary failed (non-fatal)");
                        }
                        client.finalize_review(config.task_id, "exhausted").await?;
                        review_detail = Some(note);
                        "review posted (truncated at step budget)".to_string()
                    }
                }
                Ok(review::ReviewOutcome::Aborted(reason)) => {
                    // The model couldn't complete the review. An aborted run is incomplete and
                    // unverified — its buffered findings never went through the refute pass — so clear
                    // them first and post ONLY the honest note, never half-baked/placeholder findings
                    // (a `placeholder` P1 reached a PR this way — run 7c15f9bb). Best-effort clear.
                    let note = format!("Couldn't complete this review: {reason}");
                    if let Err(error) = client.clear_findings(config.task_id).await {
                        tracing::warn!(%error, "clearing findings on abort failed (non-fatal)");
                    }
                    if let Err(error) = client.set_review_summary(config.task_id, &note).await {
                        tracing::warn!(%error, "setting abort summary failed (non-fatal)");
                    }
                    // "aborted" makes the control plane POST the note (never a silent misleading 👍)
                    // and react 😕 (ADR-0068).
                    client.finalize_review(config.task_id, "aborted").await?;
                    review_detail = Some(note);
                    "review aborted (note posted)".to_string()
                }
                Err(error) => {
                    // A true transport/chat failure — the gateway was unreachable and nothing useful
                    // happened. Stays non-fatal (indexing already landed; nothing is posted), but carry
                    // the reason to the FINAL terminal status (#137) rather than a mid-run `running`
                    // report — the control plane clears `error_detail` on every `running` transition, so
                    // a detail reported there would be erased before a human or retry could see it.
                    let detail = format!("review run failed: {error:#}");
                    tracing::warn!(%detail, "review run failed (non-fatal; nothing posted)");
                    review_detail = Some(detail);
                    "review failed".to_string()
                }
            }
        }
        None => "review disabled".to_string(),
    };

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
