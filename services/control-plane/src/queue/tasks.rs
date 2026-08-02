//! Read API for tasks — the dashboard's data source (ADR-0016). Bearer-protected via the `Claims`
//! extractor (a valid OIDC access token is required).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::AppState;
use crate::db::TasksPageFilter;
use crate::jwt::Caller;

const TASK_LIST_LIMIT: i64 = 100;

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

/// `GET /repositories` — connected repositories + their run activity (the Repositories view).
pub async fn list_repositories(caller: Caller, State(state): State<AppState>) -> Response {
    if let Err(e) = caller.require("repo:read") {
        return e.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    match crate::db::list_repositories(pool, None).await {
        Ok(repos) => Json(repos).into_response(),
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
}
