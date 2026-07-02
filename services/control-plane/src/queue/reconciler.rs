//! The `reconciler` role (ADR-0058) — bidirectional GitHub reconciliation in one single-replica loop:
//!
//! - **outbound (ADR-0059):** the **sole** GitHub egress. It drains `github_outbox` — the intent rows
//!   serve/finalize, the reaper, and the webhook 👀 enqueue — and posts each via the App key, marking it
//!   `posted` (recording the id for the feedback join) or backing it off on failure. NOTIFY-driven with a
//!   timer fallback, exactly like the dispatcher on `task_queued`.
//! - **inbound (ADR-0035):** reads 👍/👎 reactions on the comments we posted and reconciles them into
//!   `review_feedback` (GitHub emits no webhook for reactions).
//!
//! Single replica is load-bearing: it makes "sole consumer" literal and keeps the outbox's per-task
//! ordering intact. The role is the only one besides serve that holds the App key (ADR-0002).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::PgListener;
use sqlx::PgPool;

use crate::config::ReviewSection;
use crate::integrations::github::{GithubApp, ReviewComment};

/// How many intents to claim per drain pass.
const DRAIN_BATCH: i64 = 50;
/// Fallback wake if a `NOTIFY github_outbox` is missed (e.g. fired while we were mid-batch).
const DRAIN_FALLBACK: Duration = Duration::from_secs(15);

/// Run the reconciler: the outbox drain (foreground) plus the feedback poll (spawned). Either failing a
/// cycle is logged and retried — a transient GitHub/DB blip must not kill the role.
pub async fn run(
    pool: PgPool,
    app: GithubApp,
    review: Arc<ReviewSection>,
    interval: Duration,
    within_days: i32,
) -> anyhow::Result<()> {
    // Feedback poll (ADR-0035) on its own cadence, alongside the drain.
    {
        let (pool, app) = (pool.clone(), app.clone());
        let interval_secs = interval.as_secs() as i64;
        tokio::spawn(async move {
            tracing::info!(
                interval_secs,
                within_days,
                "reconciler: feedback poll started"
            );
            let mut tick = tokio::time::interval(interval);
            // A slow cycle (e.g. GitHub stalling) must not make the next ticks burst-fire to catch up and
            // spike DB + API load — skip the missed ticks instead (gemini #219).
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                match poll_once(&pool, &app, within_days, interval_secs).await {
                    Ok(n) => tracing::debug!(comments = n, "feedback poll cycle complete"),
                    Err(error) => tracing::warn!(%error, "feedback poll cycle failed (will retry)"),
                }
            }
        });
    }
    run_outbox_drain(pool, app, review).await
}

/// The GitHub-egress drain loop (ADR-0059): wake on `NOTIFY github_outbox` (timer fallback), then drain
/// every due intent before sleeping again. If the `LISTEN` connection drops, reconnect a fresh listener
/// (gemini #219) — the timer fallback keeps draining throughout, so a lost connection degrades latency,
/// never liveness.
async fn run_outbox_drain(
    pool: PgPool,
    app: GithubApp,
    review: Arc<ReviewSection>,
) -> anyhow::Result<()> {
    loop {
        let mut listener = match connect_listener(&pool).await {
            Ok(l) => {
                tracing::info!("reconciler: github-egress drain listening");
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
                match drain_once(&pool, &app, &review).await {
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
    listener.listen(crate::db::GITHUB_OUTBOX_CHANNEL).await?;
    Ok(listener)
}

/// Claim one batch and deliver each intent. Marks every row `posted` (with the returned id) or backs it
/// off `failed`, so the row never re-claims unbounded — including a token-mint failure. Returns how many
/// posted.
async fn drain_once(
    pool: &PgPool,
    app: &GithubApp,
    review: &ReviewSection,
) -> anyhow::Result<usize> {
    let rows = crate::db::claim_outbox_batch(pool, DRAIN_BATCH).await?;
    if rows.is_empty() {
        return Ok(0);
    }
    // One token per installation per batch; `None` caches a mint failure.
    let mut tokens: HashMap<i64, Option<String>> = HashMap::new();
    let mut posted = 0;
    for row in rows {
        let token = match tokens.get(&row.installation_id) {
            Some(Some(t)) => t.clone(),
            Some(None) => {
                // Mint already failed this batch — back the row off rather than spin on it.
                let _ = crate::db::mark_outbox_failed(pool, row.id, "mint token failed").await;
                continue;
            }
            None => match app.installation_token(row.installation_id).await {
                Ok(t) => {
                    tokens.insert(row.installation_id, Some(t.clone()));
                    t
                }
                Err(error) => {
                    tracing::warn!(%error, installation = row.installation_id, "outbox: mint token failed");
                    tokens.insert(row.installation_id, None);
                    let _ = crate::db::mark_outbox_failed(pool, row.id, "mint token failed").await;
                    continue;
                }
            },
        };
        if row.attempts > 0 {
            tracing::info!(outbox_id = row.id, attempts = row.attempts, kind = %row.kind, "outbox: retrying delivery");
        }
        match deliver(pool, app, &token, review, &row).await {
            Ok(github_id) => {
                if let Err(error) = crate::db::mark_outbox_posted(pool, row.id, github_id).await {
                    tracing::warn!(%error, outbox_id = row.id, "marking outbox posted failed");
                }
                posted += 1;
            }
            Err(error) => {
                tracing::warn!(%error, outbox_id = row.id, kind = %row.kind, "outbox delivery failed (will back off)");
                let _ = crate::db::mark_outbox_failed(pool, row.id, &error.to_string()).await;
            }
        }
    }
    Ok(posted)
}

/// Post one intent. Returns the GitHub id to record (review/comment) or `None`. An `Err` backs the row
/// off for retry.
async fn deliver(
    pool: &PgPool,
    app: &GithubApp,
    token: &str,
    review: &ReviewSection,
    row: &crate::db::OutboxRow,
) -> anyhow::Result<Option<i64>> {
    match row.kind.as_str() {
        "reaction" => {
            let content = payload_str(&row.payload, "content")?;
            // ADR-0068: when the payload carries a `comment_id`, react on the triggering @mention comment;
            // otherwise on the PR/issue body (the automatic-review case).
            match row.payload.get("comment_id").and_then(|x| x.as_i64()) {
                Some(comment_id) => {
                    app.add_comment_reaction(token, &row.owner, &row.repo, comment_id, content)
                        .await?;
                }
                None => {
                    let issue = payload_i64(&row.payload, "issue")?;
                    app.add_reaction(token, &row.owner, &row.repo, issue, content)
                        .await?;
                }
            }
            Ok(None)
        }
        "reply" => {
            let issue = payload_i64(&row.payload, "issue")?;
            let body = payload_str(&row.payload, "body")?;
            let posted = app
                .create_issue_comment(token, &row.owner, &row.repo, issue, body)
                .await?;
            record_comment(pool, row.task_id, posted.id, "reply").await;
            Ok(posted.id)
        }
        "failure_notice" => {
            // Re-check the dedup gate at post time and consume silently if the task already responded —
            // OR is *about to*: a `review`/`reply` intent still pending in the outbox (e.g. one that
            // transiently 502'd and is backing off) means a real review is coming, so don't race a
            // misleading apology ahead of it (#219 review). A dead-lettered (`failed`) review is excluded,
            // so a review that truly can't be delivered still yields a notice.
            if let Some(task) = row.task_id {
                if crate::db::has_responded_or_pending_content(pool, task)
                    .await
                    .unwrap_or(false)
                {
                    return Ok(None);
                }
            }
            let issue = payload_i64(&row.payload, "issue")?;
            let body = payload_str(&row.payload, "body")?;
            let posted = app
                .create_issue_comment(token, &row.owner, &row.repo, issue, body)
                .await?;
            record_comment(pool, row.task_id, posted.id, "failure_notice").await;
            Ok(posted.id)
        }
        "review" => deliver_review(pool, app, token, review, row).await,
        other => anyhow::bail!("unknown outbox kind {other:?}"),
    }
}

/// Post the grouped review and its success side-effects (persist the copy, fetch inline ids, apply
/// outcome labels) — the whole bundle the old synchronous `finalize_review` did, now driven from the
/// pre-shaped payload. The verdict reaction is enqueued separately at finalize (ADR-0068).
async fn deliver_review(
    pool: &PgPool,
    app: &GithubApp,
    token: &str,
    review: &ReviewSection,
    row: &crate::db::OutboxRow,
) -> anyhow::Result<Option<i64>> {
    let p: crate::outbox::ReviewPayload = serde_json::from_value(row.payload.clone())?;
    let comments: Vec<ReviewComment> = p
        .comments
        .iter()
        .map(|c| ReviewComment {
            path: c.path.clone(),
            line: c.line,
            side: "RIGHT",
            body: c.body.clone(),
        })
        .collect();
    let posted = app
        .create_pr_review(token, &row.owner, &row.repo, p.pr, &p.body, &comments)
        .await?;
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
            match app
                .list_review_comments(token, &row.owner, &row.repo, p.pr, review_id)
                .await
            {
                Ok(refs) => {
                    let stored: Vec<crate::db::ReviewCommentRef> = refs
                        .into_iter()
                        .map(|c| crate::db::ReviewCommentRef {
                            github_comment_id: c.id,
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

    // Outcome labels (ADR rides the outbox, not a 2nd serve writer) + the 🎉 reaction — both best-effort.
    let mut labels = Vec::new();
    if let Some(l) = &review.label_reviewed {
        labels.push(l.clone());
    }
    if p.label_findings {
        if let Some(l) = &review.label_findings {
            labels.push(l.clone());
        }
    }
    if p.label_error {
        if let Some(l) = &review.label_error {
            labels.push(l.clone());
        }
    }
    if !labels.is_empty() {
        if let Err(error) = app
            .add_labels(token, &row.owner, &row.repo, p.pr, &labels)
            .await
        {
            tracing::warn!(%error, pr = p.pr, "applying outcome labels failed (non-fatal)");
        }
    }
    // ADR-0068: the verdict reaction (👎 findings / 👍 clean) is a separate `reaction` intent enqueued at
    // finalize — a `review` intent is only ever produced when there ARE findings, so the old
    // unconditional 🎉 here is gone.
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
            github_comment_id: cid,
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
/// reconcile. Returns the number checked. Mints at most one token per installation per cycle.
async fn poll_once(
    pool: &PgPool,
    app: &GithubApp,
    within_days: i32,
    interval_secs: i64,
) -> anyhow::Result<usize> {
    let comments = crate::db::list_pollable_comments(pool, within_days, interval_secs).await?;
    let mut tokens: HashMap<i64, Option<String>> = HashMap::new();
    let mut checked = 0;
    for c in &comments {
        let token = match tokens.get(&c.installation_id) {
            Some(Some(t)) => t.clone(),
            Some(None) => continue,
            None => match app.installation_token(c.installation_id).await {
                Ok(t) => {
                    tokens.insert(c.installation_id, Some(t.clone()));
                    t
                }
                Err(error) => {
                    tracing::warn!(%error, installation = c.installation_id, "mint token for poll failed");
                    tokens.insert(c.installation_id, None);
                    continue;
                }
            },
        };
        let is_review_comment = c.kind == "inline";
        match app
            .list_comment_reactions(
                &token,
                &c.owner,
                &c.name,
                c.github_comment_id,
                is_review_comment,
            )
            .await
        {
            Ok(reactions) => {
                if let Err(error) = crate::db::reconcile_comment_feedback(
                    pool,
                    c.task_id,
                    c.github_comment_id,
                    &c.kind,
                    &reactions,
                )
                .await
                {
                    tracing::warn!(%error, comment = c.github_comment_id, "reconciling feedback failed");
                } else {
                    checked += 1;
                }
            }
            Err(error) => {
                tracing::warn!(%error, comment = c.github_comment_id, "reading reactions failed")
            }
        }
    }
    Ok(checked)
}
