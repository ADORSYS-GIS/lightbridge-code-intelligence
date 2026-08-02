//! The `reconciler` role (ADR-0058) — bidirectional platform reconciliation in one single-replica loop:
//!
//! - **outbound (ADR-0059):** the **sole** platform egress. It drains `outbox` — the intent rows
//!   serve/finalize, the reaper, and the webhook 👀 enqueue — and posts each via the platform
//!   implementation, marking it `posted` (recording the id for the feedback join) or backing it off
//!   on failure. NOTIFY-driven with a timer fallback, exactly like the dispatcher on `task_queued`.
//! - **inbound (ADR-0035):** reads 👍/👎 reactions on the comments we posted and reconciles them into
//!   `review_feedback` (GitHub emits no webhook for reactions).
//!
//! Single replica is load-bearing: it makes "sole consumer" literal and keeps the outbox's per-task
//! ordering intact. The role is the only one besides serve that holds the platform credentials
//! (ADR-0002).
//!
//! Platform dispatch: each outbox row carries a `platform` column; the reconciler looks up the
//! matching `CodePlatform` implementation from a `HashMap` supplied at startup. GitHub rows use the
//! `GithubApp` impl; GitLab rows use the `GitlabClient` impl.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use lci_agent_step::{Passthrough, StepError, StepRuntime};
use lci_agent_types::StepName;
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tracing::Instrument;

use crate::config::ReviewSection;
use crate::integrations::platform::{
    CheckRunResolve, CheckRunStart, CodePlatform, Platform, ReactionTarget, RepoRef,
};

/// How many intents to claim per drain pass.
const DRAIN_BATCH: i64 = 50;
/// Fallback wake if a `NOTIFY outbox` is missed (e.g. fired while we were mid-batch).
const DRAIN_FALLBACK: Duration = Duration::from_secs(15);

/// Run the reconciler: the outbox drain (foreground) plus the feedback poll (spawned). Either failing
/// a cycle is logged and retried — a transient platform/DB blip must not kill the role.
///
/// `platforms` maps each `Platform` variant to its `CodePlatform` implementation. GitHub uses
/// `GithubApp`; GitLab uses `GitlabClient` (ADR-0072).
pub async fn run(
    pool: PgPool,
    platforms: HashMap<Platform, Arc<dyn CodePlatform>>,
    review: Arc<ReviewSection>,
    interval: Duration,
    within_days: i32,
) -> anyhow::Result<()> {
    // Feedback poll (ADR-0035) on its own cadence, alongside the drain.
    {
        let (pool, platforms) = (pool.clone(), platforms.clone());
        let interval_secs = interval.as_secs() as i64;
        tokio::spawn(async move {
            tracing::info!(
                interval_secs,
                within_days,
                "reconciler: feedback poll started"
            );
            let mut tick = tokio::time::interval(interval);
            // A slow cycle (e.g. platform stalling) must not make the next ticks burst-fire to catch up
            // and spike DB + API load — skip the missed ticks instead (gemini #219).
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                match poll_once(&pool, &platforms, within_days, interval_secs).await {
                    Ok(n) => tracing::debug!(comments = n, "feedback poll cycle complete"),
                    Err(error) => tracing::warn!(%error, "feedback poll cycle failed (will retry)"),
                }
            }
        });
    }
    run_outbox_drain(pool, platforms, review).await
}

/// The platform-egress drain loop (ADR-0059): wake on `NOTIFY outbox` (timer fallback), then drain
/// every due intent before sleeping again. If the `LISTEN` connection drops, reconnect a fresh
/// listener (gemini #219) — the timer fallback keeps draining throughout, so a lost connection
/// degrades latency, never liveness.
async fn run_outbox_drain(
    pool: PgPool,
    platforms: HashMap<Platform, Arc<dyn CodePlatform>>,
    review: Arc<ReviewSection>,
) -> anyhow::Result<()> {
    loop {
        let mut listener = match connect_listener(&pool).await {
            Ok(l) => {
                tracing::info!("reconciler: egress drain listening");
                l
            }
            Err(error) => {
                tracing::warn!(%error, "outbox LISTEN connect failed; retrying after fallback");
                tokio::time::sleep(DRAIN_FALLBACK).await;
                continue;
            }
        };
        // Drain + park until the listener drops, then reconnect via the outer loop.
        loop {
            loop {
                match drain_once(&pool, &platforms, &review).await {
                    Ok(0) => break,
                    Ok(n) => tracing::debug!(posted = n, "outbox drain batch"),
                    Err(error) => {
                        tracing::warn!(%error, "outbox drain failed (will retry on next wake)");
                        break;
                    }
                }
            }
            tokio::select! {
                res = listener.recv() => {
                    if let Err(error) = res {
                        tracing::warn!(%error, "outbox LISTEN dropped; reconnecting");
                        break; // → outer loop reconnects a fresh listener
                    }
                }
                _ = tokio::time::sleep(DRAIN_FALLBACK) => {}
            }
        }
    }
}

async fn connect_listener(pool: &PgPool) -> anyhow::Result<PgListener> {
    let mut listener = PgListener::connect_with(pool).await?;
    listener.listen(crate::db::OUTBOX_CHANNEL).await?;
    Ok(listener)
}

/// Claim one batch and deliver each intent. Marks every row `posted` (with the returned id) or backs
/// it off `failed`, so the row never re-claims unbounded — including an auth failure. Returns how
/// many posted.
///
/// Token minting is now encapsulated inside each `CodePlatform` implementation (GitHub mints an
/// installation token internally; GitLab uses a static token), so the per-installation token cache
/// is gone — the implementation owns its own caching if it needs one.
async fn drain_once(
    pool: &PgPool,
    platforms: &HashMap<Platform, Arc<dyn CodePlatform>>,
    review: &ReviewSection,
) -> anyhow::Result<usize> {
    let rows = crate::db::claim_outbox_batch(pool, DRAIN_BATCH).await?;
    if rows.is_empty() {
        return Ok(0);
    }
    let mut posted = 0;
    for row in rows {
        let Some(platform) = platforms.get(&row.platform) else {
            tracing::warn!(
                platform = %row.platform,
                outbox_id = row.id,
                "no platform implementation registered for outbox row; backing off"
            );
            let _ = crate::db::mark_outbox_failed(pool, row.id, "no platform implementation").await;
            continue;
        };
        let repo = RepoRef {
            platform: row.platform,
            full_name: format!("{}/{}", row.owner, row.repo),
            // `platform_repo_id` is not used by any API method (auth uses `installation_id`, URL
            // paths use `owner/repo`), so a placeholder is safe here.
            platform_repo_id: 0,
            installation_id: row.installation_id,
        };
        if row.attempts > 0 {
            tracing::info!(
                outbox_id = row.id,
                attempts = row.attempts,
                kind = %row.kind,
                "outbox: retrying delivery"
            );
        }
        // Ticket #246: the final span of the webhook→task→Job→turns→egress trace. Re-parented from
        // the outbox row's stored `trace_context` (copied from `tasks.trace_context` at enqueue time,
        // see `enqueue_outbox_post`) — `None` for a row not tied to a task, which starts its own
        // independently-sampled root rather than failing.
        let span = tracing::info_span!("egress.deliver", outbox_id = row.id, kind = %row.kind);
        lci_observability::set_remote_parent(&span, row.trace_context.as_deref());
        // ADR-0107: the per-row delivery + mark-posted/mark-failed transition, keyed by the
        // outbox row's own identity. `Passthrough` is the only runtime this role can construct
        // today (promotion to `CheckpointRuntime` stays blocked on #363), so this wrap is a no-op
        // seam — `Passthrough::step` is a bare `f().await` — kept solely to make the transition
        // boundary explicit ahead of that promotion.
        let step_name = StepName::from(format!("outbox:{}", row.id));
        let step_result = Passthrough
            .step(step_name, async || {
                match deliver(pool, platform.as_ref(), &repo, review, &row)
                    .instrument(span)
                    .await
                {
                    Ok(platform_id) => {
                        let outcome = if platform_id.is_some() {
                            "posted"
                        } else {
                            "skipped"
                        };
                        if let Err(error) =
                            crate::db::mark_outbox_posted(pool, row.id, platform_id).await
                        {
                            tracing::warn!(%error, outbox_id = row.id, "marking outbox posted failed");
                        }
                        crate::http::metrics::outbox_delivery(
                            &row.platform.to_string(),
                            &row.kind,
                            outcome,
                        );
                        if platform_id.is_some() {
                            posted += 1;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            outbox_id = row.id,
                            kind = %row.kind,
                            "outbox delivery failed (will back off)"
                        );
                        let _ =
                            crate::db::mark_outbox_failed(pool, row.id, &error.to_string()).await;
                        crate::http::metrics::outbox_delivery(
                            &row.platform.to_string(),
                            &row.kind,
                            "failed",
                        );
                    }
                }
                Ok::<(), StepError>(())
            })
            .await;
        // Should be impossible: `Passthrough::step` never fails on its own, it only returns
        // whatever the closure returns, and the closure above always returns `Ok(())`.
        if let Err(error) = step_result {
            tracing::warn!(
                %error,
                outbox_id = row.id,
                "outbox step wrapper returned an unexpected error"
            );
        }
    }
    Ok(posted)
}

/// Post one intent. Returns the platform id to record (review/comment) or `None`. An `Err` backs the
/// row off for retry. The single posting path the `outbox` drain uses (ADR-0059).
async fn deliver(
    pool: &PgPool,
    platform: &dyn CodePlatform,
    repo: &RepoRef,
    review: &ReviewSection,
    row: &crate::db::OutboxRow,
) -> anyhow::Result<Option<i64>> {
    // Phase A (ADR-0072): extract `target_type` from the payload so GitLab can route to MR notes
    // vs issue notes without probing (MRs and issues share iid sequences — a probe would succeed
    // on the wrong noteable). GitHub ignores this field (same endpoint for PR/issue comments).
    let noteable_type = row.payload.get("target_type").and_then(|v| v.as_str());
    match row.kind.as_str() {
        "reaction" => {
            let content = payload_str(&row.payload, "content")?;
            // ADR-0068: when the payload carries a `comment_id`, react on the triggering @mention
            // comment; otherwise on the PR/issue body (the automatic-review case).
            match row.payload.get("comment_id").and_then(|x| x.as_i64()) {
                Some(comment_id) => {
                    // The parent MR/issue iid — already in the payload as `issue` (the task's
                    // `target_id`). GitLab needs it to address a note through its parent (there is
                    // no global note endpoint); GitHub ignores it (comment IDs are global).
                    let iid = row.payload.get("issue").and_then(|x| x.as_i64());
                    platform
                        .add_reaction(
                            repo,
                            ReactionTarget::Comment { comment_id, iid },
                            content,
                            noteable_type,
                        )
                        .await?;
                }
                None => {
                    let issue = payload_i64(&row.payload, "issue")?;
                    platform
                        .add_reaction(
                            repo,
                            ReactionTarget::Issue { number: issue },
                            content,
                            noteable_type,
                        )
                        .await?;
                }
            }
            Ok(None)
        }
        "reply" => {
            let issue = payload_i64(&row.payload, "issue")?;
            let body = payload_str(&row.payload, "body")?;
            let posted = platform
                .post_comment(repo, issue, body, noteable_type)
                .await?;
            record_comment(pool, row.task_id, posted.id, "reply").await;
            Ok(posted.id)
        }
        "failure_notice" => {
            // Re-check the dedup gate at post time and consume silently if the task already
            // responded — OR is *about to*: a `review`/`reply` intent still pending in the outbox
            // (e.g. one that transiently 502'd and is backing off) means a real review is coming, so
            // don't race a misleading apology ahead of it (#219 review). A dead-lettered (`failed`)
            // review is excluded, so a review that truly can't be delivered still yields a notice.
            if let Some(task) = row.task_id
                && crate::db::has_responded_or_pending_content(pool, task)
                    .await
                    .unwrap_or(false)
            {
                return Ok(None);
            }
            let issue = payload_i64(&row.payload, "issue")?;
            let body = payload_str(&row.payload, "body")?;
            let posted = platform
                .post_comment(repo, issue, body, noteable_type)
                .await?;
            record_comment(pool, row.task_id, posted.id, "failure_notice").await;
            Ok(posted.id)
        }
        "review" => deliver_review(pool, platform, repo, review, row).await,
        "check_run_start" => deliver_check_run_start(pool, platform, repo, row).await,
        "check_run_resolve" => deliver_check_run_resolve(pool, platform, repo, row).await,
        // ADR-0088 open mode: rehydrate + verify the offloaded branch, but the credentialed push +
        // PR-open is **deferred, gated on a security sign-off** — so no `open` task is created in prod
        // and this arm is never reached there. The producer path (offload + dedup'd intent) is real and
        // tested; activating delivery means adding the forge push/open-PR call to `CodePlatform`.
        "pr_open" => deliver_pr_open(pool, row).await,
        other => anyhow::bail!("unknown outbox kind {other:?}"),
    }
}

/// Rehydrate the offloaded open-mode branch (ADR-0088 offload rule) and verify it still hashes to the
/// intent's key, then **refuse to push** — the credentialed egress (branch push + PR open against the
/// forge) is not activated in this slice (it needs a `CodePlatform::open_pull_request` and a security
/// sign-off). No `pr_open` intent is produced in prod (no trigger), so this arm is dormant; if one ever
/// appears it fails loud (backs off) rather than pushing through an unreviewed path. The rehydrate +
/// verify here proves the offload contract end-to-end.
async fn deliver_pr_open(pool: &PgPool, row: &crate::db::OutboxRow) -> anyhow::Result<Option<i64>> {
    use sha2::{Digest, Sha256};
    let payload: crate::outbox::PrOpenPayload = serde_json::from_value(row.payload.clone())?;
    let patch = crate::db::get_pr_open_blob(pool, &payload.content_hash)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "pr_open blob {} was pruned before delivery",
                payload.content_hash
            )
        })?;
    let actual = hex::encode(Sha256::digest(&patch));
    anyhow::ensure!(
        actual == payload.content_hash,
        "pr_open blob hash mismatch (offload corruption): expected {}, got {actual}",
        payload.content_hash
    );
    anyhow::bail!(
        "open-mode pr_open egress is not activated (ADR-0088): the branch rehydrated + verified, but \
         the credentialed push + PR-open is gated on a security sign-off. Branch {:?} ({} patch bytes) \
         NOT pushed.",
        payload.branch,
        patch.len()
    )
}

/// The staleness guard shared by `check_run_start`/`check_run_resolve` (#571): `true` means a NEWER,
/// non-cancelled task already exists for the same PR head SHA, so this delivery must be SKIPPED — see
/// [`crate::db::should_report_check_run`] for why. Fails/defaults to `false` (proceed) when there's no
/// task to check against, or the check itself errored — this is best-effort cosmetic reporting, matching
/// every other non-fatal path in this module.
async fn check_run_task_is_stale(pool: &PgPool, task_id: Option<uuid::Uuid>, kind: &str) -> bool {
    let Some(task) = task_id else {
        return false;
    };
    match crate::db::should_report_check_run(pool, task).await {
        Ok(true) => false,
        Ok(false) => {
            tracing::info!(
                task_id = %task, kind,
                "skipping check-run delivery: a newer task exists for the same PR head SHA (#571)"
            );
            true
        }
        Err(error) => {
            tracing::warn!(%error, task_id = %task, kind, "check-run staleness check failed (non-fatal; proceeding)");
            false
        }
    }
}

/// Open the in-progress check/status (new feature — cosmetic runner-status reporting). On success,
/// persist the platform's id (GitHub only — GitLab/Bitbucket return `None`) onto
/// `tasks.check_run_external_id` so `deliver_check_run_resolve` can address the SAME check run later.
/// That write is best-effort: a failure here just means resolve later falls back to GitHub's
/// self-healing create-completed path — never a hard failure of this delivery (the row is still
/// correctly `posted`).
async fn deliver_check_run_start(
    pool: &PgPool,
    platform: &dyn CodePlatform,
    repo: &RepoRef,
    row: &crate::db::OutboxRow,
) -> anyhow::Result<Option<i64>> {
    if check_run_task_is_stale(pool, row.task_id, "check_run_start").await {
        return Ok(None);
    }
    let p: crate::outbox::CheckRunStartPayload = serde_json::from_value(row.payload.clone())?;
    let external_id = platform
        .start_check_run(
            repo,
            CheckRunStart {
                head_sha: &p.head_sha,
                // No per-task dashboard URL exists today (PR #553 added a Grafana dashboard, not an
                // HTTP-served per-task page) — omit rather than invent a URL scheme.
                details_url: None,
            },
        )
        .await?;
    if let (Some(task), Some(id)) = (row.task_id, external_id)
        && let Err(error) = crate::db::set_check_run_external_id(pool, task, id).await
    {
        tracing::warn!(%error, task_id = %task, "persisting check_run_external_id failed (non-fatal)");
    }
    Ok(external_id)
}

/// Resolve a previously-opened check/status to its outcome (new feature).
async fn deliver_check_run_resolve(
    pool: &PgPool,
    platform: &dyn CodePlatform,
    repo: &RepoRef,
    row: &crate::db::OutboxRow,
) -> anyhow::Result<Option<i64>> {
    if check_run_task_is_stale(pool, row.task_id, "check_run_resolve").await {
        return Ok(None);
    }
    let p: crate::outbox::CheckRunResolvePayload = serde_json::from_value(row.payload.clone())?;
    // Re-read from `tasks` (not the enqueue payload) so a resolve delivered well after start still
    // sees a write that landed in between — the payload only ever carries what was known at enqueue
    // time.
    let external_id = match row.task_id {
        Some(task) => crate::db::get_check_run_external_id(pool, task)
            .await
            .unwrap_or(None),
        None => None,
    };
    // The permalink to the review this run posted, surfaced as the check's "Details" link. Looked up
    // HERE rather than baked into the payload (which is otherwise fully shaped at produce time,
    // ADR-0059) because the URL does not exist yet at finalize: the review is posted later, by the
    // `review` outbox row. That row is enqueued BEFORE this one and the drain is single-replica in
    // `(created_at, id)` order, so `deliver_review`'s `upsert_review` has already stored the URL by
    // the time we get here. `None` when the review was suppressed (clean pass), the platform omitted
    // a URL, or the review delivery is still backing off — the check then simply has no link.
    let details_url = match row.task_id {
        Some(task) => crate::db::get_review_url(pool, task).await.unwrap_or(None),
        None => None,
    };
    platform
        .resolve_check_run(
            repo,
            CheckRunResolve {
                head_sha: &p.head_sha,
                external_id,
                conclusion: p.conclusion,
                title: &p.title,
                summary: &p.summary,
                details_url: details_url.as_deref(),
            },
        )
        .await?;
    Ok(None)
}

/// Post the grouped review and its success side-effects (persist the copy, fetch inline ids, apply
/// outcome labels) — the whole bundle the old synchronous `finalize_review` did, now driven from the
/// pre-shaped payload. The verdict reaction is enqueued separately at finalize (ADR-0068).
async fn deliver_review(
    pool: &PgPool,
    platform: &dyn CodePlatform,
    repo: &RepoRef,
    review: &ReviewSection,
    row: &crate::db::OutboxRow,
) -> anyhow::Result<Option<i64>> {
    let p: crate::outbox::ReviewPayload = serde_json::from_value(row.payload.clone())?;
    let comments: Vec<crate::integrations::platform::InlineComment> = p
        .comments
        .iter()
        .map(|c| crate::integrations::platform::InlineComment {
            path: c.path.clone(),
            line: c.line,
            side: "RIGHT",
            start_line: c.start_line,
            start_side: c.start_line.map(|_| "RIGHT"),
            body: c.body.clone(),
        })
        .collect();
    let review_post = crate::integrations::platform::ReviewPost {
        pr_number: p.pr,
        body: p.body.clone(),
        comments,
        // Labels are applied separately after the review post (see below) so the trait's
        // `post_review` stays a single API call and `add_labels` rides its own best-effort path.
        labels: Vec::new(),
    };
    let posted = platform.post_review(repo, &review_post).await?;
    tracing::info!(
        outbox_id = row.id,
        pr = p.pr,
        inline = p.inline_n,
        "review posted"
    );

    if let Some(task) = row.task_id {
        if let Err(error) = crate::db::upsert_review(
            pool,
            task,
            &p.summary,
            &p.body,
            p.inline_n,
            p.deferred_n,
            p.out_of_scope_n,
            &p.findings_json,
            posted.html_url.as_deref(),
            posted.id,
        )
        .await
        {
            tracing::warn!(%error, task_id = %task, "persisting review copy failed (non-fatal)");
        }
        // Inline comment ids (the create-review response omits them) for the feedback join.
        if let Some(review_id) = posted.id {
            match platform.list_review_comments(repo, p.pr, review_id).await {
                Ok(refs) => {
                    let stored: Vec<crate::db::ReviewCommentRef> = refs
                        .into_iter()
                        .map(|c| crate::db::ReviewCommentRef {
                            platform_comment_id: c.id,
                            kind: "inline".to_string(),
                            file: c.path,
                            line: c.line.map(|l| l as i32),
                        })
                        .collect();
                    if let Err(error) = crate::db::store_review_comments(pool, task, &stored).await
                    {
                        tracing::warn!(%error, task_id = %task, "storing review comment ids failed (non-fatal)");
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, task_id = %task, "fetching review comment ids failed (non-fatal)")
                }
            }
        }
    }

    // Outcome labels (ADR rides the outbox, not a 2nd serve writer) — best-effort.
    let mut labels = Vec::new();
    if let Some(l) = &review.label_reviewed {
        labels.push(l.clone());
    }
    if p.label_findings
        && let Some(l) = &review.label_findings
    {
        labels.push(l.clone());
    }
    if p.label_error
        && let Some(l) = &review.label_error
    {
        labels.push(l.clone());
    }
    if !labels.is_empty()
        && let Err(error) = platform.add_labels(repo, p.pr, &labels).await
    {
        tracing::warn!(%error, pr = p.pr, "applying outcome labels failed (non-fatal)");
    }
    // ADR-0068: the verdict reaction (👎 findings / 👍 clean) is a separate `reaction` intent
    // enqueued at finalize — a `review` intent is only ever produced when there ARE findings, so the
    // old unconditional 🎉 here is gone.
    Ok(posted.id)
}

/// Record a posted comment's id so the feedback poll can read its reactions (ADR-0035). Best-effort;
/// a missing id or store error just means that comment's reactions go unread.
async fn record_comment(pool: &PgPool, task_id: Option<uuid::Uuid>, id: Option<i64>, kind: &str) {
    let (Some(task), Some(cid)) = (task_id, id) else {
        return;
    };
    if let Err(error) = crate::db::store_review_comments(
        pool,
        task,
        &[crate::db::ReviewCommentRef {
            platform_comment_id: cid,
            kind: kind.to_string(),
            file: None,
            line: None,
        }],
    )
    .await
    {
        tracing::warn!(%error, task_id = %task, kind, "storing posted comment id failed (non-fatal)");
    }
}

fn payload_i64(v: &serde_json::Value, key: &str) -> anyhow::Result<i64> {
    v.get(key)
        .and_then(|x| x.as_i64())
        .ok_or_else(|| anyhow::anyhow!("outbox payload missing i64 {key:?}"))
}

fn payload_str<'a>(v: &'a serde_json::Value, key: &str) -> anyhow::Result<&'a str> {
    v.get(key)
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow::anyhow!("outbox payload missing str {key:?}"))
}

/// One feedback poll cycle: for each comment due this cycle (age-tiered), read its reactions and
/// reconcile. Returns the number checked. Looks up the platform implementation per comment.
async fn poll_once(
    pool: &PgPool,
    platforms: &HashMap<Platform, Arc<dyn CodePlatform>>,
    within_days: i32,
    interval_secs: i64,
) -> anyhow::Result<usize> {
    let comments = crate::db::list_pollable_comments(pool, within_days, interval_secs).await?;
    let mut checked = 0;
    for c in &comments {
        let Some(platform) = platforms.get(&c.platform) else {
            tracing::warn!(
                platform = %c.platform,
                comment = c.platform_comment_id,
                "no platform implementation for pollable comment; skipping"
            );
            continue;
        };
        let repo = RepoRef {
            platform: c.platform,
            full_name: format!("{}/{}", c.owner, c.name),
            platform_repo_id: 0,
            installation_id: c.installation_id,
        };
        let is_review_comment = c.kind == "inline";
        match platform
            .list_comment_reactions(
                &repo,
                c.platform_comment_id,
                is_review_comment,
                Some(c.target_id),
                Some(&c.target_type),
            )
            .await
        {
            Ok(reactions) => {
                // `reconcile_comment_feedback` expects `&[(reactor_login, reaction_content)]`.
                let pairs: Vec<(String, String)> = reactions
                    .into_iter()
                    .map(|r| (r.user_login, r.content))
                    .collect();
                match crate::db::reconcile_comment_feedback(
                    pool,
                    c.task_id,
                    c.platform_comment_id,
                    &c.kind,
                    &pairs,
                )
                .await
                {
                    Ok(_) => {
                        checked += 1;
                    }
                    Err(error) => {
                        tracing::warn!(%error, comment = c.platform_comment_id, "reconciling feedback failed");
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, comment = c.platform_comment_id, "reading reactions failed")
            }
        }
    }
    Ok(checked)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0107: `drain_once`'s per-row delivery block now runs through
    /// `Passthrough.step(StepName::from(format!("outbox:{}", row.id)), ...)`. This proves that
    /// wrap is behavior-neutral for the exact shape used there — a closure that mutably captures
    /// the outer `posted` counter and conditionally increments it based on an `Option<i64>`
    /// "platform id" — by checking `Passthrough::step` (a bare `f().await`, see
    /// `services/agent-step/src/lib.rs`) neither swallows, duplicates, nor reorders that mutation
    /// relative to calling the same closure body directly with no step wrap at all.
    #[tokio::test]
    async fn outbox_step_wrap_preserves_posted_increment_on_delivery() {
        let outbox_id = 42_i64;
        let step_name = StepName::from(format!("outbox:{outbox_id}"));

        let mut posted_via_step = 0;
        Passthrough
            .step(step_name, async || {
                let platform_id: Option<i64> = Some(7);
                if platform_id.is_some() {
                    posted_via_step += 1;
                }
                Ok::<(), StepError>(())
            })
            .await
            .unwrap();

        let mut posted_unwrapped = 0;
        {
            let platform_id: Option<i64> = Some(7);
            if platform_id.is_some() {
                posted_unwrapped += 1;
            }
        }

        assert_eq!(
            posted_via_step, posted_unwrapped,
            "Passthrough.step must not change how many times/whether the closure's side effect runs"
        );
        assert_eq!(posted_via_step, 1);
    }

    /// The `deliver` "skipped" branch (e.g. a `reaction` intent, which never returns a platform
    /// id) must not increment `posted` — proven through the step wrap the same way.
    #[tokio::test]
    async fn outbox_step_wrap_does_not_increment_posted_on_skip() {
        let step_name = StepName::from(format!("outbox:{}", 7_i64));
        let mut posted = 0;
        Passthrough
            .step(step_name, async || {
                let platform_id: Option<i64> = None;
                if platform_id.is_some() {
                    posted += 1;
                }
                Ok::<(), StepError>(())
            })
            .await
            .unwrap();
        assert_eq!(posted, 0);
    }

    /// The `deliver` failure branch never touches `posted` either way; the step wrap must
    /// propagate that unchanged (no increment, no swallowed error since the closure itself
    /// always resolves `Ok(())` — the failure is handled *inside* the closure, mirroring
    /// `drain_once`, and only surfaces through `mark_outbox_failed`/metrics, not the step's own
    /// `Result`).
    #[tokio::test]
    async fn outbox_step_wrap_does_not_increment_posted_on_delivery_failure() {
        let step_name = StepName::from(format!("outbox:{}", 9_i64));
        let mut posted = 0;
        let result = Passthrough
            .step(step_name, async || {
                // Mirrors the `Err(error)` arm of `drain_once`: `mark_outbox_failed` runs,
                // `posted` is never touched, and the step call itself still resolves `Ok(())`.
                let delivery_succeeded = false;
                if delivery_succeeded {
                    posted += 1;
                }
                Ok::<(), StepError>(())
            })
            .await;
        assert!(result.is_ok());
        assert_eq!(posted, 0);
    }

    #[test]
    fn outbox_step_name_is_keyed_by_row_id() {
        let name = StepName::from(format!("outbox:{}", 123_i64));
        assert_eq!(name.as_str(), "outbox:123");
    }

    /// A minimal `CodePlatform` test double that only implements `start_check_run`/`resolve_check_run`
    /// meaningfully (recording calls); every other method is unreachable from the two `deliver` arms
    /// under test here. Mirrors `reaper.rs`'s `FakeLauncher` pattern — a hand-rolled stub for a trait
    /// this crate has no mock-generation macro for.
    #[derive(Default)]
    struct FakePlatform {
        start_calls: std::sync::Mutex<Vec<String>>,
        resolve_calls:
            std::sync::Mutex<Vec<(String, crate::integrations::platform::CheckConclusion)>>,
        start_returns: Option<i64>,
    }

    #[async_trait::async_trait]
    impl CodePlatform for FakePlatform {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn verify_webhook(&self, _headers: &axum::http::HeaderMap, _body: &[u8]) -> bool {
            unimplemented!("not exercised by the check-run delivery tests")
        }
        fn delivery_id(&self, _headers: &axum::http::HeaderMap) -> Option<String> {
            unimplemented!("not exercised by the check-run delivery tests")
        }
        fn event_type(&self, _headers: &axum::http::HeaderMap) -> Option<String> {
            unimplemented!("not exercised by the check-run delivery tests")
        }
        async fn list_changed_files(
            &self,
            _repo: &RepoRef,
            _pr_number: i64,
        ) -> anyhow::Result<Vec<crate::integrations::platform::ChangedFile>> {
            unimplemented!("not exercised by the check-run delivery tests")
        }
        async fn default_branch(&self, _repo: &RepoRef) -> anyhow::Result<String> {
            unimplemented!("not exercised by the check-run delivery tests")
        }
        async fn pr_shas(
            &self,
            _repo: &RepoRef,
            _pr_number: i64,
        ) -> anyhow::Result<(Option<String>, Option<String>)> {
            unimplemented!("not exercised by the check-run delivery tests")
        }
        async fn get_repo_file(
            &self,
            _repo: &RepoRef,
            _ref_: &str,
            _path: &str,
        ) -> anyhow::Result<Option<String>> {
            unimplemented!("not exercised by the check-run delivery tests")
        }
        async fn update_repo_file(
            &self,
            _repo: &RepoRef,
            _path: &str,
            _mutate: Box<dyn FnOnce(Option<String>) -> String + Send>,
            _message: &str,
        ) -> anyhow::Result<()> {
            unimplemented!("not exercised by the check-run delivery tests")
        }
        async fn post_review(
            &self,
            _repo: &RepoRef,
            _review: &crate::integrations::platform::ReviewPost,
        ) -> anyhow::Result<crate::integrations::platform::PostedReview> {
            unimplemented!("not exercised by the check-run delivery tests")
        }
        async fn post_comment(
            &self,
            _repo: &RepoRef,
            _issue_number: i64,
            _body: &str,
            _noteable_type: Option<&str>,
        ) -> anyhow::Result<crate::integrations::platform::PostedComment> {
            unimplemented!("not exercised by the check-run delivery tests")
        }
        async fn add_reaction(
            &self,
            _repo: &RepoRef,
            _target: ReactionTarget,
            _emoji: &str,
            _noteable_type: Option<&str>,
        ) -> anyhow::Result<()> {
            unimplemented!("not exercised by the check-run delivery tests")
        }
        async fn add_labels(
            &self,
            _repo: &RepoRef,
            _issue_number: i64,
            _labels: &[String],
        ) -> anyhow::Result<()> {
            unimplemented!("not exercised by the check-run delivery tests")
        }
        async fn start_check_run(
            &self,
            _repo: &RepoRef,
            req: CheckRunStart<'_>,
        ) -> anyhow::Result<Option<i64>> {
            self.start_calls
                .lock()
                .unwrap()
                .push(req.head_sha.to_string());
            Ok(self.start_returns)
        }
        async fn resolve_check_run(
            &self,
            _repo: &RepoRef,
            req: CheckRunResolve<'_>,
        ) -> anyhow::Result<()> {
            self.resolve_calls
                .lock()
                .unwrap()
                .push((req.head_sha.to_string(), req.conclusion));
            Ok(())
        }
        async fn list_review_comments(
            &self,
            _repo: &RepoRef,
            _pr_number: i64,
            _review_id: i64,
        ) -> anyhow::Result<Vec<crate::integrations::platform::ReviewCommentRef>> {
            unimplemented!("not exercised by the check-run delivery tests")
        }
        async fn list_comment_reactions(
            &self,
            _repo: &RepoRef,
            _comment_id: i64,
            _is_review_comment: bool,
            _iid: Option<i64>,
            _noteable_type: Option<&str>,
        ) -> anyhow::Result<Vec<crate::integrations::platform::Reaction>> {
            unimplemented!("not exercised by the check-run delivery tests")
        }
        fn clone_url(&self, _repo: &RepoRef) -> String {
            unimplemented!("not exercised by the check-run delivery tests")
        }
    }

    /// Seed the FK rows a task needs and create one PR-review task on `head_sha`, returning its id.
    /// A minimal local mirror of `db::tests::seed`/`pr_task` (that module is private to `db`), scoped
    /// to what the check-run staleness-guard tests below need.
    async fn seed_task(pool: &PgPool, head_sha: &str, command_text: &str) -> uuid::Uuid {
        let repo_id =
            crate::db::upsert_repository(pool, Platform::GitHub, 1, "octo", "repo", "main", None)
                .await
                .unwrap();
        crate::db::record_delivery(
            pool,
            Platform::GitHub,
            "d1",
            "pull_request",
            &serde_json::json!({}),
        )
        .await
        .ok(); // idempotent — a second seed on the same pool just dedupes on `delivery_id`
        crate::db::create_explicit_task(
            pool,
            &crate::db::NewTask {
                repository_id: repo_id,
                installation_id: 1,
                webhook_delivery_id: "d1".to_string(),
                target_type: "pull_request".to_string(),
                target_id: 7,
                command_text: command_text.to_string(),
                base_sha: Some("base".to_string()),
                head_sha: Some(head_sha.to_string()),
                run_epoch: 0,
                preset: "fast".to_string(),
                entry_point: "pr_open".to_string(),
                trigger_comment_id: None,
                trace_context: None,
                model_override: None,
                check_runs_enabled: true,
                run_after_secs: None,
            },
        )
        .await
        .unwrap()
    }

    fn fake_repo() -> RepoRef {
        RepoRef {
            platform: Platform::GitHub,
            full_name: "octo/repo".to_string(),
            platform_repo_id: 0,
            installation_id: 1,
        }
    }

    /// A `PgPool` that never actually connects — valid for these tests because both delivery
    /// functions only touch the pool when `row.task_id` is `Some` (the persistence/read-back
    /// branches), and these tests use `task_id: None` to isolate the routing/parsing logic from the
    /// DB-backed persistence, which is covered separately by
    /// `db::tests::check_run_external_id_round_trips_and_defaults_to_none`.
    fn unconnected_pool() -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .expect("lazy pool construction never touches the network")
    }

    fn check_run_row(kind: &str, payload: serde_json::Value) -> crate::db::OutboxRow {
        check_run_row_for_task(None, kind, payload)
    }

    fn check_run_row_for_task(
        task_id: Option<uuid::Uuid>,
        kind: &str,
        payload: serde_json::Value,
    ) -> crate::db::OutboxRow {
        crate::db::OutboxRow {
            id: 1,
            task_id,
            installation_id: 1,
            owner: "octo".to_string(),
            repo: "repo".to_string(),
            kind: kind.to_string(),
            payload,
            attempts: 0,
            platform: Platform::GitHub,
            trace_context: None,
        }
    }

    #[tokio::test]
    async fn deliver_routes_check_run_start_to_the_platform_and_returns_its_id() {
        let pool = unconnected_pool();
        let platform = FakePlatform {
            start_returns: Some(999),
            ..Default::default()
        };
        let row = check_run_row(
            "check_run_start",
            serde_json::json!({ "pr": 7, "head_sha": "abc123" }),
        );
        let repo = fake_repo();
        let result = deliver_check_run_start(&pool, &platform, &repo, &row)
            .await
            .expect("delivery succeeds");
        assert_eq!(result, Some(999));
        assert_eq!(platform.start_calls.lock().unwrap().as_slice(), ["abc123"]);
    }

    #[tokio::test]
    async fn deliver_routes_check_run_resolve_to_the_platform_with_no_stored_id() {
        let pool = unconnected_pool();
        let platform = FakePlatform::default();
        let row = check_run_row(
            "check_run_resolve",
            serde_json::json!({
                "pr": 7,
                "head_sha": "abc123",
                "conclusion": "neutral",
                "summary": "found 2 things",
            }),
        );
        let repo = fake_repo();
        let result = deliver_check_run_resolve(&pool, &platform, &repo, &row)
            .await
            .expect("delivery succeeds");
        // `check_run_resolve` never carries a platform id to `mark_outbox_posted` — unlike `review`,
        // it isn't itself the thing being correlated back.
        assert_eq!(result, None);
        let calls = platform.resolve_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "abc123");
        assert_eq!(
            calls[0].1,
            crate::integrations::platform::CheckConclusion::Neutral
        );
    }

    /// #571: an OLD task's `check_run_resolve` must be a no-op once a NEWER task shares the same PR
    /// head SHA — otherwise a stale/failed run can clobber a fresh success (or vice versa) purely
    /// because its delivery landed later. This is the exact scenario observed live on PR #561: a
    /// re-review superseded an earlier run, but the earlier run's dead-lettered resolve still had a
    /// chance to post after the newer one had already completed.
    #[sqlx::test]
    async fn deliver_check_run_resolve_skips_a_task_superseded_by_a_newer_run(pool: PgPool) {
        let older = seed_task(&pool, "same-sha", "review").await;
        sqlx::query("UPDATE tasks SET created_at = now() - interval '1 hour' WHERE id = $1")
            .bind(older)
            .execute(&pool)
            .await
            .unwrap();
        seed_task(&pool, "same-sha", "@lightbridge-assistant review").await;

        let platform = FakePlatform::default();
        let row = check_run_row_for_task(
            Some(older),
            "check_run_resolve",
            serde_json::json!({
                "pr": 7,
                "head_sha": "same-sha",
                "conclusion": "failure",
                "summary": "an old, superseded run",
            }),
        );
        let repo = fake_repo();
        let result = deliver_check_run_resolve(&pool, &platform, &repo, &row)
            .await
            .expect("delivery is a no-op, not an error");
        assert_eq!(result, None);
        assert!(
            platform.resolve_calls.lock().unwrap().is_empty(),
            "the stale task's resolve must never reach the platform"
        );
    }

    /// The mirror image: the NEWEST task for a head SHA is always current and must still post
    /// normally — the guard only skips tasks a newer run has superseded.
    #[sqlx::test]
    async fn deliver_check_run_resolve_still_posts_for_the_latest_task(pool: PgPool) {
        seed_task(&pool, "same-sha", "review").await;
        let newer = seed_task(&pool, "same-sha", "@lightbridge-assistant review").await;

        let platform = FakePlatform::default();
        let row = check_run_row_for_task(
            Some(newer),
            "check_run_resolve",
            serde_json::json!({
                "pr": 7,
                "head_sha": "same-sha",
                "conclusion": "success",
                "summary": "the current run",
            }),
        );
        let repo = fake_repo();
        deliver_check_run_resolve(&pool, &platform, &repo, &row)
            .await
            .expect("delivery succeeds");
        let calls = platform.resolve_calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            1,
            "the latest task's resolve must reach the platform"
        );
        assert_eq!(
            calls[0].1,
            crate::integrations::platform::CheckConclusion::Success
        );
    }

    /// The same staleness guard applies to `check_run_start` (#571): an old task's delayed start must
    /// not re-open (or, on GitLab/Bitbucket's upsert-by-sha status API, regress to "pending") a check
    /// a newer run has already claimed.
    #[sqlx::test]
    async fn deliver_check_run_start_skips_a_task_superseded_by_a_newer_run(pool: PgPool) {
        let older = seed_task(&pool, "same-sha", "review").await;
        sqlx::query("UPDATE tasks SET created_at = now() - interval '1 hour' WHERE id = $1")
            .bind(older)
            .execute(&pool)
            .await
            .unwrap();
        seed_task(&pool, "same-sha", "@lightbridge-assistant review").await;

        let platform = FakePlatform {
            start_returns: Some(999),
            ..Default::default()
        };
        let row = check_run_row_for_task(
            Some(older),
            "check_run_start",
            serde_json::json!({ "pr": 7, "head_sha": "same-sha" }),
        );
        let repo = fake_repo();
        let result = deliver_check_run_start(&pool, &platform, &repo, &row)
            .await
            .expect("delivery is a no-op, not an error");
        assert_eq!(result, None);
        assert!(
            platform.start_calls.lock().unwrap().is_empty(),
            "the stale task's start must never reach the platform"
        );
        assert_eq!(
            crate::db::get_check_run_external_id(&pool, older)
                .await
                .unwrap(),
            None,
            "a skipped start must not persist a check_run_external_id"
        );
    }
}
