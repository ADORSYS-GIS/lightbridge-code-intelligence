//! Review feedback (ADR-0035): polling reactions on our own comments, and the rejected-findings memory
//! (ADR-0044) fed back into future reviews. Split out of the former monolithic `db.rs` (ADR-0086
//! follow-up) — pure move, no behavior change.

use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::integrations::platform::Platform;

/// A comment the poller should check for reactions, with the repo coordinates + installation needed to
/// mint a token and hit the reactions API.
#[derive(Debug, sqlx::FromRow)]
pub struct PollableComment {
    pub task_id: Uuid,
    pub platform_comment_id: i64,
    pub kind: String,
    pub owner: String,
    pub name: String,
    pub installation_id: i64,
    pub platform: Platform,
    /// The PR/MR/issue number (GitLab `iid`) — needed by GitLab to address notes through their
    /// parent (there is no global note endpoint). GitHub ignores it.
    pub target_id: i64,
    /// `"pull_request"` or `"issue"` — tells GitLab whether to use MR notes vs issue notes.
    pub target_type: String,
}

/// Comments to poll this cycle (ADR-0035), within `within_days`, **tiered by age** so API usage stays
/// flat regardless of repo activity instead of re-reading every comment every cycle: fresh comments
/// (< 1 day) every cycle, 1–3 days old ~1-in-12 cycles, 3+ days old ~1-in-72 cycles. The tier filter
/// is a deterministic `comment_id % N == current_cycle % N` (the cycle index is wall-clock /
/// `interval_secs`), so it needs no per-comment state and spreads load evenly across cycles. Joined to
/// the repo for owner/name/installation.
pub async fn list_pollable_comments(
    pool: &PgPool,
    within_days: i32,
    interval_secs: i64,
) -> Result<Vec<PollableComment>, sqlx::Error> {
    sqlx::query_as::<_, PollableComment>(
        "SELECT rc.task_id, rc.platform_comment_id, rc.kind, r.owner, r.name, t.installation_id, r.platform, \
                t.target_id, t.target_type \
         FROM review_comments rc \
         JOIN tasks t ON t.id = rc.task_id \
         JOIN repositories r ON r.id = t.repository_id \
         WHERE t.created_at > now() - ($1 * interval '1 day') \
           AND ( \
             rc.created_at > now() - interval '1 day' \
             OR (rc.created_at BETWEEN now() - interval '3 days' AND now() - interval '1 day' \
                 AND (rc.platform_comment_id % 12) = (extract(epoch from now())::bigint / $2) % 12) \
             OR (rc.created_at < now() - interval '3 days' \
                 AND (rc.platform_comment_id % 72) = (extract(epoch from now())::bigint / $2) % 72) \
           ) \
         ORDER BY rc.created_at, rc.id",
    )
    .bind(within_days)
    .bind(interval_secs.max(1))
    .fetch_all(pool)
    .await
}

/// Reconcile the reactions on one comment with GitHub (ADR-0035): insert any new `(reactor, reaction)`
/// and delete any that have disappeared (the un-react case — no webhook needed). `reactions` is the
/// current full set from the API; an empty set removes all stored feedback for the comment.
pub async fn reconcile_comment_feedback(
    pool: &PgPool,
    task_id: Uuid,
    platform_comment_id: i64,
    kind: &str,
    reactions: &[(String, String)],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    for (reactor, reaction) in reactions {
        sqlx::query(
            "INSERT INTO review_feedback \
             (id, task_id, platform_comment_id, comment_kind, reactor, reaction) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (platform_comment_id, comment_kind, reactor, reaction) DO NOTHING",
        )
        .bind(Uuid::new_v4())
        .bind(task_id)
        .bind(platform_comment_id)
        .bind(kind)
        .bind(reactor)
        .bind(reaction)
        .execute(&mut *tx)
        .await?;
    }
    // Drop feedback no longer present on GitHub. `reactor` and `reaction` are constrained values (a
    // GitHub login and a fixed reaction vocabulary), so `|` is a safe key separator. An empty
    // `present` set makes `<> ALL('{}')` true for every row → all are deleted.
    let present: Vec<String> = reactions.iter().map(|(u, c)| format!("{u}|{c}")).collect();
    sqlx::query(
        "DELETE FROM review_feedback \
         WHERE platform_comment_id = $1 AND comment_kind = $2 \
           AND (reactor || '|' || reaction) <> ALL($3)",
    )
    .bind(platform_comment_id)
    .bind(kind)
    .bind(&present)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// One stored reaction, serialized to `GET /tasks/{id}/feedback` (ADR-0035). `file`/`line` are joined
/// from the comment so the dashboard can show feedback per finding.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct FeedbackRow {
    pub platform_comment_id: i64,
    pub comment_kind: String,
    pub reactor: String,
    pub reaction: String,
    pub file: Option<String>,
    pub line: Option<i32>,
}

/// Findings in this repo that a human **rejected** with a 👎 (`-1`) reaction (M1 feedback memory,
/// ADR-0044): joins the reaction → the inline comment's `(file, line)` → the matching finding in that
/// run's `reviews.findings` to recover its title. Fed back into future reviews as "previously rejected
/// here — don't repeat" so the agent stops re-raising the same false positives. Bounded; best-effort
/// (a path-normalization mismatch just misses a row). Returns `(file, line, title)`.
pub async fn rejected_findings_for_repo(
    pool: &PgPool,
    repository_id: i64,
    limit: i64,
) -> Result<Vec<(String, i32, String)>, sqlx::Error> {
    sqlx::query_as::<_, (String, i32, String)>(
        "SELECT DISTINCT rc.file, rc.line, finding->>'title' AS title \
         FROM review_feedback f \
         JOIN review_comments rc \
           ON rc.platform_comment_id = f.platform_comment_id AND rc.kind = f.comment_kind \
         JOIN tasks t ON t.id = f.task_id \
         JOIN reviews r ON r.task_id = f.task_id \
         JOIN LATERAL jsonb_array_elements(r.findings) finding \
           ON finding->>'file' = rc.file AND (finding->>'line')::int = rc.line \
         WHERE t.repository_id = $1 AND f.reaction = '-1' AND rc.kind = 'inline' \
           AND finding->>'title' IS NOT NULL \
         ORDER BY rc.file, rc.line \
         LIMIT $2",
    )
    .bind(repository_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// All feedback recorded for a task (ADR-0035), for the dashboard.
pub async fn get_feedback(pool: &PgPool, task_id: Uuid) -> Result<Vec<FeedbackRow>, sqlx::Error> {
    sqlx::query_as::<_, FeedbackRow>(
        "SELECT f.platform_comment_id, f.comment_kind, f.reactor, f.reaction, rc.file, rc.line \
         FROM review_feedback f \
         LEFT JOIN review_comments rc \
           ON rc.platform_comment_id = f.platform_comment_id AND rc.kind = f.comment_kind \
         WHERE f.task_id = $1 ORDER BY f.created_at, f.id",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await
}
