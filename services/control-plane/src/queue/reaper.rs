//! Stuck-task reaper (RFC-0001 Phase 2).
//!
//! A task can be stranded in `running` if its Kubernetes Job dies without reporting a terminal status
//! — `DeadlineExceeded` (the Job's `activeDeadlineSeconds` is up to 1h, ADR-0004 + #51), an OOM or
//! node eviction, a crash, or a lost status callback. The reaper makes the queue self-healing: it
//! periodically reconciles each `running` task whose claim lease has expired against its Job's *real*
//! liveness (the Job is the source of truth, not a timer — RFC-0001).
//!
//! Per candidate:
//! - **Active** → renew the lease (the reaper-driven heartbeat) so a legitimately long-running task
//!   is never reclaimed — preserving one-Job-per-task (ADR-0004).
//! - **Succeeded** (Job `Complete` but the task is still `running`) → the success report was lost;
//!   mark the task `succeeded` rather than re-running it (which would re-post the review).
//! - **Failed / Gone** (incl. `job_name` null = a dispatcher died before creating the Job) → delete
//!   the dead Job (its name is derived from the task id, so it must be freed before a retry) and
//!   requeue with backoff while `attempts < MAX_ATTEMPTS`, else mark `failed`.
//!
//! It currently runs inside the dispatcher loop (a singleton today). Every action is an idempotent,
//! `status = 'running'`-guarded write, so it stays correct even if more than one replica runs it.

use std::time::Duration;

use sqlx::PgPool;

use crate::db;
use crate::integrations::k8s::{JobLiveness, TaskLauncher};
use crate::integrations::platform::CheckConclusion;
#[cfg(test)]
use crate::integrations::platform::Platform;

/// How many times a task is retried before the reaper gives up and marks it `failed`. `attempts` is
/// incremented on every claim, so this bounds total dispatch attempts.
pub const MAX_ATTEMPTS: i32 = 5;
/// How long the reaper extends the lease of a Job it confirmed is still alive.
pub const LEASE_RENEWAL: Duration = Duration::from_secs(120);
/// Most candidates reconciled per cycle, so one tick stays cheap on a backlog.
const REAP_BATCH: i64 = 100;
/// Requeue backoff = BASE × 2^(attempts−1), capped — gives a crash-looping task room to settle.
const BACKOFF_BASE: Duration = Duration::from_secs(15);
const BACKOFF_CAP: Duration = Duration::from_secs(600);

/// What to do with one stuck task, decided purely from its Job's liveness and its attempt count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReapAction {
    /// Job is alive — extend the lease, leave the task running.
    RenewLease,
    /// Job finished OK but the report was lost — settle the task as succeeded.
    MarkSucceeded,
    /// Job is dead and retries remain — return to the queue with backoff.
    Requeue,
    /// Job is dead and retries are exhausted — give up.
    Fail,
}

/// The reaper's decision for one task. Pure (no I/O) so the whole policy is unit-tested.
pub fn decide(liveness: JobLiveness, attempts: i32, max_attempts: i32) -> ReapAction {
    match liveness {
        JobLiveness::Active => ReapAction::RenewLease,
        JobLiveness::Succeeded => ReapAction::MarkSucceeded,
        JobLiveness::Failed | JobLiveness::Gone => {
            if attempts < max_attempts {
                ReapAction::Requeue
            } else {
                ReapAction::Fail
            }
        }
    }
}

/// Exponential-with-cap backoff before a requeued task is due again.
fn requeue_backoff(attempts: i32) -> Duration {
    let shift = attempts.clamp(1, 16) as u32 - 1;
    let secs = BACKOFF_BASE.as_secs().saturating_mul(1u64 << shift);
    Duration::from_secs(secs.min(BACKOFF_CAP.as_secs()))
}

/// Reconcile every stuck (`running`, lease-expired) task once against its Job's real state.
pub async fn reap_once<L: TaskLauncher>(
    pool: &PgPool,
    launcher: &L,
    review: &crate::config::ReviewSection,
) -> anyhow::Result<()> {
    let candidates = db::list_reapable_tasks(pool, REAP_BATCH).await?;
    if candidates.is_empty() {
        return Ok(());
    }
    tracing::debug!(count = candidates.len(), "reaper: reconciling stuck tasks");

    for task in candidates {
        let liveness = match &task.job_name {
            // Claimed but no Job was ever recorded — the dispatcher died mid-launch. Treat as gone.
            None => JobLiveness::Gone,
            Some(name) => match launcher.job_liveness(name).await {
                Ok(liveness) => liveness,
                // A transient Kubernetes API error must NOT reclaim a possibly-live task — skip it
                // this cycle and retry next time.
                Err(error) => {
                    tracing::warn!(%error, task_id = %task.id, job_name = name, "reaper: liveness check failed; skipping");
                    continue;
                }
            },
        };

        // A DB error on one task is logged and skipped — it must not abort the whole cycle and
        // starve the other stuck tasks in the batch.
        let result: Result<(), sqlx::Error> = match decide(liveness, task.attempts, MAX_ATTEMPTS) {
            ReapAction::RenewLease => db::renew_lease(pool, task.id, LEASE_RENEWAL).await.map(|_| {
                crate::http::metrics::reap_outcome("renewed");
            }),
            ReapAction::MarkSucceeded => {
                db::set_task_status(pool, task.id, "succeeded", None).await.map(|_| {
                    crate::http::metrics::reap_outcome("succeeded");
                    tracing::info!(task_id = %task.id, "reaper: Job completed but report was lost; marked succeeded");
                })
            }
            ReapAction::Requeue => {
                delete_dead_job(launcher, &task).await;
                db::release_task(pool, task.id, requeue_backoff(task.attempts))
                    .await
                    .map(|requeued| {
                        if requeued {
                            crate::http::metrics::reap_outcome("requeued");
                            tracing::warn!(task_id = %task.id, attempts = task.attempts, "reaper: stuck task requeued");
                        }
                    })
            }
            ReapAction::Fail => {
                delete_dead_job(launcher, &task).await;
                let marked = db::set_task_status(
                    pool,
                    task.id,
                    "failed",
                    Some("Stuck task exhausted retries; failed by the reaper after its Job stopped reporting."),
                )
                .await;
                if marked.is_ok() {
                    crate::http::metrics::reap_outcome("failed");
                    tracing::error!(task_id = %task.id, attempts = task.attempts, "reaper: stuck task exhausted retries; marked failed");
                    // The uncatchable-kill notice (ADR-0057) + the 😕 lifecycle reaction (ADR-0068), both
                    // via the egress outbox (ADR-0059): the keyless dispatcher can't POST, but it CAN
                    // write intent rows — the reconciler delivers them. No sweep, no settle buffer.
                    enqueue_reaper_failure_notice(pool, task.id, review).await;
                }
                marked.map(|_| ())
            }
        };
        if let Err(error) = result {
            tracing::error!(%error, task_id = %task.id, "reaper: DB op failed for this task; continuing");
        }
    }

    // Stop the Jobs of cancelled tasks (e.g. a closed PR, RFC-0001 / webhook lifecycle): delete the
    // Job, then clear `job_name` so it isn't revisited. The control plane that serves webhooks has no
    // Kubernetes client, so cancellation is recorded in the DB and the Job is reaped here.
    for task in db::list_cancelled_with_job(pool, REAP_BATCH).await? {
        let Some(name) = &task.job_name else {
            continue;
        };
        // Only clear `job_name` if the delete actually succeeded — otherwise a transient k8s error
        // would orphan a live Job (the row would no longer be revisited). `delete_job` treats a
        // 404 as success, so an already-gone Job still clears. A real error: leave it for next cycle.
        match launcher.delete_job(name).await {
            Ok(()) => match db::clear_job_name(pool, task.id).await {
                Ok(()) => {
                    crate::http::metrics::reap_outcome("cancelled");
                    tracing::info!(task_id = %task.id, "reaper: stopped cancelled task's Job");
                }
                Err(error) => {
                    tracing::warn!(%error, task_id = %task.id, "reaper: failed to clear job_name after cancel")
                }
            },
            Err(error) => {
                tracing::warn!(%error, task_id = %task.id, job_name = name, "reaper: failed to delete cancelled task's Job; will retry")
            }
        }
    }
    Ok(())
}

/// Best-effort delete of a stuck task's dead Job so its (task-id-derived) name is free for a retry.
/// A failure is logged, not fatal: the reaper retries next cycle, and a lingering Job is harmless.
async fn delete_dead_job<L: TaskLauncher>(launcher: &L, task: &db::ReapableTask) {
    if let Some(name) = &task.job_name
        && let Err(error) = launcher.delete_job(name).await
    {
        tracing::warn!(%error, task_id = %task.id, job_name = name, "reaper: failed to delete dead Job before requeue");
    }
}

/// Enqueue the ADR-0056 failure notice + the 😕 lifecycle reaction (ADR-0068) for a reaper-failed PR
/// task (ADR-0059 egress outbox). A non-PR task (e.g. an index run) has no audience, so it's skipped.
/// Best-effort: any error is logged. The `failure_notice` / `<task>:reaction:confused` dedup_keys mean a
/// task that was *also* runner-reported-failed isn't double-notified or double-reacted — this mirrors
/// serve's `handle_review_failure` for the uncatchable-kill path where no status report ever arrives.
async fn enqueue_reaper_failure_notice(
    pool: &sqlx::PgPool,
    task_id: uuid::Uuid,
    review: &crate::config::ReviewSection,
) {
    let context = match db::get_task_context(pool, task_id).await {
        Ok(Some(c)) if c.target_type == "pull_request" => c,
        _ => return,
    };
    let t = crate::outbox::Target {
        task_id: Some(task_id),
        platform: context.platform,
        installation_id: context.installation_id,
        owner: &context.owner,
        repo: &context.name,
    };
    if review.reactions_enabled() {
        // 😕 on the trigger: the @mention comment when mention-triggered, else the PR body.
        if let Err(error) = crate::outbox::enqueue_reaction(
            pool,
            &t,
            context.target_id,
            "confused",
            context.trigger_comment_id,
            &context.target_type,
        )
        .await
        {
            tracing::warn!(%error, task_id = %task_id, "reaper: enqueueing 😕 failed (non-fatal)");
        }
    }
    if let Err(error) =
        crate::outbox::enqueue_failure_notice(pool, &t, context.target_id, &context.target_type)
            .await
    {
        tracing::warn!(%error, task_id = %task_id, "reaper: enqueueing failure notice failed (non-fatal)");
    }
    // Resolve any check/status opened at dispatch (new feature — cosmetic runner-status reporting).
    // `TimedOut` rather than `Failure`: this path is specifically "the Job stopped reporting", closer
    // to a timeout than a clean failure.
    if let Some(head_sha) = context.head_sha.as_deref()
        && let Err(error) = crate::outbox::enqueue_check_run_resolve(
            pool,
            &t,
            context.target_id,
            head_sha,
            CheckConclusion::TimedOut,
            "Review run did not complete (no report from the runner).",
        )
        .await
    {
        tracing::warn!(%error, task_id = %task_id, "reaper: enqueueing check-run resolve failed (non-fatal)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_job_renews_the_lease() {
        assert_eq!(
            decide(JobLiveness::Active, 1, MAX_ATTEMPTS),
            ReapAction::RenewLease
        );
        // Even past the attempt cap, a live Job is never reclaimed.
        assert_eq!(
            decide(JobLiveness::Active, 99, MAX_ATTEMPTS),
            ReapAction::RenewLease
        );
    }

    #[test]
    fn completed_job_with_lost_report_is_marked_succeeded() {
        assert_eq!(
            decide(JobLiveness::Succeeded, 1, MAX_ATTEMPTS),
            ReapAction::MarkSucceeded
        );
    }

    #[test]
    fn dead_job_requeues_until_attempts_exhausted() {
        for liveness in [JobLiveness::Failed, JobLiveness::Gone] {
            assert_eq!(
                decide(liveness, 1, 5),
                ReapAction::Requeue,
                "early attempts retry"
            );
            assert_eq!(
                decide(liveness, 4, 5),
                ReapAction::Requeue,
                "last retry still requeues"
            );
            assert_eq!(
                decide(liveness, 5, 5),
                ReapAction::Fail,
                "at the cap, give up"
            );
            assert_eq!(
                decide(liveness, 6, 5),
                ReapAction::Fail,
                "past the cap, give up"
            );
        }
    }

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(requeue_backoff(1), Duration::from_secs(15));
        assert_eq!(requeue_backoff(2), Duration::from_secs(30));
        assert_eq!(requeue_backoff(3), Duration::from_secs(60));
        // Caps rather than overflowing for a high attempt count.
        assert_eq!(requeue_backoff(99), BACKOFF_CAP);
    }

    // ── DB integration (needs Postgres via DATABASE_URL; CI runs no Rust test job today) ──────────

    use crate::db::ClaimedTask;
    use std::sync::Mutex;
    use uuid::Uuid;

    /// A launcher whose Job liveness is fixed, recording any deletes — lets the reaper be driven
    /// without a cluster.
    struct FakeLauncher {
        liveness: JobLiveness,
        deleted: Mutex<Vec<String>>,
    }

    impl FakeLauncher {
        fn new(liveness: JobLiveness) -> Self {
            Self {
                liveness,
                deleted: Mutex::new(Vec::new()),
            }
        }
    }

    impl TaskLauncher for FakeLauncher {
        async fn launch(&self, _task: &ClaimedTask) -> anyhow::Result<String> {
            anyhow::bail!("FakeLauncher::launch is not used by the reaper")
        }
        async fn job_liveness(&self, _job_name: &str) -> anyhow::Result<JobLiveness> {
            Ok(self.liveness)
        }
        async fn delete_job(&self, job_name: &str) -> anyhow::Result<()> {
            self.deleted.lock().unwrap().push(job_name.to_string());
            Ok(())
        }
    }

    /// Claim a freshly-created task and then expire its lease + record a Job name, simulating a Job
    /// that was launched and then went quiet. Returns the task id.
    async fn stuck_running_task(pool: &PgPool) -> Uuid {
        let repo_id =
            db::upsert_repository(pool, Platform::GitHub, 1, "octo", "repo", "main", None)
                .await
                .unwrap();
        db::record_delivery(
            pool,
            Platform::GitHub,
            "d1",
            "pull_request",
            &serde_json::json!({}),
        )
        .await
        .unwrap();
        db::create_task(
            pool,
            &db::NewTask {
                model_override: None,
                repository_id: repo_id,
                installation_id: 99,
                webhook_delivery_id: "d1".to_string(),
                target_type: "pull_request".to_string(),
                target_id: 7,
                command_text: "review".to_string(),
                base_sha: None,
                head_sha: Some("head1".to_string()),
                run_epoch: 0,
                preset: "deep".to_string(),
                entry_point: "mention".to_string(),
                trigger_comment_id: None,
                trace_context: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        let claimed = db::claim_next_task(pool, "owner-a", Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap();
        sqlx::query(
            "UPDATE tasks SET job_name = 'job-x', lease_expires_at = now() - interval '5 minutes' \
             WHERE id = $1",
        )
        .bind(claimed.id)
        .execute(pool)
        .await
        .unwrap();
        claimed.id
    }

    async fn status_of(pool: &PgPool, id: Uuid) -> String {
        sqlx::query_scalar("SELECT status FROM tasks WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// A stuck task whose Job has Failed (attempts < MAX) is requeued and its dead Job deleted.
    #[sqlx::test]
    async fn reap_requeues_a_failed_job(pool: PgPool) {
        let id = stuck_running_task(&pool).await;
        let launcher = FakeLauncher::new(JobLiveness::Failed);

        reap_once(&pool, &launcher, &crate::config::ReviewSection::default())
            .await
            .unwrap();

        assert_eq!(status_of(&pool, id).await, "queued", "requeued for retry");
        assert_eq!(
            launcher.deleted.lock().unwrap().as_slice(),
            &["job-x".to_string()],
            "the dead Job is deleted so its name is free"
        );
    }

    /// A still-Active Job is left running with a renewed lease, never reclaimed.
    #[sqlx::test]
    async fn reap_renews_an_active_job(pool: PgPool) {
        let id = stuck_running_task(&pool).await;
        let launcher = FakeLauncher::new(JobLiveness::Active);

        reap_once(&pool, &launcher, &crate::config::ReviewSection::default())
            .await
            .unwrap();

        assert_eq!(status_of(&pool, id).await, "running", "still running");
        assert!(
            launcher.deleted.lock().unwrap().is_empty(),
            "a live Job is not deleted"
        );
        // Lease was pushed into the future, so it's no longer reapable.
        let reapable = db::list_reapable_tasks(&pool, 10).await.unwrap();
        assert!(
            reapable.is_empty(),
            "renewed lease drops it from the candidate set"
        );
    }

    /// A Completed Job whose success report was lost settles the task as succeeded (no re-run).
    #[sqlx::test]
    async fn reap_marks_completed_job_succeeded(pool: PgPool) {
        let id = stuck_running_task(&pool).await;
        let launcher = FakeLauncher::new(JobLiveness::Succeeded);

        reap_once(&pool, &launcher, &crate::config::ReviewSection::default())
            .await
            .unwrap();

        assert_eq!(
            status_of(&pool, id).await,
            "succeeded",
            "lost report settled"
        );
    }
}
