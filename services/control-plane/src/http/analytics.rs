//! `GET /analytics/reviews` and `GET /analytics/feedback` — windowed, optionally repository-scoped
//! aggregates for the LCI app's Analytics page and repository Insights tab (ADR-0116).
//!
//! Both are gated on `task:read`: they summarize the rows `GET /tasks` lists, so seeing the summary
//! takes exactly the permission seeing the rows does. They are two endpoints rather than one document
//! because feedback lags reviews by up to a poll cycle and fails independently — a feedback query that
//! errors should cost a page one zone, not its run counts.
//!
//! Every parameter is required and validated loudly. A window that silently became a different
//! window, or a bucket that quietly fell back to a default, would answer a different question than the
//! chart drawn from it claims to.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

use crate::AppState;
use crate::db::AnalyticsWindow;
use crate::jwt::Caller;

/// Longest window a caller may ask for — the range picker's longest preset is 90 days, and an
/// unbounded window is an unbounded scan.
const MAX_WINDOW_DAYS: i64 = 400;
/// Most buckets one response may carry: a month of hourly buckets fits, a quarter of minutes does not.
const MAX_BUCKETS: i64 = 1_000;
/// Largest bucket multiplier accepted, so a width can never overflow a `Duration`.
const MAX_BUCKET_COUNT: i64 = 100_000;

#[derive(Debug, Deserialize)]
pub struct AnalyticsQuery {
    pub repository_id: Option<i64>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub bucket: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BucketUnit {
    Minute,
    Hour,
    Day,
}

/// A bucket width: `<n> minute|hour|day`, singular or plural. Weeks and months are refused rather than
/// approximated — `date_bin` cannot bin by a calendar month, and a "week" that silently meant seven
/// days is the kind of quiet substitution this module refuses everywhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bucket {
    count: i64,
    unit: BucketUnit,
}

impl Bucket {
    fn parse(raw: &str) -> Option<Self> {
        let mut parts = raw.split_whitespace();
        let (Some(count), Some(unit), None) = (parts.next(), parts.next(), parts.next()) else {
            return None;
        };
        if !count.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let count = count
            .parse::<i64>()
            .ok()
            .filter(|n| (1..=MAX_BUCKET_COUNT).contains(n))?;
        let unit = match unit {
            "minute" | "minutes" => BucketUnit::Minute,
            "hour" | "hours" => BucketUnit::Hour,
            "day" | "days" => BucketUnit::Day,
            _ => return None,
        };
        Some(Self { count, unit })
    }

    fn duration(self) -> Duration {
        match self.unit {
            BucketUnit::Minute => Duration::minutes(self.count),
            BucketUnit::Hour => Duration::hours(self.count),
            BucketUnit::Day => Duration::days(self.count),
        }
    }

    /// The Postgres interval literal the queries bin by — canonical, whichever spelling was sent.
    fn interval(self) -> String {
        let unit = match self.unit {
            BucketUnit::Minute => "minute",
            BucketUnit::Hour => "hour",
            BucketUnit::Day => "day",
        };
        if self.count == 1 {
            format!("1 {unit}")
        } else {
            format!("{} {unit}s", self.count)
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnalyticsParams {
    pub repository_id: Option<i64>,
    pub from: OffsetDateTime,
    pub to: OffsetDateTime,
    pub bucket: Bucket,
}

/// A refused query string — always a 400 whose body names the parameter.
#[derive(Debug, PartialEq)]
pub struct AnalyticsRejection(String);

impl IntoResponse for AnalyticsRejection {
    fn into_response(self) -> Response {
        (StatusCode::BAD_REQUEST, self.0).into_response()
    }
}

fn required_timestamp(
    name: &str,
    value: Option<String>,
) -> Result<OffsetDateTime, AnalyticsRejection> {
    let value = value.ok_or_else(|| {
        AnalyticsRejection(format!("`{name}` is required (an RFC3339 timestamp)"))
    })?;
    OffsetDateTime::parse(&value, &Rfc3339)
        .map_err(|_| AnalyticsRejection(format!("`{name}` is not an RFC3339 timestamp")))
}

impl TryFrom<AnalyticsQuery> for AnalyticsParams {
    type Error = AnalyticsRejection;

    fn try_from(query: AnalyticsQuery) -> Result<Self, Self::Error> {
        if query.repository_id.is_some_and(|id| id <= 0) {
            return Err(AnalyticsRejection(
                "`repository_id` must be a positive id".to_string(),
            ));
        }
        let from = required_timestamp("from", query.from)?;
        let to = required_timestamp("to", query.to)?;
        if from >= to {
            return Err(AnalyticsRejection("`from` must be before `to`".to_string()));
        }
        if to - from > Duration::days(MAX_WINDOW_DAYS) {
            return Err(AnalyticsRejection(format!(
                "the window may span at most {MAX_WINDOW_DAYS} days"
            )));
        }
        let raw = query.bucket.ok_or_else(|| {
            AnalyticsRejection(
                "`bucket` is required (e.g. `1 hour`, `1 day`, `7 days`)".to_string(),
            )
        })?;
        let bucket = Bucket::parse(&raw).ok_or_else(|| {
            AnalyticsRejection(format!(
                "`bucket` must be `<n> minute|hour|day`, got `{raw}`"
            ))
        })?;
        let span = (to - from).whole_seconds();
        let width = bucket.duration().whole_seconds();
        let buckets = (span + width - 1) / width;
        if buckets > MAX_BUCKETS {
            return Err(AnalyticsRejection(format!(
                "a `{}` bucket over this window is {buckets} buckets; at most {MAX_BUCKETS}",
                bucket.interval()
            )));
        }
        Ok(Self {
            repository_id: query.repository_id,
            from,
            to,
            bucket,
        })
    }
}

#[derive(Debug, Serialize)]
struct Window {
    #[serde(with = "time::serde::rfc3339")]
    from: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    to: OffsetDateTime,
}

#[derive(Debug, Serialize)]
struct Envelope<T> {
    repository_id: Option<i64>,
    window: Window,
    /// The window of equal length ending exactly where `window` begins — what every `previous`
    /// figure in the body is measured over.
    previous_window: Window,
    bucket: String,
    #[serde(flatten)]
    body: T,
}

#[derive(Debug, Serialize)]
struct FeedbackBody {
    /// How many days back the reconciler keeps reactions current (`RECONCILER_WINDOW_DAYS`). Feedback
    /// on comments older than this is frozen at its last reconciled state.
    poll_window_days: i32,
    #[serde(flatten)]
    analytics: crate::db::FeedbackAnalytics,
}

impl AnalyticsParams {
    fn previous_from(&self) -> OffsetDateTime {
        self.from - (self.to - self.from)
    }

    fn window(&self) -> AnalyticsWindow {
        AnalyticsWindow {
            repository_id: self.repository_id,
            from: self.from,
            to: self.to,
            bucket: self.bucket.interval(),
        }
    }

    fn envelope<T>(&self, body: T) -> Envelope<T> {
        Envelope {
            repository_id: self.repository_id,
            window: Window {
                from: self.from,
                to: self.to,
            },
            previous_window: Window {
                from: self.previous_from(),
                to: self.from,
            },
            bucket: self.bucket.interval(),
            body,
        }
    }
}

/// `GET /analytics/reviews?repository_id=&from=&to=&bucket=` — run outcomes, durations, and what the
/// finalized reviews found, with the previous window beside each total.
pub async fn reviews(
    caller: Caller,
    State(state): State<AppState>,
    Query(query): Query<AnalyticsQuery>,
) -> Response {
    if let Err(e) = caller.require("task:read") {
        return e.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    let params = match AnalyticsParams::try_from(query) {
        Ok(params) => params,
        Err(rejection) => return rejection.into_response(),
    };
    match crate::db::review_analytics(pool, &params.window()).await {
        Ok(analytics) => Json(params.envelope(analytics)).into_response(),
        Err(error) => {
            tracing::error!(%error, "review analytics failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response()
        }
    }
}

/// `GET /analytics/feedback?repository_id=&from=&to=&bucket=` — the standing 👍/👎 on the comments
/// posted in the window, by finding priority and category, plus the most down-voted findings.
pub async fn feedback(
    caller: Caller,
    State(state): State<AppState>,
    Query(query): Query<AnalyticsQuery>,
) -> Response {
    if let Err(e) = caller.require("task:read") {
        return e.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    let params = match AnalyticsParams::try_from(query) {
        Ok(params) => params,
        Err(rejection) => return rejection.into_response(),
    };
    match crate::db::feedback_analytics(pool, &params.window()).await {
        Ok(analytics) => Json(params.envelope(FeedbackBody {
            poll_window_days: crate::reconciler_window_days(),
            analytics,
        }))
        .into_response(),
        Err(error) => {
            tracing::error!(%error, "feedback analytics failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(from: Option<&str>, to: Option<&str>, bucket: Option<&str>) -> AnalyticsQuery {
        AnalyticsQuery {
            repository_id: None,
            from: from.map(str::to_string),
            to: to.map(str::to_string),
            bucket: bucket.map(str::to_string),
        }
    }

    fn rejection(query: AnalyticsQuery) -> String {
        match AnalyticsParams::try_from(query) {
            Ok(params) => panic!("expected a rejection, got {params:?}"),
            Err(AnalyticsRejection(message)) => message,
        }
    }

    const FROM: &str = "2026-08-01T00:00:00Z";
    const TO: &str = "2026-08-08T00:00:00Z";

    #[test]
    fn bucket_accepts_minutes_hours_and_days_in_either_number() {
        for (raw, interval) in [
            ("1 minute", "1 minute"),
            ("15 minutes", "15 minutes"),
            ("1 hour", "1 hour"),
            ("1 hours", "1 hour"),
            ("7 days", "7 days"),
            ("1   day", "1 day"),
        ] {
            assert_eq!(
                Bucket::parse(raw).map(Bucket::interval).as_deref(),
                Some(interval),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn bucket_refuses_weeks_months_and_anything_malformed() {
        for raw in [
            "",
            "day",
            "1 week",
            "1 month",
            "0 days",
            "-1 day",
            "+1 day",
            "1hour",
            "1 Hour",
            "1.5 hours",
            "1 hour ago",
            "99999999999999999999 days",
            "100001 days",
        ] {
            assert_eq!(Bucket::parse(raw), None, "{raw:?}");
        }
    }

    #[test]
    fn every_parameter_is_required_and_named_when_missing() {
        assert!(rejection(query(None, Some(TO), Some("1 day"))).contains("`from`"));
        assert!(rejection(query(Some(FROM), None, Some("1 day"))).contains("`to`"));
        assert!(rejection(query(Some(FROM), Some(TO), None)).contains("`bucket`"));
        assert!(rejection(query(Some("yesterday"), Some(TO), Some("1 day"))).contains("RFC3339"));
    }

    #[test]
    fn an_inverted_or_empty_window_is_refused() {
        assert!(rejection(query(Some(TO), Some(FROM), Some("1 day"))).contains("before"));
        assert!(rejection(query(Some(FROM), Some(FROM), Some("1 day"))).contains("before"));
    }

    #[test]
    fn a_window_beyond_the_cap_is_refused() {
        let message = rejection(query(
            Some("2025-01-01T00:00:00Z"),
            Some("2026-08-01T00:00:00Z"),
            Some("7 days"),
        ));
        assert!(message.contains("400 days"), "{message}");
    }

    #[test]
    fn a_bucket_too_fine_for_the_window_is_refused_with_the_count() {
        let message = rejection(query(Some(FROM), Some(TO), Some("1 minute")));
        assert!(message.contains("10080 buckets"), "{message}");
    }

    #[test]
    fn a_non_positive_repository_id_is_refused() {
        let mut q = query(Some(FROM), Some(TO), Some("1 day"));
        q.repository_id = Some(0);
        assert!(rejection(q).contains("`repository_id`"));
    }

    #[test]
    fn the_previous_window_is_equal_length_and_ends_where_the_current_begins() {
        let params =
            AnalyticsParams::try_from(query(Some(FROM), Some(TO), Some("1 day"))).expect("valid");
        let envelope = params.envelope(());
        assert_eq!(envelope.previous_window.to, params.from);
        assert_eq!(
            envelope.previous_window.from,
            OffsetDateTime::parse("2026-07-25T00:00:00Z", &Rfc3339).unwrap()
        );
    }

    /// The window and bucket travel as query-string text, so the wire format is part of the contract.
    #[test]
    fn parses_a_real_query_string_including_a_form_encoded_space() {
        let uri: axum::http::Uri =
            format!("/analytics/reviews?repository_id=7&from={FROM}&to={TO}&bucket=1+day")
                .parse()
                .expect("valid uri");
        let Query(query) = Query::<AnalyticsQuery>::try_from_uri(&uri).expect("parses");
        let params = AnalyticsParams::try_from(query).expect("valid");
        assert_eq!(params.repository_id, Some(7));
        assert_eq!(params.bucket.interval(), "1 day");
        assert_eq!(params.window().bucket, "1 day");
    }
}
