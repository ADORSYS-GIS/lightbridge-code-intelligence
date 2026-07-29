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

/// Body for `POST /admin/repositories/{id}/model` and `POST /admin/organizations/{id}/model`
/// (ADR-0110, story #501). `Some(model)` sets an override (validated against the operator
/// allowlist); `None`/omitted clears it, reverting resolution to the next tier down.
#[derive(Debug, Deserialize)]
pub struct SetModelBody {
    pub model: Option<String>,
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

/// `GET /admin/models` — the operator-curated model allowlist (ADR-0110). Requires `repo:read`: this
/// is reference data for the admin console's picker, not the write path.
pub async fn list_model_allowlist(caller: Caller, State(state): State<AppState>) -> Response {
    if let Err(e) = caller.require("repo:read") {
        return e.into_response();
    }
    Json(state.model_allowlist.as_ref()).into_response()
}

/// `POST /admin/repositories/{id}/model` — set or clear a repository's model override (ADR-0110).
/// Requires `model:configure`. A named model is validated against the operator allowlist before
/// writing — an unlisted model is rejected with a clear 400 naming the allowlist, never a silent
/// downgrade (story #501's negative-case AC).
pub async fn set_repo_model(
    caller: Caller,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<SetModelBody>,
) -> Response {
    if let Err(e) = caller.require("model:configure") {
        return e.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    match crate::db::repository_status(pool, id).await {
        Ok(Some(_)) => {}
        Ok(None) => return (StatusCode::NOT_FOUND, "repository not found").into_response(),
        Err(error) => {
            tracing::error!(%error, repo_id = id, "repository lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response();
        }
    }

    let Some(model) = body.model else {
        return match crate::db::clear_repo_model_override(pool, id).await {
            Ok(_) => {
                Json(serde_json::json!({ "repository_id": id, "model": null })).into_response()
            }
            Err(error) => {
                tracing::error!(%error, repo_id = id, "clear repo model override failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response()
            }
        };
    };
    if model.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "model must not be empty").into_response();
    }
    if let Err(message) = crate::model::validate_model_allowlist(&state.model_allowlist, &model) {
        return (StatusCode::BAD_REQUEST, message).into_response();
    }
    let by = caller.claims.identity();
    match crate::db::set_repo_model_override(pool, id, &model, by).await {
        Ok(()) => {
            tracing::info!(
                repo_id = id,
                model,
                admin = by,
                "admin set repo model override"
            );
            Json(serde_json::json!({ "repository_id": id, "model": model })).into_response()
        }
        Err(error) => {
            tracing::error!(%error, repo_id = id, "set repo model override failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response()
        }
    }
}

/// `POST /admin/organizations/{installation_id}/model` — set or clear an org (installation)'s model
/// override (ADR-0110). Requires `model:configure`. Unlike the repo endpoint, there's no
/// `organizations` table row to check for existence — `installation_id` is a bare identity carried on
/// `repositories`/`tasks` (mirrors how `NewTask.installation_id` has no FK), so any id is accepted.
pub async fn set_org_model(
    caller: Caller,
    State(state): State<AppState>,
    Path(installation_id): Path<i64>,
    Json(body): Json<SetModelBody>,
) -> Response {
    if let Err(e) = caller.require("model:configure") {
        return e.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };

    let Some(model) = body.model else {
        return match crate::db::clear_org_model_override(pool, installation_id).await {
            Ok(_) => Json(serde_json::json!({ "installation_id": installation_id, "model": null }))
                .into_response(),
            Err(error) => {
                tracing::error!(%error, installation_id, "clear org model override failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response()
            }
        };
    };
    if model.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "model must not be empty").into_response();
    }
    if let Err(message) = crate::model::validate_model_allowlist(&state.model_allowlist, &model) {
        return (StatusCode::BAD_REQUEST, message).into_response();
    }
    let by = caller.claims.identity();
    match crate::db::set_org_model_override(pool, installation_id, &model, by).await {
        Ok(()) => {
            tracing::info!(
                installation_id,
                model,
                admin = by,
                "admin set org model override"
            );
            Json(serde_json::json!({ "installation_id": installation_id, "model": model }))
                .into_response()
        }
        Err(error) => {
            tracing::error!(%error, installation_id, "set org model override failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use sqlx::PgPool;

    use super::*;
    use crate::integrations::platform::Platform;
    use crate::jwt::Claims;

    fn test_state(pool: PgPool, allowlist: Vec<String>) -> AppState {
        AppState {
            github_webhook_secret: std::sync::Arc::new(String::new()),
            seen_deliveries: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
            jwt: None,
            db: Some(pool),
            allow_no_db: true,
            github: None,
            gitlab: None,
            bitbucket: None,
            platforms: std::collections::HashMap::new(),
            runner_token_signer: None,
            neo4j: None,
            metrics: crate::http::metrics::install(),
            review: std::sync::Arc::new(crate::config::ReviewSection::default()),
            knowledge_tools: std::sync::Arc::new(crate::config::KnowledgeToolsSection::default()),
            app_handle: std::sync::Arc::new("lightbridge-assistant".to_string()),
            permissions_claim: std::sync::Arc::new("permissions".to_string()),
            model_allowlist: std::sync::Arc::new(allowlist),
        }
    }

    fn caller(permissions: &[&str]) -> Caller {
        Caller {
            claims: Claims {
                sub: "admin-1".to_string(),
                email: None,
                preferred_username: Some("test-admin".to_string()),
                name: None,
                exp: 9_999_999_999,
                extra: serde_json::Map::new(),
            },
            permissions: permissions.iter().map(|p| p.to_string()).collect(),
        }
    }

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[sqlx::test]
    async fn set_repo_model_without_permission_is_forbidden(pool: PgPool) {
        let repo_id =
            crate::db::upsert_repository(&pool, Platform::GitHub, 1, "octo", "repo", "main", None)
                .await
                .unwrap();
        let state = test_state(pool, vec!["claude-opus-5".to_string()]);
        let response = set_repo_model(
            caller(&[]),
            State(state),
            Path(repo_id),
            Json(SetModelBody {
                model: Some("claude-opus-5".to_string()),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn set_repo_model_outside_the_allowlist_is_rejected_with_a_clear_message(pool: PgPool) {
        let repo_id =
            crate::db::upsert_repository(&pool, Platform::GitHub, 2, "octo", "repo2", "main", None)
                .await
                .unwrap();
        let state = test_state(pool.clone(), vec!["claude-opus-5".to_string()]);
        let response = set_repo_model(
            caller(&["model:configure"]),
            State(state),
            Path(repo_id),
            Json(SetModelBody {
                model: Some("gpt-4-typo".to_string()),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let message = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(message.contains("gpt-4-typo"));
        assert!(message.contains("claude-opus-5"));
        // Never a silent downgrade: no row should have been written.
        assert_eq!(
            crate::db::get_repo_model_override(&pool, repo_id)
                .await
                .unwrap(),
            None
        );
    }

    #[sqlx::test]
    async fn set_repo_model_unknown_repo_is_not_found(pool: PgPool) {
        let state = test_state(pool, vec!["claude-opus-5".to_string()]);
        let response = set_repo_model(
            caller(&["model:configure"]),
            State(state),
            Path(999_999),
            Json(SetModelBody {
                model: Some("claude-opus-5".to_string()),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    async fn set_then_clear_repo_model_override_round_trips(pool: PgPool) {
        let repo_id =
            crate::db::upsert_repository(&pool, Platform::GitHub, 3, "octo", "repo3", "main", None)
                .await
                .unwrap();
        let state = test_state(pool.clone(), vec!["claude-opus-5".to_string()]);
        let set_response = set_repo_model(
            caller(&["model:configure"]),
            State(state.clone()),
            Path(repo_id),
            Json(SetModelBody {
                model: Some("claude-opus-5".to_string()),
            }),
        )
        .await;
        assert_eq!(set_response.status(), StatusCode::OK);
        assert_eq!(
            crate::db::get_repo_model_override(&pool, repo_id)
                .await
                .unwrap(),
            Some("claude-opus-5".to_string())
        );

        let clear_response = set_repo_model(
            caller(&["model:configure"]),
            State(state),
            Path(repo_id),
            Json(SetModelBody { model: None }),
        )
        .await;
        assert_eq!(clear_response.status(), StatusCode::OK);
        assert_eq!(
            crate::db::get_repo_model_override(&pool, repo_id)
                .await
                .unwrap(),
            None
        );
    }

    #[sqlx::test]
    async fn set_org_model_outside_the_allowlist_is_rejected(pool: PgPool) {
        let state = test_state(pool.clone(), vec!["claude-opus-5".to_string()]);
        let response = set_org_model(
            caller(&["model:configure"]),
            State(state),
            Path(777),
            Json(SetModelBody {
                model: Some("not-allowed".to_string()),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            crate::db::get_org_model_override(&pool, 777).await.unwrap(),
            None
        );
    }

    #[sqlx::test]
    async fn set_org_model_without_permission_is_forbidden(pool: PgPool) {
        let state = test_state(pool, vec!["claude-opus-5".to_string()]);
        let response = set_org_model(
            caller(&[]),
            State(state),
            Path(777),
            Json(SetModelBody {
                model: Some("claude-opus-5".to_string()),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn list_model_allowlist_returns_the_configured_list(pool: PgPool) {
        let state = test_state(pool, vec!["claude-opus-5".to_string(), "gpt-5".to_string()]);
        let response = list_model_allowlist(caller(&["repo:read"]), State(state)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body, serde_json::json!(["claude-opus-5", "gpt-5"]));
    }

    #[sqlx::test]
    async fn empty_allowlist_rejects_a_write_even_with_permission(pool: PgPool) {
        let repo_id =
            crate::db::upsert_repository(&pool, Platform::GitHub, 4, "octo", "repo4", "main", None)
                .await
                .unwrap();
        let state = test_state(pool, Vec::new());
        let response = set_repo_model(
            caller(&["model:configure"]),
            State(state),
            Path(repo_id),
            Json(SetModelBody {
                model: Some("anything".to_string()),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
