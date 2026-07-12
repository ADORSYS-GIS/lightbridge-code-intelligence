//! Admin API for the approval gate (Epic #75, Milestone A).
//!
//! The GitHub App can be installed on any org/repo, but a repository is **not** indexed or reviewed
//! until approved (so nobody can point the tool at arbitrary private repos). These endpoints are
//! gated by **permissions** carried in the OIDC token (`repo:read`/`repo:approve`/`repo:deny`,
//! ADR-0023) via the [`Caller`] extractor.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::AppState;
use crate::jwt::Caller;

#[derive(Debug, Deserialize)]
pub struct RepoListQuery {
    /// Optional approval-status filter, e.g. `?status=pending` for the approval queue.
    pub status: Option<String>,
}

/// `GET /admin/repositories[?status=pending]` — repositories for the admin console; filter by status
/// to show the approval queue.
pub async fn list_repositories(
    caller: Caller,
    State(state): State<AppState>,
    Query(query): Query<RepoListQuery>,
) -> Response {
    if let Err(e) = caller.require("repo:read") {
        return e.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    match crate::db::list_repositories(pool, query.status.as_deref()).await {
        Ok(repos) => Json(repos).into_response(),
        Err(error) => {
            tracing::error!(%error, "admin list repositories failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response()
        }
    }
}

/// `POST /admin/repositories/{id}/approve` — opt a repository in (opens the gate + triggers its base
/// index). Requires `repo:approve`. Records the approver's identity for audit.
pub async fn approve(
    caller: Caller,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    if let Err(e) = caller.require("repo:approve") {
        return e.into_response();
    }
    set_status(caller, state, id, "approved").await
}

/// `POST /admin/repositories/{id}/deny` — keep a repository out of scope (sets `disabled` + purges
/// its index data). Requires `repo:deny`.
pub async fn deny(caller: Caller, State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    if let Err(e) = caller.require("repo:deny") {
        return e.into_response();
    }
    set_status(caller, state, id, "disabled").await
}

/// Shared by approve/deny (permission already checked by the caller). Plain helper, not a handler.
async fn set_status(caller: Caller, state: AppState, id: i64, status: &str) -> Response {
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    let by = caller.claims.identity();
    match crate::db::set_repository_status_by_id(pool, id, status, Some(by)).await {
        Ok(Some(repo)) => {
            tracing::info!(
                repo_id = id,
                status,
                admin = by,
                "admin set repository status"
            );
            // Denial removes the repo from scope → purge its index data (Epic #75, Milestone B).
            if status == "disabled" {
                crate::queue::lifecycle::spawn_purge(&state, id);
            }
            // Approval opts the repo in → index its default branch (Epic #75, Milestone B). Spawned:
            // it makes GitHub calls (token mint, default-branch resolve) that must not block the
            // admin response.
            if status == "approved" {
                let (state, repo_id, owner, name, default_branch) = (
                    state.clone(),
                    repo.id,
                    repo.owner.clone(),
                    repo.name.clone(),
                    repo.default_branch.clone(),
                );
                tokio::spawn(async move {
                    enqueue_index_on_approve(state, repo_id, owner, name, default_branch).await;
                });
            }
            Json(repo).into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, "repository not found").into_response(),
        Err(error) => {
            tracing::error!(%error, repo_id = id, status, "admin set repository status failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response()
        }
    }
}

/// Enqueue the base index for a just-approved repo (best-effort — never fails the approval response).
/// Needs the repo's `installation_id` (to mint a clone token); logs + skips if it's unknown (e.g. a
/// repo approved before any installation/PR webhook recorded it). When the `default_branch` is blank
/// (a repo first seen via an installation webhook, which omits it) it's resolved via the API and
/// persisted, so the runner clones the right ref.
async fn enqueue_index_on_approve(
    state: AppState,
    repo_id: i64,
    owner: String,
    name: String,
    default_branch: String,
) {
    let Some(pool) = state.db.as_ref() else {
        return;
    };
    let installation_id = match crate::db::repository_installation_id(pool, repo_id).await {
        Ok(Some(id)) => id,
        Ok(None) => {
            tracing::warn!(
                repository_id = repo_id,
                "approved but no installation_id recorded; base index skipped (will index on the next PR)"
            );
            return;
        }
        Err(error) => {
            tracing::error!(%error, repository_id = repo_id, "approved: installation_id lookup failed");
            return;
        }
    };

    // Resolve the default branch if it's a placeholder (installation webhooks don't carry it).
    if default_branch.trim().is_empty() {
        match state.github.as_ref() {
            Some(app) => match app.installation_token(installation_id).await {
                Ok(token) => match app.repository_default_branch(&token, &owner, &name).await {
                    Ok(branch) => {
                        if let Err(error) =
                            crate::db::update_repository_default_branch(pool, repo_id, &branch)
                                .await
                        {
                            tracing::error!(%error, repository_id = repo_id, "approved: persist default_branch failed");
                            return;
                        }
                    }
                    Err(error) => {
                        tracing::error!(%error, repository_id = repo_id, "approved: resolve default_branch failed; index skipped");
                        return;
                    }
                },
                Err(error) => {
                    tracing::error!(%error, repository_id = repo_id, "approved: token mint failed; index skipped");
                    return;
                }
            },
            None => {
                tracing::warn!(
                    repository_id = repo_id,
                    "approved but GitHub App unconfigured + no default_branch; index skipped"
                );
                return;
            }
        }
    }

    match crate::db::create_index_task(pool, repo_id, installation_id).await {
        Ok(Some(task_id)) => {
            crate::http::metrics::task_created();
            tracing::info!(repository_id = repo_id, %task_id, "approved: enqueued base index task")
        }
        Ok(None) => {
            tracing::info!(
                repository_id = repo_id,
                "approved: an index task is already active; skipping"
            )
        }
        Err(error) => {
            tracing::error!(%error, repository_id = repo_id, "approved: enqueue index failed")
        }
    }
}
