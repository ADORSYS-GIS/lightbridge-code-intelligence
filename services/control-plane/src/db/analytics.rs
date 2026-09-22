//! Reviewer feedback analytics (ADR-0118): the windowed, optionally repository-scoped aggregates
//! behind `GET /analytics/feedback`.
//!
//! The query takes a half-open window `[from, to)` and computes the immediately preceding window of
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
//! Every figure here is a count of reactions on comments. Nothing reads what a finding *said* — no
//! priority, no category, no title — so a reaction needs no route back to the finding it stands on.

use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;

use crate::integrations::platform::Platform;

/// Most repositories an estate-scope breakdown lists. One more row than this is fetched to tell a
/// full page from a truncated one.
pub const REPOSITORY_BREAKDOWN_LIMIT: i64 = 50;

/// The window every query ranges over: `[from, to)`, one repository or (`None`) all of them,
/// bucketed by `bucket` — a Postgres interval literal the HTTP layer has already validated
/// (`"1 hour"`, `"7 days"`).
#[derive(Debug, Clone)]
pub struct AnalyticsWindow {
    pub repository_id: Option<i64>,
    pub from: OffsetDateTime,
    pub to: OffsetDateTime,
    pub bucket: String,
}

/// What reviewers did with the comments posted in one window.
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

/// One repository's share of the estate's feedback, named so a page can link to it.
#[derive(Debug, Serialize)]
pub struct RepositoryFeedback {
    pub repository_id: i64,
    pub owner: String,
    pub name: String,
    pub platform: Platform,
    pub inline_comments: i64,
    pub up: i64,
    pub down: i64,
    pub approval_rate: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct FeedbackAnalytics {
    pub current: FeedbackTotals,
    pub previous: FeedbackTotals,
    pub series: Vec<FeedbackSeriesPoint>,
    /// Estate scope only — every repository with an inline comment in the window, most-rejected
    /// first, capped at [`REPOSITORY_BREAKDOWN_LIMIT`].
    pub by_repository: Option<Vec<RepositoryFeedback>>,
    /// Whether that cap cut the list short, so a page can say the table is not the whole estate.
    pub by_repository_truncated: bool,
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
    prev_inline_comments: i64,
    prev_reacted_inline: i64,
    prev_up: i64,
    prev_down: i64,
    prev_other: i64,
    prev_reactors: i64,
    prev_reply_up: i64,
    prev_reply_down: i64,
}

#[derive(sqlx::FromRow)]
struct RepositoryFeedbackRow {
    repository_id: i64,
    owner: String,
    name: String,
    platform: Platform,
    inline_comments: i64,
    up: i64,
    down: i64,
}

// Both windows in one statement: the cohort spans `from - (to - from)` to `to`, and `cur` splits it.
// `$1` repository or NULL for the estate, `$2` from, `$3` to.
pub(crate) const FEEDBACK_TOTALS_SQL: &str = "\
WITH c AS ( \
    SELECT rc.platform_comment_id, rc.kind, \
           rc.created_at >= $2::timestamptz AS cur \
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
    count(DISTINCT c.platform_comment_id) FILTER (WHERE NOT c.cur AND c.kind = 'inline') AS prev_inline_comments, \
    count(DISTINCT c.platform_comment_id) FILTER (WHERE NOT c.cur AND c.kind = 'inline' AND fb.reaction IN ('+1', '-1')) AS prev_reacted_inline, \
    count(fb.reaction) FILTER (WHERE NOT c.cur AND c.kind = 'inline' AND fb.reaction = '+1') AS prev_up, \
    count(fb.reaction) FILTER (WHERE NOT c.cur AND c.kind = 'inline' AND fb.reaction = '-1') AS prev_down, \
    count(fb.reaction) FILTER (WHERE NOT c.cur AND c.kind = 'inline' AND fb.reaction NOT IN ('+1', '-1')) AS prev_other, \
    count(DISTINCT fb.reactor) FILTER (WHERE NOT c.cur) AS prev_reactors, \
    count(fb.reaction) FILTER (WHERE NOT c.cur AND c.kind = 'reply' AND fb.reaction = '+1') AS prev_reply_up, \
    count(fb.reaction) FILTER (WHERE NOT c.cur AND c.kind = 'reply' AND fb.reaction = '-1') AS prev_reply_down \
FROM c \
LEFT JOIN review_feedback fb \
       ON fb.platform_comment_id = c.platform_comment_id AND fb.comment_kind = c.kind";

// `generate_series` and `date_bin` share the window's start, so a bucket with no comment in it is a
// zero rather than a gap the chart would interpolate across. `$4` is the bucket interval.
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

// `$1` from, `$2` to, `$3` limit.
const REPOSITORY_FEEDBACK_SQL: &str = "\
SELECT t.repository_id, r.owner, r.name, r.platform, \
       count(DISTINCT rc.platform_comment_id) AS inline_comments, \
       count(fb.reaction) FILTER (WHERE fb.reaction = '+1') AS up, \
       count(fb.reaction) FILTER (WHERE fb.reaction = '-1') AS down \
FROM review_comments rc \
JOIN tasks t ON t.id = rc.task_id \
JOIN repositories r ON r.id = t.repository_id \
LEFT JOIN review_feedback fb \
       ON fb.platform_comment_id = rc.platform_comment_id AND fb.comment_kind = rc.kind \
WHERE rc.kind = 'inline' \
  AND rc.created_at >= $1::timestamptz AND rc.created_at < $2::timestamptz \
GROUP BY t.repository_id, r.owner, r.name, r.platform \
ORDER BY down DESC, up DESC, t.repository_id \
LIMIT $3";

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

    let mut by_repository = None;
    let mut by_repository_truncated = false;
    if window.repository_id.is_none() {
        let mut rows = sqlx::query_as::<_, RepositoryFeedbackRow>(REPOSITORY_FEEDBACK_SQL)
            .bind(window.from)
            .bind(window.to)
            .bind(REPOSITORY_BREAKDOWN_LIMIT + 1)
            .fetch_all(pool)
            .await?;
        by_repository_truncated = rows.len() as i64 > REPOSITORY_BREAKDOWN_LIMIT;
        rows.truncate(REPOSITORY_BREAKDOWN_LIMIT as usize);
        by_repository = Some(
            rows.into_iter()
                .map(|row| RepositoryFeedback {
                    approval_rate: approval_rate(row.up, row.down),
                    repository_id: row.repository_id,
                    owner: row.owner,
                    name: row.name,
                    platform: row.platform,
                    inline_comments: row.inline_comments,
                    up: row.up,
                    down: row.down,
                })
                .collect(),
        );
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
            approval_rate: approval_rate(totals.prev_up, totals.prev_down),
        },
        series,
        by_repository,
        by_repository_truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{reconcile_comment_feedback, upsert_repository};
    use time::Duration;
    use time::format_description::well_known::Rfc3339;
    use uuid::Uuid;

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

    async fn task(pool: &PgPool, repository_id: i64, created_at: &str) -> Uuid {
        let id = Uuid::new_v4();
        let created = ts(created_at);
        sqlx::query(
            "INSERT INTO tasks (id, repository_id, installation_id, target_type, target_id, \
             command_text, status, created_at, started_at, completed_at) \
             VALUES ($1, $2, 1, 'pull_request', $3, 'review', 'succeeded', $4, $4, $5)",
        )
        .bind(id)
        .bind(repository_id)
        .bind((id.as_u128() % 1_000_000) as i64)
        .bind(created)
        .bind(created + Duration::seconds(60))
        .execute(pool)
        .await
        .unwrap();
        id
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

    #[sqlx::test]
    async fn feedback_is_a_cohort_of_the_comments_posted_in_the_window(pool: PgPool) {
        let widgets = repo(&pool, 1, "widgets").await;
        let gadgets = repo(&pool, 2, "gadgets").await;

        let t1 = task(&pool, widgets, "2026-08-02T09:00:00Z").await;
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
            Some(("c.rs", 1)),
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

        let t0 = task(&pool, widgets, "2026-07-28T09:00:00Z").await;
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

        let other = task(&pool, gadgets, "2026-08-04T09:00:00Z").await;
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
            (1, 1, 0),
            "the preceding week is counted from the same statement"
        );
        assert_eq!(scoped.previous.approval_rate, Some(1.0));

        assert_eq!(
            scoped.series.len(),
            7,
            "every day in the window has a point"
        );
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
        assert!(scoped.by_repository.is_none(), "one repository, no table");

        let estate = feedback_analytics(&pool, &week(None)).await.unwrap();
        assert_eq!(
            (estate.current.inline_comments, estate.current.down),
            (4, 4)
        );
        assert!(!estate.by_repository_truncated);
        assert_eq!(
            estate
                .by_repository
                .expect("estate scope lists repositories")
                .iter()
                .map(|r| (
                    r.repository_id,
                    r.name.as_str(),
                    r.inline_comments,
                    r.up,
                    r.down
                ))
                .collect::<Vec<_>>(),
            vec![(widgets, "widgets", 3, 2, 3), (gadgets, "gadgets", 1, 0, 1)]
        );
    }

    /// ADR-0118 D7: the windowed statement must stay servable by an index. With sequential scans
    /// priced out, a plan that still scans `review_comments` sequentially means no index matches the
    /// window predicate any more — which on a real estate is the page going slow.
    #[sqlx::test]
    async fn the_windowed_cohort_can_be_served_by_an_index(pool: PgPool) {
        let mut conn = pool.acquire().await.unwrap();
        sqlx::query("SET enable_seqscan = off")
            .execute(&mut *conn)
            .await
            .unwrap();
        let window = week(Some(1));
        let plan: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "EXPLAIN {FEEDBACK_TOTALS_SQL}"
        )))
        .bind(window.repository_id)
        .bind(window.from)
        .bind(window.to)
        .fetch_all(&mut *conn)
        .await
        .unwrap();
        let plan = plan.join("\n");
        assert!(
            !plan.contains("Seq Scan on review_comments"),
            "the comment cohort is no longer index-servable:\n{plan}"
        );
    }
}
