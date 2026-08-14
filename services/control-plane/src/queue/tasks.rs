//! Read API for tasks — the dashboard's data source (ADR-0016). Bearer-protected via the `Claims`
//! extractor (a valid OIDC access token is required).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::AppState;
use crate::db::TasksPageFilter;
use crate::jwt::Caller;

const TASK_LIST_LIMIT: i64 = 100;
/// `GET /repositories` page size when the caller does not ask for one, and the largest it may ask
/// for. A list view renders a screenful; the ceiling bounds the response for everything else.
const DEFAULT_REPOSITORY_PAGE_SIZE: i64 = 25;
const MAX_REPOSITORY_PAGE_SIZE: i64 = 100;

/// `GET /tasks` query params — all optional, so a bare `GET /tasks` (the Overview page's insights
/// fetch) keeps returning exactly the old "most recent 100, unfiltered" behavior. `status` is one of
/// the dashboard's `StatusVariant` strings (`active`/`pending`/`success`/`error`/`muted`), not a raw
/// DB status — [`status_variant_to_raw`] is the single place that mapping lives.
#[derive(Debug, Deserialize)]
pub struct TasksQuery {
    pub page: Option<i64>,
    pub page_size: Option<i64>,
    pub status: Option<String>,
    pub repository_id: Option<i64>,
    pub q: Option<String>,
}

#[derive(Debug, Serialize)]
struct TasksPage {
    tasks: Vec<crate::db::TaskRow>,
    total: i64,
}

/// Expand a dashboard `StatusVariant` into the raw DB `status` values it covers (mirrors
/// `statusVisual()` in `apps/web/lib/domain/tasks.ts` — that function is the inverse of this one).
/// `None` for `"all"` or an unrecognized value: both mean "no status filter" rather than a 400, since
/// this is a read-only display filter, not a mutation gate.
fn status_variant_to_raw(variant: &str) -> Option<&'static [&'static str]> {
    match variant {
        "pending" => Some(&["received", "waiting_for_index", "queued"]),
        "active" => Some(&["running", "posting_result"]),
        "success" => Some(&["succeeded"]),
        "error" => Some(&["failed", "timed_out"]),
        "muted" => Some(&["cancelled"]),
        _ => None,
    }
}

/// `GET /tasks` — task runs, most recent first. With no query params, behaves exactly as the old
/// fixed-`LIMIT 100` endpoint (the Overview page's insights rely on this); `page`/`page_size`/
/// `status`/`repository_id`/`q` add real server-side pagination and filtering for the Runs page.
pub async fn list(
    caller: Caller,
    State(state): State<AppState>,
    Query(query): Query<TasksQuery>,
) -> Response {
    if let Err(e) = caller.require("task:read") {
        return e.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };

    let page = query.page.unwrap_or(0).max(0);
    let page_size = query
        .page_size
        .unwrap_or(TASK_LIST_LIMIT)
        .clamp(1, TASK_LIST_LIMIT);
    let status = query
        .status
        .as_deref()
        .and_then(status_variant_to_raw)
        .map(|values| values.iter().map(|s| s.to_string()).collect());
    let filter = TasksPageFilter {
        status,
        repository_id: query.repository_id,
        query: query.q,
    };

    match crate::db::list_tasks_page(pool, filter, page_size, page * page_size).await {
        Ok((tasks, total)) => Json(TasksPage { tasks, total }).into_response(),
        Err(error) => {
            tracing::error!(%error, "list tasks failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response()
        }
    }
}

/// `GET /tasks/{id}` — a single task run, or 404.
pub async fn get(caller: Caller, State(state): State<AppState>, Path(id): Path<Uuid>) -> Response {
    if let Err(e) = caller.require("task:read") {
        return e.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    match crate::db::get_task(pool, id).await {
        Ok(Some(task)) => Json(task).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "task not found").into_response(),
        Err(error) => {
            tracing::error!(%error, "get task failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response()
        }
    }
}

/// `POST /tasks/{id}/cancel` — manually cancel an active run. Requires `task:cancel`. Sets the task
/// `cancelled`; the runner's self-cancel poll / the reaper then stop the Job + pod. `409` when the
/// task is already terminal (nothing to cancel), `404` when unknown.
pub async fn cancel(
    caller: Caller,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Response {
    if let Err(e) = caller.require("task:cancel") {
        return e.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    // Distinguish "unknown id" (404) from "already finished" (409) so the UI can message correctly.
    match crate::db::get_task_status(pool, id).await {
        Ok(None) => return (StatusCode::NOT_FOUND, "task not found").into_response(),
        Err(error) => {
            tracing::error!(%error, "cancel: status lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response();
        }
        Ok(Some(_)) => {}
    }
    match crate::db::cancel_task_by_id(pool, id).await {
        Ok(true) => {
            tracing::info!(task_id = %id, by = %caller.claims.identity(), "task cancelled (manual)");
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => (StatusCode::CONFLICT, "task is already finished").into_response(),
        Err(error) => {
            tracing::error!(%error, "cancel task failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "update error").into_response()
        }
    }
}

/// `GET /tasks/{id}/review` — the persisted review for a run (summary + body + findings), or 404 when
/// none was recorded (older run, index task, or a review that never posted).
pub async fn get_review(
    caller: Caller,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Response {
    if let Err(e) = caller.require("review:read") {
        return e.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    match crate::db::get_review(pool, id).await {
        Ok(Some(review)) => Json(review).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no review recorded").into_response(),
        Err(error) => {
            tracing::error!(%error, "get review failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response()
        }
    }
}

/// `GET /tasks/{id}/feedback` — 👍/👎 reactions captured on the run's posted comments (ADR-0035),
/// with the file/line of the finding each reacts to. Empty array when none. Gated `review:read`.
pub async fn get_feedback(
    caller: Caller,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Response {
    if let Err(e) = caller.require("review:read") {
        return e.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    match crate::db::get_feedback(pool, id).await {
        Ok(rows) => Json(rows).into_response(),
        Err(error) => {
            tracing::error!(%error, "get feedback failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response()
        }
    }
}

/// `GET /repositories` query params — all optional. `after_activity_at` + `after_id` are the
/// `last_task_at` and `id` of the last row of the previous page: the keyset cursor, sent as the
/// values it is made of so a paginated URL says what it selects.
#[derive(Debug, Deserialize)]
pub struct RepositoriesQuery {
    pub page_size: Option<i64>,
    pub q: Option<String>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub after_activity_at: Option<OffsetDateTime>,
    pub after_id: Option<i64>,
}

/// Validated [`RepositoriesQuery`]. Building one is the only way into the query, so an out-of-range
/// page size or half a cursor cannot reach SQL.
#[derive(Debug)]
struct RepositoriesParams {
    page_size: i64,
    q: Option<String>,
    after: Option<(OffsetDateTime, i64)>,
}

impl TryFrom<RepositoriesQuery> for RepositoriesParams {
    type Error = (StatusCode, String);

    fn try_from(query: RepositoriesQuery) -> Result<Self, Self::Error> {
        // Rejected rather than clamped: a client asking for 500 and quietly receiving 100 has no way
        // to notice it is paging against a size it did not choose.
        let page_size = match query.page_size {
            None => DEFAULT_REPOSITORY_PAGE_SIZE,
            Some(size) if (1..=MAX_REPOSITORY_PAGE_SIZE).contains(&size) => size,
            Some(_) => {
                return Err(bad_request(format!(
                    "page_size must be between 1 and {MAX_REPOSITORY_PAGE_SIZE}"
                )));
            }
        };
        // Half a cursor is a client bug, not a first page — ignoring the given half would serve page
        // one forever.
        let after = match (query.after_activity_at, query.after_id) {
            (Some(activity_at), Some(id)) => Some((activity_at, id)),
            (None, None) => None,
            _ => {
                return Err(bad_request(
                    "after_activity_at and after_id must be given together".to_string(),
                ));
            }
        };
        Ok(Self {
            page_size,
            // Trimmed before it reaches the pattern: surrounding whitespace in a search box is
            // typing, not a term, and `ILIKE '% shop %'` would match nothing.
            q: query
                .q
                .map(|q| q.trim().to_string())
                .filter(|q| !q.is_empty()),
            after,
        })
    }
}

fn bad_request(message: String) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message)
}

/// Where the next page starts. Serialized under the names the client sends them back as.
#[derive(Debug, Serialize)]
struct RepositoriesCursor {
    #[serde(with = "time::serde::rfc3339")]
    after_activity_at: OffsetDateTime,
    after_id: i64,
}

#[derive(Debug, Serialize)]
struct RepositoriesPage {
    repositories: Vec<crate::db::RepositoryRow>,
    /// `null` on the last page.
    next: Option<RepositoriesCursor>,
}

/// `GET /repositories` — connected repositories + their run activity (the Repositories view), one
/// keyset page at a time, most-recently-active first.
pub async fn list_repositories(
    caller: Caller,
    State(state): State<AppState>,
    Query(query): Query<RepositoriesQuery>,
) -> Response {
    if let Err(e) = caller.require("repo:read") {
        return e.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    let params = match RepositoriesParams::try_from(query) {
        Ok(params) => params,
        Err(rejection) => return rejection.into_response(),
    };

    match crate::db::list_repositories_page(
        pool,
        params.q.as_deref(),
        params.after,
        params.page_size,
    )
    .await
    {
        Ok((repositories, next)) => Json(RepositoriesPage {
            repositories,
            next: next.map(|(after_activity_at, after_id)| RepositoriesCursor {
                after_activity_at,
                after_id,
            }),
        })
        .into_response(),
        Err(error) => {
            tracing::error!(%error, "list repositories failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_variant_to_raw_covers_every_ui_variant() {
        assert_eq!(
            status_variant_to_raw("pending"),
            Some(["received", "waiting_for_index", "queued"].as_slice())
        );
        assert_eq!(
            status_variant_to_raw("active"),
            Some(["running", "posting_result"].as_slice())
        );
        assert_eq!(
            status_variant_to_raw("success"),
            Some(["succeeded"].as_slice())
        );
        assert_eq!(
            status_variant_to_raw("error"),
            Some(["failed", "timed_out"].as_slice())
        );
        assert_eq!(
            status_variant_to_raw("muted"),
            Some(["cancelled"].as_slice())
        );
    }

    #[test]
    fn status_variant_to_raw_treats_all_and_unknown_as_no_filter() {
        assert_eq!(status_variant_to_raw("all"), None);
        assert_eq!(status_variant_to_raw("bogus"), None);
        assert_eq!(status_variant_to_raw(""), None);
    }

    /// The cursor travels as query-string text, so the timestamp's wire format is part of the
    /// contract: parse a real URL rather than only hand-built structs.
    #[test]
    fn repositories_query_parses_a_cursor_from_the_url() {
        let uri: axum::http::Uri =
            "/repositories?page_size=10&q=shop&after_activity_at=2026-02-01T09:14:02Z&after_id=7"
                .parse()
                .expect("valid uri");
        let Query(query) = Query::<RepositoriesQuery>::try_from_uri(&uri).expect("parses");
        let params = RepositoriesParams::try_from(query).expect("valid");

        let expected = OffsetDateTime::parse(
            "2026-02-01T09:14:02Z",
            &time::format_description::well_known::Rfc3339,
        )
        .expect("valid timestamp");

        assert_eq!(params.page_size, 10);
        assert_eq!(params.q.as_deref(), Some("shop"));
        assert_eq!(params.after, Some((expected, 7)));
    }

    fn repositories_query() -> RepositoriesQuery {
        RepositoriesQuery {
            page_size: None,
            q: None,
            after_activity_at: None,
            after_id: None,
        }
    }

    #[test]
    fn repositories_params_default_to_the_first_page() {
        let params = RepositoriesParams::try_from(repositories_query()).expect("valid");
        assert_eq!(params.page_size, DEFAULT_REPOSITORY_PAGE_SIZE);
        assert!(params.after.is_none());
        assert!(params.q.is_none());
    }

    #[test]
    fn repositories_params_reject_an_out_of_range_page_size() {
        for size in [0, -1, MAX_REPOSITORY_PAGE_SIZE + 1] {
            let query = RepositoriesQuery {
                page_size: Some(size),
                ..repositories_query()
            };
            let (status, _) = RepositoriesParams::try_from(query).expect_err("rejected");
            assert_eq!(status, StatusCode::BAD_REQUEST, "page_size {size}");
        }
    }

    #[test]
    fn repositories_params_reject_half_a_cursor() {
        let activity_at = OffsetDateTime::from_unix_timestamp(1_770_000_000).expect("in range");
        let halves = [
            RepositoriesQuery {
                after_activity_at: Some(activity_at),
                ..repositories_query()
            },
            RepositoriesQuery {
                after_id: Some(42),
                ..repositories_query()
            },
        ];
        for query in halves {
            let (status, _) = RepositoriesParams::try_from(query).expect_err("rejected");
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }

        let both = RepositoriesQuery {
            after_activity_at: Some(activity_at),
            after_id: Some(42),
            ..repositories_query()
        };
        let params = RepositoriesParams::try_from(both).expect("valid");
        assert_eq!(params.after, Some((activity_at, 42)));
    }

    #[test]
    fn repositories_params_trim_the_search_and_drop_a_blank_one() {
        let blank = RepositoriesQuery {
            q: Some("   ".to_string()),
            ..repositories_query()
        };
        assert!(
            RepositoriesParams::try_from(blank)
                .expect("valid")
                .q
                .is_none()
        );

        let padded = RepositoriesQuery {
            q: Some("  shop  ".to_string()),
            ..repositories_query()
        };
        let params = RepositoriesParams::try_from(padded).expect("valid");
        assert_eq!(params.q.as_deref(), Some("shop"));
    }
}
