//! Dispatcher role (RFC-0001 Phase 1 + Phase 2 reaper): claim queued tasks, launch one Kubernetes
//! Job per task, and reconcile stuck tasks.
//!
//! The loop drains all currently-due tasks, then blocks until a `LISTEN/NOTIFY` wakeup, the reap
//! tick, or a short poll fallback — the poll covers a missed notification so work is never stranded.
//! Claiming uses `SELECT … FOR UPDATE SKIP LOCKED`, so any number of dispatcher replicas can run
//! concurrently without ever claiming the same task. Loop timings come from the file config (else
//! defaults). The reaper shares this loop (singleton today; idempotent writes keep it correct on N).

use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::PgListener;
use sqlx::PgPool;

use crate::config::ReviewSection;
use crate::db;
use crate::integrations::k8s::TaskLauncher;
use crate::queue::reaper;

/// Defaults for the dispatcher timings when the file config doesn't set them.
const DEFAULT_CLAIM_LEASE_SECS: u64 = 60;
const DEFAULT_POLL_FALLBACK_SECS: u64 = 5;
const DEFAULT_LAUNCH_BACKOFF_SECS: u64 = 30;
const DEFAULT_REAP_INTERVAL_SECS: u64 = 30;
/// Storage GC isn't urgent (it only reclaims space, never affects correctness), so it runs far less
/// often than the reaper — every 10 minutes by default.
const DEFAULT_PRUNE_INTERVAL_SECS: u64 = 600;
/// Outbox retention defaults (ADR-0059): delivered rows go after a week; dead-lettered rows linger a
/// month so a post-mortem can still read them.
const DEFAULT_OUTBOX_POSTED_RETENTION_DAYS: i64 = 7;
const DEFAULT_OUTBOX_FAILED_RETENTION_DAYS: i64 = 30;
/// The data-purge backstop is a rare recovery net (a spawned purge lost to a restart), so it runs on
/// its own slow tick — 10 min — instead of riding the ~30s reaper cadence. Its "which disabled repos
/// still have data?" scan probes `code_chunks` once per ever-disabled repo, which is cheap warm but
/// costs real I/O cold; every-30s was pure waste for a check whose answer is almost always "none".
const DEFAULT_PURGE_RECONCILE_INTERVAL_SECS: u64 = 600;

/// Tunable dispatcher loop timings.
#[derive(Debug, Clone, Copy)]
pub struct DispatcherConfig {
    /// Claim lease before the reaper may reconcile a task (Phase 2). Kept short: it only covers Job
    /// creation; the reaper renews it while the Job is live.
    pub claim_lease: Duration,
    /// Fallback poll cadence in case a `NOTIFY` is missed (e.g. enqueued while we were busy).
    pub poll_fallback: Duration,
    /// Backoff before a task whose Job failed to launch is retried.
    pub launch_backoff: Duration,
    /// How often the reaper reconciles stuck (lease-expired) tasks against their Jobs.
    pub reap_interval: Duration,
    /// How often the index sweeper prunes stale `(repo, commit)` index snapshots (ADR-0052). The
    /// outbox sweeper (ADR-0059) shares this same GC tick.
    pub prune_interval: Duration,
    /// How often the durable data-purge backstop re-checks for `disabled` repos with leftover index
    /// data. A rare recovery net, so it runs on its own slow tick, not the reaper cadence.
    pub purge_reconcile_interval: Duration,
    /// Days a delivered (`posted`) `outbox` row is kept before the outbox sweeper prunes it.
    pub outbox_posted_retention_days: i64,
    /// Days a dead-lettered (`failed`) `outbox` row is kept — longer, for inspection.
    pub outbox_failed_retention_days: i64,
}

impl DispatcherConfig {
    /// Resolve from the file config's `dispatcher` section; each unset (or zero) field uses its
    /// default.
    pub fn from_file(section: Option<&crate::config::DispatcherSection>) -> Self {
        let secs = |value: Option<u64>, default: u64| {
            Duration::from_secs(value.filter(|&s| s > 0).unwrap_or(default))
        };
        // Retention windows are in days; a zero/negative value falls back to the default rather than
        // pruning everything (`interval '0 days'` would delete every terminal row).
        let days = |value: Option<i64>, default: i64| value.filter(|&d| d > 0).unwrap_or(default);
        Self {
            claim_lease: secs(
                section.and_then(|s| s.claim_lease_seconds),
                DEFAULT_CLAIM_LEASE_SECS,
            ),
            poll_fallback: secs(
                section.and_then(|s| s.poll_fallback_seconds),
                DEFAULT_POLL_FALLBACK_SECS,
            ),
            launch_backoff: secs(
                section.and_then(|s| s.launch_backoff_seconds),
                DEFAULT_LAUNCH_BACKOFF_SECS,
            ),
            reap_interval: secs(
                section.and_then(|s| s.reap_interval_seconds),
                DEFAULT_REAP_INTERVAL_SECS,
            ),
            prune_interval: secs(
                section.and_then(|s| s.prune_interval_seconds),
                DEFAULT_PRUNE_INTERVAL_SECS,
            ),
            purge_reconcile_interval: secs(
                section.and_then(|s| s.purge_reconcile_interval_seconds),
                DEFAULT_PURGE_RECONCILE_INTERVAL_SECS,
            ),
            outbox_posted_retention_days: days(
                section.and_then(|s| s.outbox_posted_retention_days),
                DEFAULT_OUTBOX_POSTED_RETENTION_DAYS,
            ),
            outbox_failed_retention_days: days(
                section.and_then(|s| s.outbox_failed_retention_days),
                DEFAULT_OUTBOX_FAILED_RETENTION_DAYS,
            ),
        }
    }
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        Self::from_file(None)
    }
}

/// Run the dispatcher until cancelled. `owner` identifies this replica in the lease (e.g. the pod
/// name) for observability and Phase-2 reaping.
pub async fn run<L: TaskLauncher>(
    pool: PgPool,
    launcher: L,
    owner: String,
    cfg: DispatcherConfig,
    neo4j: Option<std::sync::Arc<neo4rs::Graph>>,
    review: Arc<ReviewSection>,
) -> anyhow::Result<()> {
    let mut listener = PgListener::connect_with(&pool).await?;
    listener.listen(db::TASK_QUEUED_CHANNEL).await?;
    // The reaper shares this loop (the dispatcher is a singleton today); its writes are idempotent
    // and active-status-guarded, so it stays correct even if more than one replica runs it.
    let mut reap_tick = tokio::time::interval(cfg.reap_interval);
    reap_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Index-snapshot GC (ADR-0052) shares this loop, like the reaper — its deletes are idempotent and
    // keep-set-guarded, so it stays correct even if more than one replica runs it.
    let mut prune_tick = tokio::time::interval(cfg.prune_interval);
    prune_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Durable data-purge backstop (Epic #75) on its own slow tick — not the ~30s reaper cadence. Its
    // "disabled repos still holding index data?" scan is idempotent and almost always finds nothing,
    // so re-running it every 30s was steady waste (and I/O-costly when its index pages fell cold).
    // The first `tick()` fires immediately, so a purge lost to a restart is still caught promptly at
    // startup — exactly the case this backstop exists for.
    let mut purge_tick = tokio::time::interval(cfg.purge_reconcile_interval);
    purge_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tracing::info!(owner, "dispatcher started");

    loop {
        drain(&pool, &launcher, &owner, &cfg, &review).await;

        // Wait for an enqueue notification, the reap tick, the poll fallback, or shutdown.
        tokio::select! {
            recv = listener.recv() => {
                if let Err(error) = recv {
                    // The listener connection dropped; log and let the poll cadence drive recovery.
                    tracing::warn!(%error, "notify listener error; falling back to polling");
                    tokio::time::sleep(cfg.poll_fallback).await;
                }
            }
            _ = reap_tick.tick() => {
                if let Err(error) = reaper::reap_once(&pool, &launcher, &review).await {
                    tracing::error!(%error, "reaper cycle failed");
                }
            }
            _ = purge_tick.tick() => {
                // Durable backstop for repo data purge (a spawned purge can be lost on restart). Runs
                // OFF the loop like the prune sweeps: its listing scan is cheap warm but I/O-heavy cold,
                // and it must never add head-of-line latency to task dispatch. Idempotent + status-
                // guarded, so an occasional overlap is harmless.
                let pool = pool.clone();
                let neo4j = neo4j.clone();
                tokio::spawn(async move {
                    crate::queue::lifecycle::reconcile_purges(&pool, neo4j.as_deref()).await;
                });
            }
            _ = prune_tick.tick() => {
                // Storage GC shares this tick. Every sweep runs OFF the loop so a large prune never adds
                // head-of-line latency to task dispatch — each is idempotent, so an occasional overlap
                // (a sweep outliving `prune_interval`) is harmless. `PgPool`/`Arc<Graph>` clone cheaply.
                {
                    // Reap stale `(repo, commit)` index snapshots from pgvector + Neo4j (ADR-0052).
                    let pool = pool.clone();
                    let neo4j = neo4j.clone();
                    tokio::spawn(async move {
                        if let Err(error) = crate::queue::index_sweeper::sweep_once(&pool, neo4j.as_deref()).await {
                            tracing::error!(%error, "index sweeper cycle failed");
                        }
                    });
                }
                {
                    // Prune terminal `outbox` rows past their retention window (ADR-0059) — the
                    // table is append-only otherwise (a 👀 reaction leaves a permanent `posted` row per PR).
                    let pool = pool.clone();
                    let posted_days = cfg.outbox_posted_retention_days;
                    let failed_days = cfg.outbox_failed_retention_days;
                    tokio::spawn(async move {
                        if let Err(error) = crate::queue::outbox_sweeper::sweep_once(&pool, posted_days, failed_days).await {
                            tracing::error!(%error, "outbox sweeper cycle failed");
                        }
                    });
                }
            }
            _ = tokio::time::sleep(cfg.poll_fallback) => {}
            // Graceful shutdown (e.g. a deploy SIGTERMs the pod): stop the loop between iterations so
            // we never die mid-claim/launch leaving a task claimed-but-not-launched. In-flight Jobs
            // keep running independently; the successor's reaper reconciles them.
            _ = shutdown_signal() => {
                tracing::info!(owner, "received shutdown signal; stopping dispatcher loop");
                break;
            }
        }
    }
    Ok(())
}

/// Resolves on SIGTERM (Kubernetes pod termination) or Ctrl-C, for a clean dispatcher shutdown. We
/// run on Linux/macOS; the non-Unix arm falls back to Ctrl-C so the code still compiles.
#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        // Can't install the handler — never resolve (the orchestrator's SIGKILL still stops us).
        Err(error) => {
            tracing::warn!(%error, "could not install SIGTERM handler");
            return std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::warn!(%error, "could not install Ctrl-C handler");
        std::future::pending::<()>().await;
    }
}

/// Claim and dispatch every task that is due right now, then return so the caller can wait.
async fn drain<L: TaskLauncher>(
    pool: &PgPool,
    launcher: &L,
    owner: &str,
    cfg: &DispatcherConfig,
    review: &ReviewSection,
) {
    loop {
        match db::claim_next_task(pool, owner, cfg.claim_lease).await {
            Ok(Some(task)) => dispatch(pool, launcher, &task, cfg, review).await,
            Ok(None) => return,
            Err(error) => {
                tracing::error!(%error, "failed to claim next task");
                return;
            }
        }
    }
}

/// Launch a claimed task's Job and record it; on failure, requeue with backoff so the work is not
/// lost (the claim already moved it out of `queued`).
async fn dispatch<L: TaskLauncher>(
    pool: &PgPool,
    launcher: &L,
    task: &db::ClaimedTask,
    cfg: &DispatcherConfig,
    review: &ReviewSection,
) {
    let started = std::time::Instant::now();
    match launcher.launch(task).await {
        Ok(job_name) => {
            crate::http::metrics::dispatch_outcome("launched");
            crate::http::metrics::dispatch_launch_seconds(started.elapsed().as_secs_f64());
            match db::set_task_job(pool, task.id, &job_name).await {
                Ok(()) => tracing::info!(task_id = %task.id, job_name, "dispatched task to a Job"),
                Err(error) => {
                    // The Job exists but we couldn't record its name; surface it for follow-up
                    // rather than launching a second Job.
                    tracing::error!(%error, task_id = %task.id, job_name, "failed to record job name")
                }
            }
            // ADR-0068: 👀 means "seen AND work started" — enqueue it now the agent Job is launched (the
            // queued→running-and-dispatched transition), not at webhook receipt. Best-effort: a failure
            // here must never fail the dispatch. PR tasks and @mention-triggered tasks react; the target
            // is the @mention comment when mention-triggered, else the PR body.
            react_work_started(pool, task, review).await;
        }
        Err(error) => {
            crate::http::metrics::dispatch_outcome("failed");
            tracing::error!(%error, task_id = %task.id, "failed to launch job; requeueing");
            if let Err(release_error) = db::release_task(pool, task.id, cfg.launch_backoff).await {
                tracing::error!(%release_error, task_id = %task.id, "failed to requeue task");
            }
        }
    }
}

/// Enqueue the 👀 "work started" reaction (ADR-0068) for a just-launched task. It rides the egress
/// outbox (ADR-0059) like every other reaction; the reconciler posts it. Everything here is best-effort
/// — the dispatch already succeeded, so a DB/queue hiccup only means the 👀 is missing, never a failed
/// launch. Who reacts: every PR task, plus any `@mention`-triggered task — a plain-ISSUE mention gets
/// its 👀 on the triggering comment (removing receipt-time 👀 must not leave issue asks
/// unacknowledged). Index tasks (no human audience) never react. Needs owner/repo + the trigger comment
/// id, which the lightweight `ClaimedTask` lacks, so it loads the task context.
async fn react_work_started(pool: &PgPool, task: &db::ClaimedTask, review: &ReviewSection) {
    if !review.reactions_enabled() || task.command_text == "index" {
        return;
    }
    let context = match db::get_task_context(pool, task.id).await {
        Ok(Some(c)) => c,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(%error, task_id = %task.id, "load context for 👀 failed (non-fatal)");
            return;
        }
    };
    if context.target_type != "pull_request" && context.trigger_comment_id.is_none() {
        return; // a non-PR task with no trigger comment has nowhere meaningful to react
    }
    let t = crate::outbox::Target {
        task_id: Some(task.id),
        platform: context.platform,
        installation_id: context.installation_id,
        owner: &context.owner,
        repo: &context.name,
    };
    if let Err(error) = crate::outbox::enqueue_reaction(
        pool,
        &t,
        context.target_id,
        "eyes",
        context.trigger_comment_id,
        &context.target_type,
    )
    .await
    {
        tracing::warn!(%error, task_id = %task.id, "enqueueing 👀 work-started failed (non-fatal)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DispatcherSection;

    // The purge backstop runs on its own slow tick, decoupled from the ~30s reaper cadence: it
    // defaults to 10 min and is independent of `reap_interval`. (Guards the fix for the every-30s
    // slow-statement churn from `list_disabled_repos_needing_purge`.)
    #[test]
    fn purge_reconcile_interval_defaults_to_ten_minutes_independent_of_reaping() {
        let cfg = DispatcherConfig::default();
        assert_eq!(cfg.purge_reconcile_interval, Duration::from_secs(600));
        assert_eq!(cfg.reap_interval, Duration::from_secs(30));
        assert_ne!(
            cfg.purge_reconcile_interval, cfg.reap_interval,
            "the purge backstop must not ride the reaper's fast cadence"
        );
    }

    // The interval is operator-tunable via config (like the other dispatcher timings), and a zero
    // falls back to the default rather than busy-looping the backstop.
    #[test]
    fn purge_reconcile_interval_is_config_overridable_and_zero_falls_back() {
        let overridden = DispatcherConfig::from_file(Some(&DispatcherSection {
            purge_reconcile_interval_seconds: Some(120),
            ..Default::default()
        }));
        assert_eq!(
            overridden.purge_reconcile_interval,
            Duration::from_secs(120)
        );

        let zeroed = DispatcherConfig::from_file(Some(&DispatcherSection {
            purge_reconcile_interval_seconds: Some(0),
            ..Default::default()
        }));
        assert_eq!(
            zeroed.purge_reconcile_interval,
            Duration::from_secs(DEFAULT_PURGE_RECONCILE_INTERVAL_SECS)
        );
    }
}
