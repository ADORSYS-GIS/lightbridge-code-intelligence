//! Read API for tasks — the dashboard's data source (ADR-0016). Bearer-protected via the `Claims`
//! extractor (a valid OIDC access token is required).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use uuid::Uuid;

use crate::AppState;
use crate::jwt::Caller;

const TASK_LIST_LIMIT: i64 = 100;

/// `GET /tasks` — most recent task runs first.
pub async fn list(caller: Caller, State(state): State<AppState>) -> Response {
    if let Err(e) = caller.require("task:read") {
        return e.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    match crate::db::list_tasks(pool, TASK_LIST_LIMIT).await {
        Ok(tasks) => Json(tasks).into_response(),
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
