//! Review and feedback analytics (converse-frontends ADR 0018): the windowed, optionally
//! repository-scoped aggregates behind `GET /analytics/reviews` and `GET /analytics/feedback`.
//!
//! Every query takes a half-open window `[from, to)` and computes the immediately preceding window of
//! the same length in the same statement, so a page's comparison deltas cost no second round trip.
//!
//! **Feedback is a cohort of the comments posted in the window, not a timeline of reactions.** The
//! forge emits no webhook for reactions; the reconciler polls them, and `review_feedback.created_at`
//! is the moment a poll *noticed* one — which can be days after the reaction, and lands a 👍 on a
//! months-old comment in today's bucket. Attributing each reaction to the comment it stands on instead
//! answers "how were the findings we posted that week received", and gives a number that does not
//! move when the poller happens to run. The honest cost is that a recent bucket is still accumulating
//! reactions, and one older than the poll window (`RECONCILER_WINDOW_DAYS`) is frozen at its last
//! reconciled state; the endpoint reports that window so a page can say so.
//!
//! A reaction reaches a finding through `review_findings` (migration 0040) on the comment's
//! `(task_id, file, line)`. A reaction whose comment matches no finding is counted in `unresolved`
//! rather than dropped, so the size of that gap is visible instead of silently shrinking every
//! by-priority and by-category figure.

use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::integrations::platform::Platform;

/// Most repositories an estate-scope review breakdown lists.
pub const REPOSITORY_BREAKDOWN_LIMIT: i64 = 50;
/// Most findings the "most down-voted" list carries.
pub const TOP_DOWNVOTED_LIMIT: i64 = 10;

/// The window every analytics query ranges over: `[from, to)`, one repository or (`None`) all of
/// them, bucketed by `bucket` — a Postgres interval literal the HTTP layer has already validated
/// (`"1 hour"`, `"7 days"`).
#[derive(Debug, Clone)]
pub struct AnalyticsWindow {
    pub repository_id: Option<i64>,
    pub from: OffsetDateTime,
    pub to: OffsetDateTime,
    pub bucket: String,
}

// ── Reviews ───────────────────────────────────────────────────────────────────────────────────────

/// Run outcomes, run durations, and what the finalized reviews in one window said.
///
/// Runs are windowed on `tasks.created_at`; reviews and findings on `reviews.created_at`, the time the
/// review was finalized — so a run started just before the window whose review landed inside it counts
/// as a review here and as a run in the previous window. Durations cover terminal runs only
/// (succeeded, failed, timed out); a cancelled run's lifetime says nothing about review speed.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ReviewTotals {
    pub runs: i64,
    pub succeeded: i64,
    /// `failed` and `timed_out`.
    pub failed: i64,
    pub cancelled: i64,
    /// `running` and `posting_result`.
    pub active: i64,
    /// `received`, `waiting_for_index` and `queued`.
    pub pending: i64,
    pub p50_duration_secs: Option<f64>,
    pub p95_duration_secs: Option<f64>,
    pub reviews: i64,
    pub findings: i64,
    pub inline: i64,
    pub deferred: i64,
    pub out_of_scope: i64,
}

/// One bucket of the reviews series. Every bucket in the window is present, empty ones as zeros, so a
/// chart never has to invent the gaps.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ReviewSeriesPoint {
    #[serde(with = "time::serde::rfc3339")]
    pub bucket_start: OffsetDateTime,
    pub runs: i64,
    pub succeeded: i64,
    pub failed: i64,
    pub cancelled: i64,
    pub reviews: i64,
    pub findings: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KeyCount {
    pub key: String,
    pub count: i64,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct RepositoryReviews {
    pub repository_id: i64,
    pub owner: String,
    pub name: String,
    pub platform: Platform,
    pub runs: i64,
    pub succeeded: i64,
    pub failed: i64,
    pub reviews: i64,
    pub findings: i64,
}

#[derive(Debug, Serialize)]
pub struct ReviewAnalytics {
    pub current: ReviewTotals,
    pub previous: ReviewTotals,
    pub series: Vec<ReviewSeriesPoint>,
    pub by_priority: Vec<KeyCount>,
    pub by_category: Vec<KeyCount>,
    /// Estate scope only — `None` when the window names a repository. Most runs first, capped at
    /// [`REPOSITORY_BREAKDOWN_LIMIT`].
    pub by_repository: Option<Vec<RepositoryReviews>>,
    /// Whether `by_repository` was cut at the cap, so a page can say "top 50" rather than imply it
    /// listed every repository.
    pub by_repository_truncated: bool,
}

#[derive(sqlx::FromRow)]
struct RunTotalsRow {
    runs: i64,
    succeeded: i64,
    failed: i64,
    cancelled: i64,
    active: i64,
    pending: i64,
    p50_duration_secs: Option<f64>,
    p95_duration_secs: Option<f64>,
    prev_runs: i64,
    prev_succeeded: i64,
    prev_failed: i64,
    prev_cancelled: i64,
    prev_active: i64,
    prev_pending: i64,
    prev_p50_duration_secs: Option<f64>,
    prev_p95_duration_secs: Option<f64>,
}

#[derive(sqlx::FromRow)]
struct ReviewTotalsRow {
    reviews: i64,
    findings: i64,
    inline: i64,
    deferred: i64,
    out_of_scope: i64,
    prev_reviews: i64,
    prev_findings: i64,
    prev_inline: i64,
    prev_deferred: i64,
    prev_out_of_scope: i64,
}

// `$1` repository (nullable), `$2` from, `$3` to. The previous window is `[$2 - ($3 - $2), $2)`.
pub(crate) const RUN_TOTALS_SQL: &str = "\
SELECT \
    count(*) FILTER (WHERE cur) AS runs, \
    count(*) FILTER (WHERE cur AND status = 'succeeded') AS succeeded, \
    count(*) FILTER (WHERE cur AND status IN ('failed', 'timed_out')) AS failed, \
    count(*) FILTER (WHERE cur AND status = 'cancelled') AS cancelled, \
    count(*) FILTER (WHERE cur AND status IN ('running', 'posting_result')) AS active, \
    count(*) FILTER (WHERE cur AND status IN ('received', 'waiting_for_index', 'queued')) AS pending, \
    percentile_cont(0.5) WITHIN GROUP (ORDER BY secs) FILTER (WHERE cur) AS p50_duration_secs, \
    percentile_cont(0.95) WITHIN GROUP (ORDER BY secs) FILTER (WHERE cur) AS p95_duration_secs, \
    count(*) FILTER (WHERE NOT cur) AS prev_runs, \
    count(*) FILTER (WHERE NOT cur AND status = 'succeeded') AS prev_succeeded, \
    count(*) FILTER (WHERE NOT cur AND status IN ('failed', 'timed_out')) AS prev_failed, \
    count(*) FILTER (WHERE NOT cur AND status = 'cancelled') AS prev_cancelled, \
    count(*) FILTER (WHERE NOT cur AND status IN ('running', 'posting_result')) AS prev_active, \
    count(*) FILTER (WHERE NOT cur AND status IN ('received', 'waiting_for_index', 'queued')) AS prev_pending, \
    percentile_cont(0.5) WITHIN GROUP (ORDER BY secs) FILTER (WHERE NOT cur) AS prev_p50_duration_secs, \
    percentile_cont(0.95) WITHIN GROUP (ORDER BY secs) FILTER (WHERE NOT cur) AS prev_p95_duration_secs \
FROM ( \
    SELECT t.status, \
           t.created_at >= $2::timestamptz AS cur, \
           CASE WHEN t.status IN ('succeeded', 'failed', 'timed_out') \
                     AND t.completed_at >= t.started_at \
                THEN extract(epoch FROM t.completed_at - t.started_at)::float8 END AS secs \
    FROM tasks t \
    WHERE ($1::bigint IS NULL OR t.repository_id = $1) \
      AND t.created_at >= $2::timestamptz - ($3::timestamptz - $2::timestamptz) \
      AND t.created_at < $3::timestamptz \
) w";

pub(crate) const REVIEW_TOTALS_SQL: &str = "\
SELECT \
    count(*) FILTER (WHERE cur) AS reviews, \
    coalesce(sum(n_findings) FILTER (WHERE cur), 0)::int8 AS findings, \
    coalesce(sum(inline_count) FILTER (WHERE cur), 0)::int8 AS inline, \
    coalesce(sum(deferred_count) FILTER (WHERE cur), 0)::int8 AS deferred, \
    coalesce(sum(out_of_scope_count) FILTER (WHERE cur), 0)::int8 AS out_of_scope, \
    count(*) FILTER (WHERE NOT cur) AS prev_reviews, \
    coalesce(sum(n_findings) FILTER (WHERE NOT cur), 0)::int8 AS prev_findings, \
    coalesce(sum(inline_count) FILTER (WHERE NOT cur), 0)::int8 AS prev_inline, \
    coalesce(sum(deferred_count) FILTER (WHERE NOT cur), 0)::int8 AS prev_deferred, \
    coalesce(sum(out_of_scope_count) FILTER (WHERE NOT cur), 0)::int8 AS prev_out_of_scope \
FROM ( \
    SELECT rv.created_at >= $2::timestamptz AS cur, \
           rv.inline_count, rv.deferred_count, rv.out_of_scope_count, \
           (SELECT count(*) FROM review_findings f WHERE f.task_id = rv.task_id) AS n_findings \
    FROM reviews rv \
    JOIN tasks t ON t.id = rv.task_id \
    WHERE ($1::bigint IS NULL OR t.repository_id = $1) \
      AND rv.created_at >= $2::timestamptz - ($3::timestamptz - $2::timestamptz) \
      AND rv.created_at < $3::timestamptz \
) w";

// `$4` bucket interval. Buckets are anchored at `$2`, so `date_bin` and `generate_series` agree.
const REVIEW_SERIES_SQL: &str = "\
WITH buckets AS ( \
    SELECT generate_series($2::timestamptz, $3::timestamptz - interval '1 microsecond', $4::interval) \
           AS bucket_start \
), runs AS ( \
    SELECT date_bin($4::interval, t.created_at, $2::timestamptz) AS bucket_start, \
           count(*) AS runs, \
           count(*) FILTER (WHERE t.status = 'succeeded') AS succeeded, \
           count(*) FILTER (WHERE t.status IN ('failed', 'timed_out')) AS failed, \
           count(*) FILTER (WHERE t.status = 'cancelled') AS cancelled \
    FROM tasks t \
    WHERE ($1::bigint IS NULL OR t.repository_id = $1) \
      AND t.created_at >= $2::timestamptz AND t.created_at < $3::timestamptz \
    GROUP BY 1 \
), revs AS ( \
    SELECT date_bin($4::interval, x.created_at, $2::timestamptz) AS bucket_start, \
           count(*) AS reviews, \
           coalesce(sum(x.n_findings), 0)::int8 AS findings \
    FROM ( \
        SELECT rv.created_at, \
               (SELECT count(*) FROM review_findings f WHERE f.task_id = rv.task_id) AS n_findings \
        FROM reviews rv \
        JOIN tasks t ON t.id = rv.task_id \
        WHERE ($1::bigint IS NULL OR t.repository_id = $1) \
          AND rv.created_at >= $2::timestamptz AND rv.created_at < $3::timestamptz \
    ) x \
    GROUP BY 1 \
) \
SELECT b.bucket_start, \
       coalesce(runs.runs, 0) AS runs, \
       coalesce(runs.succeeded, 0) AS succeeded, \
       coalesce(runs.failed, 0) AS failed, \
       coalesce(runs.cancelled, 0) AS cancelled, \
       coalesce(revs.reviews, 0) AS reviews, \
       coalesce(revs.findings, 0) AS findings \
FROM buckets b \
LEFT JOIN runs ON runs.bucket_start = b.bucket_start \
LEFT JOIN revs ON revs.bucket_start = b.bucket_start \
ORDER BY b.bucket_start";

const FINDING_BREAKDOWN_SQL: &str = "\
SELECT CASE WHEN GROUPING(f.priority) = 0 THEN 'priority' ELSE 'category' END AS dimension, \
       coalesce(f.priority, f.category) AS key, \
       count(*) AS count \
FROM review_findings f \
JOIN reviews rv ON rv.task_id = f.task_id \
JOIN tasks t ON t.id = rv.task_id \
WHERE ($1::bigint IS NULL OR t.repository_id = $1) \
  AND rv.created_at >= $2::timestamptz AND rv.created_at < $3::timestamptz \
GROUP BY GROUPING SETS ((f.priority), (f.category)) \
ORDER BY dimension, count DESC, key";

// `$1` from, `$2` to, `$3` limit.
const REPOSITORY_REVIEWS_SQL: &str = "\
WITH runs AS ( \
    SELECT t.repository_id, \
           count(*) AS runs, \
           count(*) FILTER (WHERE t.status = 'succeeded') AS succeeded, \
           count(*) FILTER (WHERE t.status IN ('failed', 'timed_out')) AS failed \
    FROM tasks t \
    WHERE t.created_at >= $1::timestamptz AND t.created_at < $2::timestamptz \
    GROUP BY 1 \
), revs AS ( \
    SELECT t.repository_id, \
           count(*) AS reviews, \
           coalesce(sum(x.n_findings), 0)::int8 AS findings \
    FROM ( \
        SELECT rv.task_id, \
               (SELECT count(*) FROM review_findings f WHERE f.task_id = rv.task_id) AS n_findings \
        FROM reviews rv \
        WHERE rv.created_at >= $1::timestamptz AND rv.created_at < $2::timestamptz \
    ) x \
    JOIN tasks t ON t.id = x.task_id \
    GROUP BY 1 \
) \
SELECT r.id AS repository_id, r.owner, r.name, r.platform, \
       coalesce(runs.runs, 0) AS runs, \
       coalesce(runs.succeeded, 0) AS succeeded, \
       coalesce(runs.failed, 0) AS failed, \
       coalesce(revs.reviews, 0) AS reviews, \
       coalesce(revs.findings, 0) AS findings \
FROM runs \
FULL JOIN revs ON revs.repository_id = runs.repository_id \
JOIN repositories r ON r.id = coalesce(runs.repository_id, revs.repository_id) \
ORDER BY runs DESC, reviews DESC, r.owner, r.name \
LIMIT $3";

/// Everything `GET /analytics/reviews` returns for one window.
pub async fn review_analytics(
    pool: &PgPool,
    window: &AnalyticsWindow,
) -> Result<ReviewAnalytics, sqlx::Error> {
    let runs: RunTotalsRow = sqlx::query_as(RUN_TOTALS_SQL)
        .bind(window.repository_id)
        .bind(window.from)
        .bind(window.to)
        .fetch_one(pool)
        .await?;
    let reviews: ReviewTotalsRow = sqlx::query_as(REVIEW_TOTALS_SQL)
        .bind(window.repository_id)
        .bind(window.from)
        .bind(window.to)
        .fetch_one(pool)
        .await?;
    let series = sqlx::query_as::<_, ReviewSeriesPoint>(REVIEW_SERIES_SQL)
        .bind(window.repository_id)
        .bind(window.from)
        .bind(window.to)
        .bind(&window.bucket)
        .fetch_all(pool)
        .await?;
    let breakdown: Vec<(String, String, i64)> = sqlx::query_as(FINDING_BREAKDOWN_SQL)
        .bind(window.repository_id)
        .bind(window.from)
        .bind(window.to)
        .fetch_all(pool)
        .await?;

    let (by_repository, by_repository_truncated) = if window.repository_id.is_none() {
        let mut rows = sqlx::query_as::<_, RepositoryReviews>(REPOSITORY_REVIEWS_SQL)
            .bind(window.from)
            .bind(window.to)
            .bind(REPOSITORY_BREAKDOWN_LIMIT + 1)
            .fetch_all(pool)
            .await?;
        let truncated = rows.len() as i64 > REPOSITORY_BREAKDOWN_LIMIT;
        rows.truncate(REPOSITORY_BREAKDOWN_LIMIT as usize);
        (Some(rows), truncated)
    } else {
        (None, false)
    };

    let mut by_priority = Vec::new();
    let mut by_category = Vec::new();
    for (dimension, key, count) in breakdown {
        let target = if dimension == "priority" {
            &mut by_priority
        } else {
            &mut by_category
        };
        target.push(KeyCount { key, count });
    }

    Ok(ReviewAnalytics {
        current: ReviewTotals {
            runs: runs.runs,
            succeeded: runs.succeeded,
            failed: runs.failed,
            cancelled: runs.cancelled,
            active: runs.active,
            pending: runs.pending,
            p50_duration_secs: runs.p50_duration_secs,
            p95_duration_secs: runs.p95_duration_secs,
            reviews: reviews.reviews,
            findings: reviews.findings,
            inline: reviews.inline,
            deferred: reviews.deferred,
            out_of_scope: reviews.out_of_scope,
        },
        previous: ReviewTotals {
            runs: runs.prev_runs,
            succeeded: runs.prev_succeeded,
            failed: runs.prev_failed,
            cancelled: runs.prev_cancelled,
            active: runs.prev_active,
            pending: runs.prev_pending,
            p50_duration_secs: runs.prev_p50_duration_secs,
            p95_duration_secs: runs.prev_p95_duration_secs,
            reviews: reviews.prev_reviews,
            findings: reviews.prev_findings,
            inline: reviews.prev_inline,
            deferred: reviews.prev_deferred,
            out_of_scope: reviews.prev_out_of_scope,
        },
        series,
        by_priority,
        by_category,
        by_repository,
        by_repository_truncated,
    })
}

// ── Feedback ──────────────────────────────────────────────────────────────────────────────────────

/// The standing reactions on the comments posted in one window.
///
/// Rate and coverage are over **inline** comments — the ones that carry a finding. Reactions on the
/// consolidated reply are reported beside them (`reply_up` / `reply_down`) rather than folded in,
/// since a 👍 on "here is my summary" is not a verdict on any one finding.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct FeedbackTotals {
    /// Inline comments posted in the window — the coverage denominator.
    pub inline_comments: i64,
    /// Of those, how many carry at least one 👍 or 👎.
    pub reacted_inline: i64,
    pub up: i64,
    pub down: i64,
    /// Every other reaction on an inline comment (`heart`, `rocket`, `eyes`, …). Never part of the rate.
    pub other: i64,
    /// Distinct people who reacted to anything posted in the window, reply included.
    pub reactors: i64,
    pub reply_up: i64,
    pub reply_down: i64,
    /// 👍/👎 on inline comments that matched no finding in `review_findings`.
    pub unresolved: i64,
    /// `up / (up + down)`; `None` when nobody gave either.
    pub approval_rate: Option<f64>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct FeedbackSeriesPoint {
    #[serde(with = "time::serde::rfc3339")]
    pub bucket_start: OffsetDateTime,
    pub inline_comments: i64,
    pub up: i64,
    pub down: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KeyReactions {
    pub key: String,
    pub up: i64,
    pub down: i64,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct DownvotedFinding {
    pub task_id: Uuid,
    pub repository_id: i64,
    pub owner: String,
    pub name: String,
    pub platform: Platform,
    /// The PR/MR number the comment was posted on.
    pub target_id: i64,
    pub file: Option<String>,
    pub line: Option<i32>,
    /// `None` (and so priority/category) when the comment matched no finding.
    pub title: Option<String>,
    pub priority: Option<String>,
    pub category: Option<String>,
    pub downvotes: i64,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct RepositoryFeedback {
    pub repository_id: i64,
    pub inline_comments: i64,
    pub up: i64,
    pub down: i64,
}

#[derive(Debug, Serialize)]
pub struct FeedbackAnalytics {
    pub current: FeedbackTotals,
    pub previous: FeedbackTotals,
    pub series: Vec<FeedbackSeriesPoint>,
    /// Resolved reactions only — see [`FeedbackTotals::unresolved`] for the rest.
    pub by_priority: Vec<KeyReactions>,
    pub by_category: Vec<KeyReactions>,
    pub top_downvoted: Vec<DownvotedFinding>,
    /// Estate scope only — every repository with an inline comment in the window.
    pub by_repository: Option<Vec<RepositoryFeedback>>,
}

#[derive(sqlx::FromRow)]
struct FeedbackTotalsRow {
    inline_comments: i64,
    reacted_inline: i64,
    up: i64,
    down: i64,
    other: i64,
    reactors: i64,
    reply_up: i64,
    reply_down: i64,
    unresolved: i64,
    prev_inline_comments: i64,
    prev_reacted_inline: i64,
    prev_up: i64,
    prev_down: i64,
    prev_other: i64,
    prev_reactors: i64,
    prev_reply_up: i64,
    prev_reply_down: i64,
    prev_unresolved: i64,
}

pub(crate) const FEEDBACK_TOTALS_SQL: &str = "\
WITH c AS ( \
    SELECT rc.platform_comment_id, rc.kind, \
           rc.created_at >= $2::timestamptz AS cur, \
           rc.kind <> 'inline' OR EXISTS ( \
               SELECT 1 FROM review_findings rf \
               WHERE rf.task_id = rc.task_id AND rf.file = rc.file AND rf.line = rc.line \
           ) AS resolved \
    FROM review_comments rc \
    JOIN tasks t ON t.id = rc.task_id \
    WHERE rc.kind IN ('inline', 'reply') \
      AND ($1::bigint IS NULL OR t.repository_id = $1) \
      AND rc.created_at >= $2::timestamptz - ($3::timestamptz - $2::timestamptz) \
      AND rc.created_at < $3::timestamptz \
) \
SELECT \
    count(DISTINCT c.platform_comment_id) FILTER (WHERE c.cur AND c.kind = 'inline') AS inline_comments, \
    count(DISTINCT c.platform_comment_id) FILTER (WHERE c.cur AND c.kind = 'inline' AND fb.reaction IN ('+1', '-1')) AS reacted_inline, \
    count(fb.reaction) FILTER (WHERE c.cur AND c.kind = 'inline' AND fb.reaction = '+1') AS up, \
    count(fb.reaction) FILTER (WHERE c.cur AND c.kind = 'inline' AND fb.reaction = '-1') AS down, \
    count(fb.reaction) FILTER (WHERE c.cur AND c.kind = 'inline' AND fb.reaction NOT IN ('+1', '-1')) AS other, \
    count(DISTINCT fb.reactor) FILTER (WHERE c.cur) AS reactors, \
    count(fb.reaction) FILTER (WHERE c.cur AND c.kind = 'reply' AND fb.reaction = '+1') AS reply_up, \
    count(fb.reaction) FILTER (WHERE c.cur AND c.kind = 'reply' AND fb.reaction = '-1') AS reply_down, \
    count(fb.reaction) FILTER (WHERE c.cur AND c.kind = 'inline' AND fb.reaction IN ('+1', '-1') AND NOT c.resolved) AS unresolved, \
    count(DISTINCT c.platform_comment_id) FILTER (WHERE NOT c.cur AND c.kind = 'inline') AS prev_inline_comments, \
    count(DISTINCT c.platform_comment_id) FILTER (WHERE NOT c.cur AND c.kind = 'inline' AND fb.reaction IN ('+1', '-1')) AS prev_reacted_inline, \
    count(fb.reaction) FILTER (WHERE NOT c.cur AND c.kind = 'inline' AND fb.reaction = '+1') AS prev_up, \
    count(fb.reaction) FILTER (WHERE NOT c.cur AND c.kind = 'inline' AND fb.reaction = '-1') AS prev_down, \
    count(fb.reaction) FILTER (WHERE NOT c.cur AND c.kind = 'inline' AND fb.reaction NOT IN ('+1', '-1')) AS prev_other, \
    count(DISTINCT fb.reactor) FILTER (WHERE NOT c.cur) AS prev_reactors, \
    count(fb.reaction) FILTER (WHERE NOT c.cur AND c.kind = 'reply' AND fb.reaction = '+1') AS prev_reply_up, \
    count(fb.reaction) FILTER (WHERE NOT c.cur AND c.kind = 'reply' AND fb.reaction = '-1') AS prev_reply_down, \
    count(fb.reaction) FILTER (WHERE NOT c.cur AND c.kind = 'inline' AND fb.reaction IN ('+1', '-1') AND NOT c.resolved) AS prev_unresolved \
FROM c \
LEFT JOIN review_feedback fb \
       ON fb.platform_comment_id = c.platform_comment_id AND fb.comment_kind = c.kind";

const FEEDBACK_SERIES_SQL: &str = "\
WITH buckets AS ( \
    SELECT generate_series($2::timestamptz, $3::timestamptz - interval '1 microsecond', $4::interval) \
           AS bucket_start \
), c AS ( \
    SELECT rc.platform_comment_id, rc.kind, \
           date_bin($4::interval, rc.created_at, $2::timestamptz) AS bucket_start \
    FROM review_comments rc \
    JOIN tasks t ON t.id = rc.task_id \
    WHERE rc.kind = 'inline' \
      AND ($1::bigint IS NULL OR t.repository_id = $1) \
      AND rc.created_at >= $2::timestamptz AND rc.created_at < $3::timestamptz \
), agg AS ( \
    SELECT c.bucket_start, \
           count(DISTINCT c.platform_comment_id) AS inline_comments, \
           count(fb.reaction) FILTER (WHERE fb.reaction = '+1') AS up, \
           count(fb.reaction) FILTER (WHERE fb.reaction = '-1') AS down \
    FROM c \
    LEFT JOIN review_feedback fb \
           ON fb.platform_comment_id = c.platform_comment_id AND fb.comment_kind = c.kind \
    GROUP BY 1 \
) \
SELECT b.bucket_start, \
       coalesce(agg.inline_comments, 0) AS inline_comments, \
       coalesce(agg.up, 0) AS up, \
       coalesce(agg.down, 0) AS down \
FROM buckets b \
LEFT JOIN agg ON agg.bucket_start = b.bucket_start \
ORDER BY b.bucket_start";

// A comment can match more than one finding at the same location (a deferred finding beside the inline
// one); `LIMIT 1` by position keeps one reaction from being counted once per match.
const FEEDBACK_BREAKDOWN_SQL: &str = "\
WITH c AS ( \
    SELECT rc.platform_comment_id, rc.kind, f.priority, f.category \
    FROM review_comments rc \
    JOIN tasks t ON t.id = rc.task_id \
    JOIN LATERAL ( \
        SELECT rf.priority, rf.category FROM review_findings rf \
        WHERE rf.task_id = rc.task_id AND rf.file = rc.file AND rf.line = rc.line \
        ORDER BY rf.idx LIMIT 1 \
    ) f ON true \
    WHERE rc.kind = 'inline' \
      AND ($1::bigint IS NULL OR t.repository_id = $1) \
      AND rc.created_at >= $2::timestamptz AND rc.created_at < $3::timestamptz \
) \
SELECT CASE WHEN GROUPING(c.priority) = 0 THEN 'priority' ELSE 'category' END AS dimension, \
       coalesce(c.priority, c.category) AS key, \
       count(*) FILTER (WHERE fb.reaction = '+1') AS up, \
       count(*) FILTER (WHERE fb.reaction = '-1') AS down \
FROM c \
JOIN review_feedback fb \
  ON fb.platform_comment_id = c.platform_comment_id AND fb.comment_kind = c.kind \
 AND fb.reaction IN ('+1', '-1') \
GROUP BY GROUPING SETS ((c.priority), (c.category)) \
ORDER BY dimension, down DESC, up DESC, key";

// `$4` limit.
const TOP_DOWNVOTED_SQL: &str = "\
SELECT rc.task_id, t.repository_id, r.owner, r.name, r.platform, t.target_id, rc.file, rc.line, \
       f.title, f.priority, f.category, count(*) AS downvotes \
FROM review_comments rc \
JOIN tasks t ON t.id = rc.task_id \
JOIN repositories r ON r.id = t.repository_id \
JOIN review_feedback fb \
  ON fb.platform_comment_id = rc.platform_comment_id AND fb.comment_kind = rc.kind \
 AND fb.reaction = '-1' \
LEFT JOIN LATERAL ( \
    SELECT rf.title, rf.priority, rf.category FROM review_findings rf \
    WHERE rf.task_id = rc.task_id AND rf.file = rc.file AND rf.line = rc.line \
    ORDER BY rf.idx LIMIT 1 \
) f ON true \
WHERE rc.kind = 'inline' \
  AND ($1::bigint IS NULL OR t.repository_id = $1) \
  AND rc.created_at >= $2::timestamptz AND rc.created_at < $3::timestamptz \
GROUP BY rc.id, t.repository_id, r.owner, r.name, r.platform, t.target_id, \
         f.title, f.priority, f.category \
ORDER BY downvotes DESC, rc.created_at DESC \
LIMIT $4";

// `$1` from, `$2` to.
const REPOSITORY_FEEDBACK_SQL: &str = "\
SELECT t.repository_id, \
       count(DISTINCT rc.platform_comment_id) AS inline_comments, \
       count(fb.reaction) FILTER (WHERE fb.reaction = '+1') AS up, \
       count(fb.reaction) FILTER (WHERE fb.reaction = '-1') AS down \
FROM review_comments rc \
JOIN tasks t ON t.id = rc.task_id \
LEFT JOIN review_feedback fb \
       ON fb.platform_comment_id = rc.platform_comment_id AND fb.comment_kind = rc.kind \
WHERE rc.kind = 'inline' \
  AND rc.created_at >= $1::timestamptz AND rc.created_at < $2::timestamptz \
GROUP BY t.repository_id \
ORDER BY down DESC, up DESC, t.repository_id";

fn approval_rate(up: i64, down: i64) -> Option<f64> {
    let rated = up + down;
    (rated > 0).then(|| up as f64 / rated as f64)
}

/// Everything `GET /analytics/feedback` returns for one window.
pub async fn feedback_analytics(
    pool: &PgPool,
    window: &AnalyticsWindow,
) -> Result<FeedbackAnalytics, sqlx::Error> {
    let totals: FeedbackTotalsRow = sqlx::query_as(FEEDBACK_TOTALS_SQL)
        .bind(window.repository_id)
        .bind(window.from)
        .bind(window.to)
        .fetch_one(pool)
        .await?;
    let series = sqlx::query_as::<_, FeedbackSeriesPoint>(FEEDBACK_SERIES_SQL)
        .bind(window.repository_id)
        .bind(window.from)
        .bind(window.to)
        .bind(&window.bucket)
        .fetch_all(pool)
        .await?;
    let breakdown: Vec<(String, String, i64, i64)> = sqlx::query_as(FEEDBACK_BREAKDOWN_SQL)
        .bind(window.repository_id)
        .bind(window.from)
        .bind(window.to)
        .fetch_all(pool)
        .await?;
    let top_downvoted = sqlx::query_as::<_, DownvotedFinding>(TOP_DOWNVOTED_SQL)
        .bind(window.repository_id)
        .bind(window.from)
        .bind(window.to)
        .bind(TOP_DOWNVOTED_LIMIT)
        .fetch_all(pool)
        .await?;
    let by_repository = if window.repository_id.is_none() {
        Some(
            sqlx::query_as::<_, RepositoryFeedback>(REPOSITORY_FEEDBACK_SQL)
                .bind(window.from)
                .bind(window.to)
                .fetch_all(pool)
                .await?,
        )
    } else {
        None
    };

    let mut by_priority = Vec::new();
    let mut by_category = Vec::new();
    for (dimension, key, up, down) in breakdown {
        let target = if dimension == "priority" {
            &mut by_priority
        } else {
            &mut by_category
        };
        target.push(KeyReactions { key, up, down });
    }

    Ok(FeedbackAnalytics {
        current: FeedbackTotals {
            inline_comments: totals.inline_comments,
            reacted_inline: totals.reacted_inline,
            up: totals.up,
            down: totals.down,
            other: totals.other,
            reactors: totals.reactors,
            reply_up: totals.reply_up,
            reply_down: totals.reply_down,
            unresolved: totals.unresolved,
            approval_rate: approval_rate(totals.up, totals.down),
        },
        previous: FeedbackTotals {
            inline_comments: totals.prev_inline_comments,
            reacted_inline: totals.prev_reacted_inline,
            up: totals.prev_up,
            down: totals.prev_down,
            other: totals.prev_other,
            reactors: totals.prev_reactors,
            reply_up: totals.prev_reply_up,
            reply_down: totals.prev_reply_down,
            unresolved: totals.prev_unresolved,
            approval_rate: approval_rate(totals.prev_up, totals.prev_down),
        },
        series,
        by_priority,
        by_category,
        top_downvoted,
        by_repository,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{
        insert_review_if_absent, reconcile_comment_feedback, upsert_repository, upsert_review,
    };
    use serde_json::{Value, json};
    use time::Duration;
    use time::format_description::well_known::Rfc3339;

    fn ts(raw: &str) -> OffsetDateTime {
        OffsetDateTime::parse(raw, &Rfc3339).expect("rfc3339")
    }

    /// 1–7 August 2026; its predecessor is 25–31 July.
    fn week(repository_id: Option<i64>) -> AnalyticsWindow {
        AnalyticsWindow {
            repository_id,
            from: ts("2026-08-01T00:00:00Z"),
            to: ts("2026-08-08T00:00:00Z"),
            bucket: "1 day".to_string(),
        }
    }

    async fn repo(pool: &PgPool, platform_repo_id: i64, name: &str) -> i64 {
        upsert_repository(
            pool,
            Platform::GitHub,
            platform_repo_id,
            "acme",
            name,
            "main",
            None,
        )
        .await
        .unwrap()
    }

    async fn task(
        pool: &PgPool,
        repository_id: i64,
        created_at: &str,
        status: &str,
        secs: Option<i64>,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let created = ts(created_at);
        sqlx::query(
            "INSERT INTO tasks (id, repository_id, installation_id, target_type, target_id, \
             command_text, status, created_at, started_at, completed_at) \
             VALUES ($1, $2, 1, 'pull_request', $3, 'review', $4, $5, $5, $6)",
        )
        .bind(id)
        .bind(repository_id)
        .bind((id.as_u128() % 1_000_000) as i64)
        .bind(status)
        .bind(created)
        .bind(secs.map(|s| created + Duration::seconds(s)))
        .execute(pool)
        .await
        .unwrap();
        id
    }

    /// A review through the real write path (so the projection trigger runs), then back-dated.
    async fn review(pool: &PgPool, task_id: Uuid, at: &str, findings: Value) {
        let inline = findings.as_array().map_or(0, |a| a.len() as i32);
        upsert_review(
            pool, task_id, "summary", "body", inline, 0, 0, &findings, None, None,
        )
        .await
        .unwrap();
        sqlx::query("UPDATE reviews SET created_at = $2 WHERE task_id = $1")
            .bind(task_id)
            .bind(ts(at))
            .execute(pool)
            .await
            .unwrap();
    }

    async fn comment(
        pool: &PgPool,
        task_id: Uuid,
        platform_comment_id: i64,
        kind: &str,
        location: Option<(&str, i32)>,
        at: &str,
    ) {
        sqlx::query(
            "INSERT INTO review_comments (id, task_id, platform_comment_id, kind, file, line, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(Uuid::new_v4())
        .bind(task_id)
        .bind(platform_comment_id)
        .bind(kind)
        .bind(location.map(|(file, _)| file))
        .bind(location.map(|(_, line)| line))
        .bind(ts(at))
        .execute(pool)
        .await
        .unwrap();
    }

    /// Reactions through the reconciler's own write path — which stamps `created_at = now()`, well
    /// outside every window below. That is the point: feedback must be attributed to the comment.
    async fn react(
        pool: &PgPool,
        task_id: Uuid,
        comment_id: i64,
        kind: &str,
        who: &[(&str, &str)],
    ) {
        let pairs: Vec<(String, String)> = who
            .iter()
            .map(|(reactor, reaction)| (reactor.to_string(), reaction.to_string()))
            .collect();
        reconcile_comment_feedback(pool, task_id, comment_id, kind, &pairs)
            .await
            .unwrap();
    }

    async fn projected(pool: &PgPool, task_id: Uuid) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM review_findings WHERE task_id = $1")
            .bind(task_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    fn close(actual: Option<f64>, expected: f64) -> bool {
        actual.is_some_and(|value| (value - expected).abs() < 1e-9)
    }

    fn kc(key: &str, count: i64) -> KeyCount {
        KeyCount {
            key: key.to_string(),
            count,
        }
    }

    fn kr(key: &str, up: i64, down: i64) -> KeyReactions {
        KeyReactions {
            key: key.to_string(),
            up,
            down,
        }
    }

    #[sqlx::test]
    async fn review_findings_projects_every_write_with_the_adr_0032_fallbacks(pool: PgPool) {
        let repository = repo(&pool, 1, "widgets").await;
        let task_id = task(
            &pool,
            repository,
            "2026-08-02T10:00:00Z",
            "succeeded",
            Some(60),
        )
        .await;
        let findings = json!([
            { "file": "a.rs", "line": 10, "priority": "p1", "category": "security", "title": "explicit" },
            { "file": "b.rs", "line": 12, "severity": "error", "title": "legacy severity" },
            { "file": "c.rs", "line": 3, "severity": "warn", "category": "  ", "title": "blank category" },
            { "file": "d.rs", "line": "not-a-line", "title": "no level at all" },
        ]);
        upsert_review(&pool, task_id, "s", "b", 3, 1, 0, &findings, None, None)
            .await
            .unwrap();

        type Row = (
            i32,
            Option<String>,
            Option<i32>,
            String,
            String,
            Option<String>,
        );
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT idx, file, line, priority, category, title FROM review_findings \
             WHERE task_id = $1 ORDER BY idx",
        )
        .bind(task_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        let row = |idx, file: &str, line, priority: &str, category: &str, title: &str| -> Row {
            (
                idx,
                Some(file.to_string()),
                line,
                priority.to_string(),
                category.to_string(),
                Some(title.to_string()),
            )
        };
        assert_eq!(
            rows,
            vec![
                row(0, "a.rs", Some(10), "P1", "security", "explicit"),
                row(1, "b.rs", Some(12), "P0", "correctness", "legacy severity"),
                row(2, "c.rs", Some(3), "P1", "correctness", "blank category"),
                row(3, "d.rs", None, "P2", "correctness", "no level at all"),
            ]
        );

        let only = json!([{ "file": "z.rs", "line": 1, "priority": "P0", "title": "only" }]);
        upsert_review(&pool, task_id, "s", "b", 1, 0, 0, &only, None, None)
            .await
            .unwrap();
        assert_eq!(
            projected(&pool, task_id).await,
            1,
            "a re-post replaces the projection, never appends to it"
        );

        insert_review_if_absent(&pool, task_id, "clean", "b", 0, 0, 0, &json!([]))
            .await
            .unwrap();
        assert_eq!(
            projected(&pool, task_id).await,
            1,
            "the silent-clean path clobbers neither the posted review nor its projection"
        );

        let malformed = task(
            &pool,
            repository,
            "2026-08-02T11:00:00Z",
            "succeeded",
            Some(5),
        )
        .await;
        upsert_review(
            &pool,
            malformed,
            "s",
            "b",
            0,
            0,
            0,
            &json!({ "not": "an array" }),
            None,
            None,
        )
        .await
        .expect("a findings document that is not an array must not fail the audit write");
        assert_eq!(projected(&pool, malformed).await, 0);
    }

    #[sqlx::test]
    async fn sql_fallbacks_agree_with_the_rust_finding_model(pool: PgPool) {
        let variants = [
            json!({}),
            json!({ "priority": "P0" }),
            json!({ "priority": " p2 " }),
            json!({ "priority": "urgent", "severity": "critical" }),
            json!({ "severity": "ERROR" }),
            json!({ "severity": "high" }),
            json!({ "severity": "Warning" }),
            json!({ "severity": "info" }),
            json!({ "category": "style" }),
            json!({ "category": "" }),
            json!({ "category": "  performance " }),
        ];
        for variant in variants {
            let mut finding = json!({ "file": "x.rs", "line": 1, "title": "t", "body": "b" });
            finding
                .as_object_mut()
                .unwrap()
                .extend(variant.as_object().unwrap().clone());
            let model: crate::review::Finding =
                serde_json::from_value(finding.clone()).expect("a valid Finding");
            let (priority, category): (String, String) =
                sqlx::query_as("SELECT review_finding_priority($1), review_finding_category($1)")
                    .bind(&finding)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(priority, model.priority(), "priority for {finding}");
            assert_eq!(category, model.category(), "category for {finding}");
        }
    }

    #[sqlx::test]
    async fn review_analytics_compares_with_the_preceding_window_and_zero_fills_buckets(
        pool: PgPool,
    ) {
        let widgets = repo(&pool, 1, "widgets").await;
        let gadgets = repo(&pool, 2, "gadgets").await;

        let w1 = task(
            &pool,
            widgets,
            "2026-08-02T09:00:00Z",
            "succeeded",
            Some(60),
        )
        .await;
        review(
            &pool,
            w1,
            "2026-08-02T09:05:00Z",
            json!([
                { "file": "a.rs", "line": 1, "priority": "P0", "category": "security", "title": "leak" },
                { "file": "a.rs", "line": 9, "title": "nit" },
            ]),
        )
        .await;
        task(&pool, widgets, "2026-08-03T09:00:00Z", "failed", Some(30)).await;
        let w0 = task(
            &pool,
            widgets,
            "2026-07-28T09:00:00Z",
            "succeeded",
            Some(100),
        )
        .await;
        review(
            &pool,
            w0,
            "2026-07-28T09:05:00Z",
            json!([{ "file": "a.rs", "line": 2, "title": "old" }]),
        )
        .await;
        task(&pool, widgets, "2026-07-01T09:00:00Z", "succeeded", Some(1)).await; // before both
        task(&pool, gadgets, "2026-08-04T09:00:00Z", "cancelled", None).await;
        let g1 = task(
            &pool,
            gadgets,
            "2026-08-05T09:00:00Z",
            "succeeded",
            Some(10),
        )
        .await;
        review(
            &pool,
            g1,
            "2026-08-05T09:01:00Z",
            json!([{ "file": "b.rs", "line": 4, "priority": "P1", "title": "slow" }]),
        )
        .await;

        let estate = review_analytics(&pool, &week(None)).await.unwrap();
        let current = &estate.current;
        assert_eq!(
            (
                current.runs,
                current.succeeded,
                current.failed,
                current.cancelled
            ),
            (4, 2, 1, 1)
        );
        assert_eq!((current.active, current.pending), (0, 0));
        // Terminal durations 10, 30, 60 — the cancelled run has none.
        assert!(close(current.p50_duration_secs, 30.0), "{current:?}");
        assert!(close(current.p95_duration_secs, 57.0), "{current:?}");
        assert_eq!(
            (current.reviews, current.findings, current.inline),
            (2, 3, 3)
        );
        assert_eq!(
            (
                estate.previous.runs,
                estate.previous.reviews,
                estate.previous.findings
            ),
            (1, 1, 1),
            "the preceding week holds exactly the 28 July run; the 1 July one is in neither"
        );

        assert_eq!(
            estate.series.len(),
            7,
            "one bucket per day, empty ones included"
        );
        assert_eq!(estate.series[0].bucket_start, ts("2026-08-01T00:00:00Z"));
        assert_eq!((estate.series[0].runs, estate.series[0].reviews), (0, 0));
        let aug2 = &estate.series[1];
        assert_eq!(
            (aug2.runs, aug2.succeeded, aug2.reviews, aug2.findings),
            (1, 1, 1, 2)
        );
        assert_eq!(
            estate.series.iter().map(|point| point.runs).sum::<i64>(),
            current.runs,
            "the series and the totals describe the same rows"
        );

        assert_eq!(
            estate.by_priority,
            vec![kc("P0", 1), kc("P1", 1), kc("P2", 1)]
        );
        assert_eq!(
            estate.by_category,
            vec![kc("correctness", 2), kc("security", 1)]
        );

        let repositories = estate
            .by_repository
            .expect("estate scope lists repositories");
        assert!(!estate.by_repository_truncated);
        assert_eq!(
            repositories
                .iter()
                .map(|r| (r.name.as_str(), r.runs, r.reviews, r.findings))
                .collect::<Vec<_>>(),
            vec![("gadgets", 2, 1, 1), ("widgets", 2, 1, 2)]
        );

        let scoped = review_analytics(&pool, &week(Some(widgets))).await.unwrap();
        assert_eq!(
            (
                scoped.current.runs,
                scoped.current.reviews,
                scoped.current.findings
            ),
            (2, 1, 2)
        );
        assert!(scoped.by_repository.is_none());
    }

    #[sqlx::test]
    async fn feedback_is_a_cohort_of_posted_comments_with_unresolved_reactions_kept_visible(
        pool: PgPool,
    ) {
        let widgets = repo(&pool, 1, "widgets").await;
        let gadgets = repo(&pool, 2, "gadgets").await;

        let t1 = task(
            &pool,
            widgets,
            "2026-08-02T09:00:00Z",
            "succeeded",
            Some(60),
        )
        .await;
        review(
            &pool,
            t1,
            "2026-08-02T09:05:00Z",
            json!([
                { "file": "a.rs", "line": 10, "priority": "P0", "category": "security", "title": "leak" },
                { "file": "b.rs", "line": 5, "category": "style", "title": "nit" },
            ]),
        )
        .await;
        comment(
            &pool,
            t1,
            101,
            "inline",
            Some(("a.rs", 10)),
            "2026-08-02T09:06:00Z",
        )
        .await;
        comment(
            &pool,
            t1,
            102,
            "inline",
            Some(("b.rs", 5)),
            "2026-08-02T09:06:00Z",
        )
        .await;
        comment(
            &pool,
            t1,
            103,
            "inline",
            Some(("gone.rs", 1)),
            "2026-08-03T09:06:00Z",
        )
        .await;
        comment(&pool, t1, 104, "reply", None, "2026-08-02T09:07:00Z").await;
        react(
            &pool,
            t1,
            101,
            "inline",
            &[("alice", "-1"), ("bob", "-1"), ("carol", "+1")],
        )
        .await;
        react(
            &pool,
            t1,
            102,
            "inline",
            &[("alice", "+1"), ("dave", "heart")],
        )
        .await;
        react(&pool, t1, 103, "inline", &[("bob", "-1")]).await;
        react(&pool, t1, 104, "reply", &[("alice", "+1")]).await;

        let t0 = task(
            &pool,
            widgets,
            "2026-07-28T09:00:00Z",
            "succeeded",
            Some(60),
        )
        .await;
        review(
            &pool,
            t0,
            "2026-07-28T09:05:00Z",
            json!([{ "file": "a.rs", "line": 10, "title": "older" }]),
        )
        .await;
        comment(
            &pool,
            t0,
            201,
            "inline",
            Some(("a.rs", 10)),
            "2026-07-28T09:06:00Z",
        )
        .await;
        react(&pool, t0, 201, "inline", &[("alice", "+1")]).await;

        let other = task(
            &pool,
            gadgets,
            "2026-08-04T09:00:00Z",
            "succeeded",
            Some(60),
        )
        .await;
        review(
            &pool,
            other,
            "2026-08-04T09:05:00Z",
            json!([{ "file": "c.rs", "line": 1, "title": "elsewhere" }]),
        )
        .await;
        comment(
            &pool,
            other,
            301,
            "inline",
            Some(("c.rs", 1)),
            "2026-08-04T09:06:00Z",
        )
        .await;
        react(&pool, other, 301, "inline", &[("erin", "-1")]).await;

        let scoped = feedback_analytics(&pool, &week(Some(widgets)))
            .await
            .unwrap();
        assert_eq!(
            scoped.current,
            FeedbackTotals {
                inline_comments: 3,
                reacted_inline: 3,
                up: 2,
                down: 3,
                other: 1,
                reactors: 4,
                reply_up: 1,
                reply_down: 0,
                unresolved: 1,
                approval_rate: Some(0.4),
            },
            "the heart is never part of the rate, and the reply's 👍 is reported beside it"
        );
        assert_eq!(
            (
                scoped.previous.inline_comments,
                scoped.previous.up,
                scoped.previous.down
            ),
            (1, 1, 0)
        );
        assert_eq!(scoped.previous.approval_rate, Some(1.0));

        assert_eq!(scoped.by_priority, vec![kr("P0", 1, 2), kr("P2", 1, 0)]);
        assert_eq!(
            scoped.by_category,
            vec![kr("security", 1, 2), kr("style", 1, 0)]
        );

        assert_eq!(
            scoped
                .top_downvoted
                .iter()
                .map(|f| (f.file.as_deref(), f.title.as_deref(), f.downvotes))
                .collect::<Vec<_>>(),
            vec![(Some("a.rs"), Some("leak"), 2), (Some("gone.rs"), None, 1)],
            "an unresolved comment is still listed, without a title it does not have"
        );

        assert_eq!(scoped.series.len(), 7);
        assert_eq!(
            (
                scoped.series[1].inline_comments,
                scoped.series[1].up,
                scoped.series[1].down
            ),
            (2, 2, 2)
        );
        assert_eq!(
            (
                scoped.series[2].inline_comments,
                scoped.series[2].up,
                scoped.series[2].down
            ),
            (1, 0, 1)
        );
        assert!(scoped.by_repository.is_none());

        let estate = feedback_analytics(&pool, &week(None)).await.unwrap();
        assert_eq!(
            (estate.current.inline_comments, estate.current.down),
            (4, 4)
        );
        assert_eq!(
            estate
                .by_repository
                .expect("estate scope lists repositories")
                .iter()
                .map(|r| (r.repository_id, r.inline_comments, r.up, r.down))
                .collect::<Vec<_>>(),
            vec![(widgets, 3, 2, 3), (gadgets, 1, 0, 1)]
        );
    }

    /// ADR 0018 D7: the windowed statements must stay servable by an index. With sequential scans
    /// priced out, a plan that still scans `tasks`/`reviews`/`review_comments` sequentially means no
    /// index matches the window predicate any more — which on a real estate is the page going slow.
    #[sqlx::test]
    async fn windowed_totals_can_be_served_by_an_index(pool: PgPool) {
        let mut conn = pool.acquire().await.unwrap();
        sqlx::query("SET enable_seqscan = off")
            .execute(&mut *conn)
            .await
            .unwrap();
        let window = week(Some(1));
        for (statement, table) in [
            (RUN_TOTALS_SQL, "tasks"),
            (REVIEW_TOTALS_SQL, "reviews"),
            (FEEDBACK_TOTALS_SQL, "review_comments"),
        ] {
            let plan: Vec<String> =
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!("EXPLAIN {statement}")))
                    .bind(window.repository_id)
                    .bind(window.from)
                    .bind(window.to)
                    .fetch_all(&mut *conn)
                    .await
                    .unwrap();
            let plan = plan.join("\n");
            assert!(
                !plan.contains(&format!("Seq Scan on {table}")),
                "the {table} window is no longer index-servable:\n{plan}"
            );
        }
    }
}
