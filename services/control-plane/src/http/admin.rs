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

#[derive(Debug, Clone, Deserialize)]
pub struct SetPresetBody {
    /// The flat `preset` field to set in `.lightbridge-code-review.jsonc` (ADR-0030). Omit to leave
    /// it untouched (e.g. when only setting `entry_points`).
    pub preset: Option<String>,
    /// Per-entry-point overrides (`{"pr_open": "fast", ...}`). Omit to leave untouched.
    pub entry_points: Option<std::collections::HashMap<String, String>>,
}

/// Parse the existing `.lightbridge-code-review.jsonc` content (if any), update only the
/// `preset`/`entry_points` keys `body` sets, and re-serialize — pure, so it's unit-testable without a
/// platform round-trip. A malformed or non-object existing file is treated as empty (create fresh)
/// rather than failing the whole request — same trust posture
/// `services/control-plane/src/preset.rs` already uses for reading this file. Every other key already
/// present (`conventions`/`architecture`/`focus`/…) is preserved untouched; JSONC comments are not
/// guaranteed to survive the round-trip (ADR-0109's documented limitation — `jsonc_parser` parses past
/// them but doesn't reproduce them).
fn merge_preset_fields(current: Option<&str>, body: &SetPresetBody) -> String {
    let mut doc: serde_json::Value = current
        .and_then(|text| {
            jsonc_parser::parse_to_serde_value(text, &jsonc_parser::ParseOptions::default())
                .ok()
                .flatten()
        })
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    let obj = doc
        .as_object_mut()
        .expect("filtered to is_object above, or freshly constructed as one");
    if let Some(preset) = &body.preset {
        obj.insert(
            "preset".to_string(),
            serde_json::Value::String(preset.clone()),
        );
    }
    if let Some(entry_points) = &body.entry_points {
        obj.insert(
            "entry_points".to_string(),
            serde_json::to_value(entry_points).expect("HashMap<String, String> always serializes"),
        );
    }
    // `to_string_pretty` on a `Value` built purely from objects/strings (never a non-finite float) is
    // infallible in practice; fall back to a minimal valid document rather than unwrap-panicking a
    // request if that assumption is ever wrong.
    let text = serde_json::to_string_pretty(&doc).unwrap_or_else(|error| {
        tracing::error!(%error, "admin set preset: serializing merged config failed; falling back to a minimal document");
        serde_json::to_string_pretty(&serde_json::json!({ "preset": body.preset }))
            .unwrap_or_else(|_| "{}".to_string())
    });
    format!("{text}\n")
}

/// `.lightbridge-code-review.jsonc`'s path, shared by the read and write preset endpoints.
const CONFIG_FILENAME: &str = ".lightbridge-code-review.jsonc";

/// Look up a repository by id and resolve the platform-specific installation/project it belongs to.
/// Shared by [`get_preset`] and [`set_preset`] — both need the exact same repo + installation lookup
/// before picking a `CodePlatform` client.
async fn load_repo_ref(
    pool: &sqlx::PgPool,
    id: i64,
) -> Result<
    (
        crate::db::RepositoryRow,
        crate::integrations::platform::RepoRef,
    ),
    Box<Response>,
> {
    let repo = match crate::db::get_repository_by_id(pool, id).await {
        Ok(Some(repo)) => repo,
        Ok(None) => {
            return Err(Box::new(
                (StatusCode::NOT_FOUND, "repository not found").into_response(),
            ));
        }
        Err(error) => {
            tracing::error!(%error, repo_id = id, "admin preset endpoint: repository lookup failed");
            return Err(Box::new(
                (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response(),
            ));
        }
    };
    let installation_id = match crate::db::repository_installation_id(pool, id).await {
        Ok(Some(installation_id)) => installation_id,
        Ok(None) => {
            return Err(Box::new(
                (
                    StatusCode::CONFLICT,
                    "repository has no recorded installation id yet (no webhook has been received for it)",
                )
                    .into_response(),
            ));
        }
        Err(error) => {
            tracing::error!(%error, repo_id = id, "admin preset endpoint: installation id lookup failed");
            return Err(Box::new(
                (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response(),
            ));
        }
    };
    let repo_ref = crate::integrations::platform::RepoRef {
        platform: repo.platform,
        full_name: format!("{}/{}", repo.owner, repo.name),
        platform_repo_id: repo.platform_repo_id,
        installation_id,
    };
    Ok((repo, repo_ref))
}

/// Pick the `CodePlatform` client for a repo's platform + installation, or a 503 response naming
/// which platform isn't configured. Shared by [`get_preset`] and [`set_preset`].
fn pick_platform(
    state: &AppState,
    platform: crate::integrations::platform::Platform,
    installation_id: i64,
) -> Result<&dyn crate::integrations::platform::CodePlatform, Box<Response>> {
    use crate::integrations::platform::Platform;
    match platform {
        Platform::GitHub => match state.platforms.get(&Platform::GitHub) {
            Some(client) => Ok(client.as_ref()),
            None => Err(Box::new(
                (StatusCode::SERVICE_UNAVAILABLE, "GitHub App not configured").into_response(),
            )),
        },
        Platform::GitLab => match state
            .gitlab
            .as_ref()
            .and_then(|registry| registry.client_for_project(installation_id))
        {
            Some(client) => Ok(client),
            None => Err(Box::new(
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "GitLab project not configured",
                )
                    .into_response(),
            )),
        },
        Platform::Bitbucket => match state
            .bitbucket
            .as_ref()
            .and_then(|registry| registry.client_for_project(installation_id))
        {
            Some(client) => Ok(client),
            None => Err(Box::new(
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Bitbucket repo not configured",
                )
                    .into_response(),
            )),
        },
    }
}

/// `GET /admin/repositories/{id}/preset` — the repo's currently-configured preset/entry_points, read
/// straight from `.lightbridge-code-review.jsonc` (story #500's TUI/web selector shows this before
/// offering to change it). Requires `repo:read` — this is a read of already-repo-visible content, not
/// a new capability, unlike [`set_preset`]. `null`/`{}` when the repo declares nothing (platform
/// defaults apply, per `services/control-plane/src/preset.rs`).
pub async fn get_preset(
    caller: Caller,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    if let Err(e) = caller.require("repo:read") {
        return e.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    let (repo, repo_ref) = match load_repo_ref(pool, id).await {
        Ok(pair) => pair,
        Err(response) => return *response,
    };
    let platform = match pick_platform(&state, repo.platform, repo_ref.installation_id) {
        Ok(platform) => platform,
        Err(response) => return *response,
    };
    let config = crate::preset::fetch_repo_preset_config(platform, &repo_ref, &repo.default_branch)
        .await
        .unwrap_or_default();
    Json(serde_json::json!({
        "preset": config.preset,
        "entry_points": config.entry_points,
    }))
    .into_response()
}

/// `POST /admin/repositories/{id}/preset` — set (or update) a repo's review preset by committing to
/// `.lightbridge-code-review.jsonc` on the repo's default branch (ADR-0109, story #500). Requires
/// `repo:configure`. Read-modify-write: fetches the current file (if any), updates only the
/// `preset`/`entry_points` keys, and commits the result — other author-set fields
/// (conventions/architecture/focus/etc.) are preserved, though JSONC comments are not guaranteed to
/// survive the round-trip (ADR-0109's documented limitation).
pub async fn set_preset(
    caller: Caller,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<SetPresetBody>,
) -> Response {
    if let Err(e) = caller.require("repo:configure") {
        return e.into_response();
    }
    if body.preset.is_none() && body.entry_points.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            "at least one of `preset`/`entry_points` must be set",
        )
            .into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    let (repo, repo_ref) = match load_repo_ref(pool, id).await {
        Ok(pair) => pair,
        Err(response) => return *response,
    };
    let platform = match pick_platform(&state, repo.platform, repo_ref.installation_id) {
        Ok(platform) => platform,
        Err(response) => return *response,
    };
    let admin = caller.claims.identity().to_string();
    let message = format!(
        "chore: update review preset via Lightbridge admin console (by {admin})\n\n\
         {CONFIG_FILENAME} updated by an admin action, not an author commit."
    );
    // The read (current content + the platform's concurrency token) and the merge happen INSIDE
    // `update_repo_file`, as one call — never a separate `get_repo_file` here. A caller-side read
    // followed by a later platform-side write reads two different snapshots, so a concurrent author
    // edit landing in between is silently discarded instead of surfacing as a conflict (the exact
    // TOCTOU gap ADR-0109's `mutate`-closure design exists to close — see the trait doc).
    let body_for_merge = body.clone();
    let mutate: Box<dyn FnOnce(Option<String>) -> String + Send> =
        Box::new(move |current: Option<String>| {
            merge_preset_fields(current.as_deref(), &body_for_merge)
        });
    match platform
        .update_repo_file(&repo_ref, CONFIG_FILENAME, mutate, &message)
        .await
    {
        Ok(()) => {
            tracing::info!(
                repo_id = id,
                admin,
                preset = ?body.preset,
                "admin committed a repo review-config change"
            );
            Json(serde_json::json!({
                "preset": body.preset,
                "entry_points": body.entry_points,
            }))
            .into_response()
        }
        Err(error) => {
            tracing::error!(%error, repo_id = id, "admin set preset: updating the repo config failed");
            (
                StatusCode::BAD_GATEWAY,
                "updating the repo's review config failed",
            )
                .into_response()
        }
    }
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

/// `GET /admin/repositories/{id}/model` — the repo's effective model-override provenance (ADR-0110),
/// resolved with the same precedence [`crate::model::resolve_model_override`] applies at task
/// creation (repo override → org override → none — no repo-file layer for models, unlike the
/// ADR-0111 settings resolver): `source` is `"repo"`/`"org"`/`"none"`, `model` is the resolved value or
/// `null`. A dashboard needs this to render the current state before offering a "set" form — there was
/// previously no read path at all, only the two `POST` writes below. Requires `repo:read`.
pub async fn get_repo_model(
    caller: Caller,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    if let Err(e) = caller.require("repo:read") {
        return e.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    match crate::db::get_repo_model_override(pool, id).await {
        Ok(Some(model)) => {
            Json(serde_json::json!({ "repository_id": id, "model": model, "source": "repo" }))
                .into_response()
        }
        Ok(None) => {
            let installation_id = match crate::db::repository_installation_id(pool, id).await {
                Ok(Some(installation_id)) => installation_id,
                Ok(None) => {
                    return Json(
                        serde_json::json!({ "repository_id": id, "model": null, "source": "none" }),
                    )
                    .into_response();
                }
                Err(error) => {
                    tracing::error!(%error, repo_id = id, "repository installation_id lookup failed");
                    return (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response();
                }
            };
            match crate::db::get_org_model_override(pool, installation_id).await {
                Ok(Some(model)) => Json(
                    serde_json::json!({ "repository_id": id, "model": model, "source": "org" }),
                )
                .into_response(),
                Ok(None) => Json(
                    serde_json::json!({ "repository_id": id, "model": null, "source": "none" }),
                )
                .into_response(),
                Err(error) => {
                    tracing::error!(%error, installation_id, "org model override lookup failed");
                    (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response()
                }
            }
        }
        Err(error) => {
            tracing::error!(%error, repo_id = id, "repo model override lookup failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response()
        }
    }
}

/// `GET /admin/organizations/{installation_id}/model` — the org-level model override, if any.
/// Requires `repo:read`.
pub async fn get_org_model(
    caller: Caller,
    State(state): State<AppState>,
    Path(installation_id): Path<i64>,
) -> Response {
    if let Err(e) = caller.require("repo:read") {
        return e.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    match crate::db::get_org_model_override(pool, installation_id).await {
        Ok(model) => {
            let source = if model.is_some() { "org" } else { "none" };
            Json(
                serde_json::json!({ "installation_id": installation_id, "model": model, "source": source }),
            )
            .into_response()
        }
        Err(error) => {
            tracing::error!(%error, installation_id, "org model override lookup failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response()
        }
    }
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

/// The per-field patch body for `POST /admin/repositories/{id}/settings/override`.
///
/// Each field is `Option<Option<T>>` with `#[serde(default)]`, which distinguishes the three cases the
/// override semantics need: **absent** = leave the stored value alone, explicit **`null`** = clear the
/// override (fall back to the repo file / built-in default), **value** = set it. This is subtle enough
/// that both the absent and null cases are unit-tested.
#[derive(Debug, Default, PartialEq, Deserialize)]
pub struct SetSettingsBody {
    #[serde(default, deserialize_with = "double_option")]
    pub check_run_reporting: Option<Option<bool>>,
    #[serde(default, deserialize_with = "double_option")]
    pub review_on_pr_open: Option<Option<bool>>,
    #[serde(default, deserialize_with = "double_option")]
    pub review_on_push: Option<Option<bool>>,
    #[serde(default, deserialize_with = "double_option")]
    pub push_strategy: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub push_debounce_seconds: Option<Option<i32>>,
    #[serde(default, deserialize_with = "double_option")]
    pub dedup_scope: Option<Option<String>>,
}

/// Deserialize a present-but-null field as `Some(None)` rather than `None`, so an explicit `null`
/// (clear the override) is distinguishable from an omitted key (leave it alone). `#[serde(default)]`
/// alone collapses both to `None`.
fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

impl SetSettingsBody {
    fn into_patch(self) -> crate::db::RepoSettingsPatch {
        crate::db::RepoSettingsPatch {
            check_run_reporting: self.check_run_reporting,
            review_on_pr_open: self.review_on_pr_open,
            review_on_push: self.review_on_push,
            push_strategy: self.push_strategy,
            push_debounce_seconds: self.push_debounce_seconds,
            dedup_scope: self.dedup_scope,
        }
    }
}

/// `GET /admin/repositories/{id}/settings` — a repo's **effective** review settings, each with the
/// layer that produced it (`default` / `file` / `db`). Requires `repo:read`.
///
/// The provenance is the point: with three layers, "why is this repo not posting check runs?" is
/// otherwise unanswerable without reading both the repo file and the database by hand.
pub async fn get_settings(
    caller: Caller,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    if let Err(e) = caller.require("repo:read") {
        return e.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    let (repo, repo_ref) = match load_repo_ref(pool, id).await {
        Ok(pair) => pair,
        Err(response) => return *response,
    };
    // An unconfigured platform is not fatal here (unlike `set_preset`, which must write): resolution
    // simply skips the file layer and reports default/db values, which is still useful.
    let platform = pick_platform(&state, repo.platform, repo_ref.installation_id).ok();
    let resolved =
        crate::settings::resolve_repo_settings(pool, platform, &repo_ref, &repo.default_branch, id)
            .await;
    Json(serde_json::json!({ "repository_id": id, "settings": resolved })).into_response()
}

/// `POST /admin/repositories/{id}/settings/override` — set or clear this repo's DB-side overrides,
/// which BEAT the repo's own `.lightbridge-code-review.jsonc`. Requires `repo:configure`.
///
/// The `/override` path segment is deliberate: `POST .../preset` commits to the repo **file**, whereas
/// this writes the operator row that wins over it. Values are validated here rather than only by the
/// table's CHECK constraints so a typo returns a self-explanatory 400 instead of a 500.
pub async fn set_settings_override(
    caller: Caller,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<SetSettingsBody>,
) -> Response {
    if let Err(e) = caller.require("repo:configure") {
        return e.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    let patch = body.into_patch();
    if patch.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "no settings supplied; send at least one field (a null value clears that override)",
        )
            .into_response();
    }
    if let Some(Some(strategy)) = patch.push_strategy.as_ref()
        && crate::settings::PushStrategy::parse_public(strategy).is_none()
    {
        return (
            StatusCode::BAD_REQUEST,
            format!("unknown push_strategy {strategy:?}; expected supersede, debounce or every"),
        )
            .into_response();
    }
    if let Some(Some(scope)) = patch.dedup_scope.as_ref()
        && crate::settings::DedupScope::parse_public(scope).is_none()
    {
        return (
            StatusCode::BAD_REQUEST,
            format!("unknown dedup_scope {scope:?}; expected pr or commit"),
        )
            .into_response();
    }
    if let Some(Some(secs)) = patch.push_debounce_seconds
        && !crate::settings::debounce_seconds_in_range(secs)
    {
        return (
            StatusCode::BAD_REQUEST,
            format!("push_debounce_seconds {secs} out of range; expected 10..=900"),
        )
            .into_response();
    }
    // 404 on an unknown repo before writing, mirroring `set_repo_model`.
    match crate::db::repository_status(pool, id).await {
        Ok(Some(_)) => {}
        Ok(None) => return (StatusCode::NOT_FOUND, "repository not found").into_response(),
        Err(error) => {
            tracing::error!(%error, repo_id = id, "settings override: repository lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response();
        }
    }
    match crate::db::set_repo_settings(pool, id, &patch, caller.claims.identity()).await {
        Ok(()) => {
            tracing::info!(repo_id = id, by = %caller.claims.identity(), "repo settings override updated");
            (StatusCode::NO_CONTENT).into_response()
        }
        Err(error) => {
            tracing::error!(%error, repo_id = id, "writing repo settings override failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response()
        }
    }
}

// ── Code graph ───────────────────────────────────────────────────────────────────────────────
//
// Two read-only views over the same Neo4j graph the review agent's `lightbridge_graph_semantic_search`
// tool queries, sharing one response shape (`GraphResponse`) so a single frontend renderer serves
// both. Neither endpoint embeds any user-typed text: `similar` reuses a symbol's own already-stored
// embedding as the query vector, so the same node always produces the same result with no new
// embeddings credential and nothing that can fail on unusual input.

/// Shared response shape for every graph-browsing endpoint below.
#[derive(serde::Serialize)]
struct GraphResponse {
    commit: String,
    nodes: Vec<crate::integrations::neo4j::SymbolHit>,
    edges: Vec<crate::integrations::neo4j::RelHit>,
}

/// A `404` body for `get_graph`/`get_similar`. Both can 404 for more than one reason — repository
/// not found, or, for `get_similar` only, the symbol having no stored embedding — and a bare status
/// code can't tell those apart. `reason` is the frontend's dispatch key; `message` is human copy for
/// the case that doesn't get its own written copy client-side.
#[derive(serde::Serialize)]
struct GraphNotFound {
    reason: &'static str,
    message: &'static str,
}

/// The commit scope to query the graph at, for a repository as a whole (not a specific task/PR).
/// `agent-runner`'s base-index write uses the repo's default branch name as the graph's `commit`
/// property when there's no specific head SHA (see `indexer::graph::index_graph`) — this mirrors that
/// exactly, rather than assuming a `latest_indexed_commit` column that doesn't exist.
fn repo_commit_scope(repo: &crate::db::RepositoryRow) -> &str {
    &repo.default_branch
}

/// Query params shared by the neighborhood/overview endpoint.
#[derive(Debug, Deserialize)]
pub struct GraphQuery {
    /// Seed symbol to center the view on. Omitted on first load — falls back to an unseeded slice of
    /// the graph so the canvas is never empty.
    node: Option<String>,
    /// Hops out from `node`, clamped to 1..=3 server-side regardless of what's requested.
    hops: Option<i64>,
    limit: Option<i64>,
}

const GRAPH_NODE_LIMIT_DEFAULT: i64 = 60;
const GRAPH_NODE_LIMIT_MAX: i64 = 200;
/// Smaller default specifically for the unseeded overview — see `get_graph`'s `None` arm.
const GRAPH_OVERVIEW_LIMIT_DEFAULT: i64 = 25;

fn clamp_graph_limit(limit: Option<i64>) -> i64 {
    limit
        .unwrap_or(GRAPH_NODE_LIMIT_DEFAULT)
        .clamp(1, GRAPH_NODE_LIMIT_MAX)
}

/// `GET /admin/repositories/{id}/graph?node=&hops=&limit=` — structural neighborhood browse.
/// `node` omitted returns an unseeded overview slice instead of an error, so the graph tab always has
/// something to render on first load.
pub async fn get_graph(
    caller: Caller,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<GraphQuery>,
) -> Response {
    if let Err(e) = caller.require("repo:read") {
        return e.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    let Some(neo4j) = state.neo4j.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "neo4j not configured").into_response();
    };
    let repo = match crate::db::get_repository_by_id(pool, id).await {
        Ok(Some(repo)) => repo,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(GraphNotFound {
                    reason: "not_found",
                    message: "repository not found",
                }),
            )
                .into_response();
        }
        Err(error) => {
            tracing::error!(%error, repo_id = id, "graph endpoint: repository lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response();
        }
    };
    let commit = repo_commit_scope(&repo).to_string();

    let result = match q.node.as_deref() {
        Some(node_id) => {
            crate::integrations::neo4j::graph_neighborhood(
                neo4j,
                id,
                &commit,
                node_id,
                q.hops.unwrap_or(1),
                clamp_graph_limit(q.limit),
            )
            .await
        }
        // The unseeded overview has no natural grouping (just "the first N symbols in file order"),
        // so a flat cap that reads fine for a focused neighborhood renders as an illegible hairball
        // here — a smaller default specifically for this case keeps the first render readable.
        None => {
            let overview_limit = q
                .limit
                .unwrap_or(GRAPH_OVERVIEW_LIMIT_DEFAULT)
                .clamp(1, GRAPH_NODE_LIMIT_MAX);
            crate::integrations::neo4j::graph_overview(neo4j, id, &commit, overview_limit).await
        }
    };
    match result {
        Ok((nodes, edges)) => Json(GraphResponse {
            commit,
            nodes,
            edges,
        })
        .into_response(),
        Err(error) => {
            tracing::error!(%error, repo_id = id, "graph endpoint: query failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SimilarQuery {
    limit: Option<i64>,
}

/// `GET /admin/repositories/{id}/symbols/{node_id}/similar` — symbols found by meaning. The query
/// vector is the symbol's own already-stored embedding, never text embedded at request time: the same
/// node always returns the same result, and this needs no new embeddings credential on control-plane.
pub async fn get_similar(
    caller: Caller,
    State(state): State<AppState>,
    Path((id, node_id)): Path<(i64, String)>,
    Query(q): Query<SimilarQuery>,
) -> Response {
    if let Err(e) = caller.require("repo:read") {
        return e.into_response();
    }
    let Some(pool) = state.db.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
    };
    let Some(neo4j) = state.neo4j.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "neo4j not configured").into_response();
    };
    let repo = match crate::db::get_repository_by_id(pool, id).await {
        Ok(Some(repo)) => repo,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(GraphNotFound {
                    reason: "not_found",
                    message: "repository not found",
                }),
            )
                .into_response();
        }
        Err(error) => {
            tracing::error!(%error, repo_id = id, "similar endpoint: repository lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response();
        }
    };
    let commit = repo_commit_scope(&repo).to_string();
    let limit = clamp_graph_limit(q.limit);

    let (label, embedding) = match crate::integrations::neo4j::symbol_embedding(
        neo4j, id, &commit, &node_id,
    )
    .await
    {
        Ok(Some(pair)) => pair,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(GraphNotFound {
                    reason: "no_embedding",
                    message: "symbol has no stored embedding (not indexed, or no correlated chunk at index time)",
                }),
            )
                .into_response();
        }
        Err(error) => {
            tracing::error!(%error, repo_id = id, %node_id, "similar endpoint: embedding lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "query error").into_response();
        }
    };

    match crate::integrations::neo4j::hybrid_symbol_search(
        neo4j,
        id,
        &commit,
        &label,
        &embedding,
        limit.max(20),
        limit,
    )
    .await
    {
        Ok(nodes) => {
            let edge_result =
                crate::integrations::neo4j::edges_among(neo4j, id, &commit, &nodes).await;
            let edges = edge_result.unwrap_or_default();
            Json(GraphResponse {
                commit,
                nodes,
                edges,
            })
            .into_response()
        }
        Err(error) => {
            tracing::error!(%error, repo_id = id, %node_id, "similar endpoint: search failed");
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

    fn caller_with(permissions: &[&str]) -> Caller {
        Caller {
            claims: Claims {
                sub: "admin-1".to_string(),
                email: None,
                preferred_username: Some("admin-1".to_string()),
                name: None,
                exp: 9_999_999_999,
                extra: serde_json::Map::new(),
            },
            permissions: permissions.iter().map(|p| p.to_string()).collect(),
        }
    }

    /// AppState with only GitLab configured, pointed at a wiremock server — mirrors
    /// `webhook::tests::gitlab_only_state`, but built fresh here since `admin.rs` has no existing test
    /// module to share one with, and story #500's endpoint only needs GitLab exercised end-to-end (the
    /// GitHub/GitLab/Bitbucket `update_repo_file` implementations are each their own concern, covered
    /// by story #495's precedent for the read side — this proves the ADMIN ENDPOINT's own logic:
    /// permission gate, lookup, read-modify-write, platform dispatch).
    fn gitlab_only_state(pool: sqlx::PgPool, mock_uri: &str) -> AppState {
        let section = crate::config::GitlabSection {
            enabled: true,
            default_api_url: Some("https://gitlab.example.com/api/v4".to_string()),
            default_bot_handle: Some("lightbridge-bot".to_string()),
            projects: vec![crate::config::GitlabProjectConfig {
                project_id: 3001,
                installation_id: None,
                api_url: Some(format!("{mock_uri}/api/v4")),
                access_token: "token-admin-preset-test".to_string(),
                webhook_secret: "admin-preset-secret".to_string(),
                bot_handle: None,
            }],
        };
        let registry = crate::integrations::gitlab::GitlabRegistry::from_config(&section)
            .expect("valid config")
            .expect("enabled registry");
        AppState {
            github_webhook_secret: std::sync::Arc::new(String::new()),
            seen_deliveries: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
            jwt: None,
            db: Some(pool),
            allow_no_db: true,
            github: None,
            gitlab: Some(registry),
            bitbucket: None,
            platforms: std::collections::HashMap::new(),
            runner_token_signer: None,
            neo4j: None,
            metrics: crate::http::metrics::install(),
            review: std::sync::Arc::new(crate::config::ReviewSection::default()),
            knowledge_tools: std::sync::Arc::new(crate::config::KnowledgeToolsSection::default()),
            app_handle: std::sync::Arc::new("lightbridge-assistant".to_string()),
            permissions_claim: std::sync::Arc::new("permissions".to_string()),
            model_allowlist: std::sync::Arc::new(Vec::new()),
            mcp_public_url: None,
        }
    }

    // --- Per-repo settings overrides (epic #566) ---

    /// The override endpoint writes the DB layer, which BEATS the repo file — so it must be gated on
    /// `repo:configure`, not merely `repo:read`.
    #[sqlx::test]
    async fn set_settings_override_without_permission_is_forbidden(pool: sqlx::PgPool) {
        let repo_id = insert_gitlab_repo(&pool).await;
        let state = gitlab_only_state(pool, "http://unused");
        let response = set_settings_override(
            caller_with(&["repo:read"]),
            State(state),
            Path(repo_id),
            Json(SetSettingsBody {
                check_run_reporting: Some(Some(false)),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn set_settings_override_rejects_an_empty_body(pool: sqlx::PgPool) {
        let repo_id = insert_gitlab_repo(&pool).await;
        let state = gitlab_only_state(pool, "http://unused");
        let response = set_settings_override(
            caller_with(&["repo:configure"]),
            State(state),
            Path(repo_id),
            Json(SetSettingsBody::default()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// A typo must come back as a self-explanatory 400, not a CHECK-constraint 500.
    #[sqlx::test]
    async fn set_settings_override_rejects_unknown_enum_values(pool: sqlx::PgPool) {
        let repo_id = insert_gitlab_repo(&pool).await;
        let state = gitlab_only_state(pool.clone(), "http://unused");
        let response = set_settings_override(
            caller_with(&["repo:configure"]),
            State(state),
            Path(repo_id),
            Json(SetSettingsBody {
                push_strategy: Some(Some("supercede".to_string())),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let state = gitlab_only_state(pool.clone(), "http://unused");
        let response = set_settings_override(
            caller_with(&["repo:configure"]),
            State(state),
            Path(repo_id),
            Json(SetSettingsBody {
                push_debounce_seconds: Some(Some(99_999)),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // Nothing may have been written by either rejected call.
        assert!(
            crate::db::get_repo_settings(&pool, repo_id)
                .await
                .unwrap()
                .is_none(),
            "a rejected override must not write a row"
        );
    }

    #[sqlx::test]
    async fn set_settings_override_unknown_repo_is_not_found(pool: sqlx::PgPool) {
        let state = gitlab_only_state(pool, "http://unused");
        let response = set_settings_override(
            caller_with(&["repo:configure"]),
            State(state),
            Path(999_999),
            Json(SetSettingsBody {
                review_on_push: Some(Some(true)),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// The three-way body semantics: absent leaves a stored value alone, explicit null clears it.
    #[sqlx::test]
    async fn override_absent_field_is_left_alone_and_explicit_null_clears(pool: sqlx::PgPool) {
        let repo_id = insert_gitlab_repo(&pool).await;

        // Set two fields.
        let state = gitlab_only_state(pool.clone(), "http://unused");
        let response = set_settings_override(
            caller_with(&["repo:configure"]),
            State(state),
            Path(repo_id),
            Json(SetSettingsBody {
                check_run_reporting: Some(Some(false)),
                review_on_push: Some(Some(true)),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        // Touch only one of them; the other must survive untouched.
        let state = gitlab_only_state(pool.clone(), "http://unused");
        set_settings_override(
            caller_with(&["repo:configure"]),
            State(state),
            Path(repo_id),
            Json(SetSettingsBody {
                review_on_push: Some(Some(false)),
                ..Default::default()
            }),
        )
        .await;
        let row = crate::db::get_repo_settings(&pool, repo_id)
            .await
            .unwrap()
            .expect("row");
        assert_eq!(
            row.check_run_reporting,
            Some(false),
            "an ABSENT field must not clobber the stored value"
        );
        assert_eq!(row.review_on_push, Some(false));

        // An explicit null clears just that field.
        let state = gitlab_only_state(pool.clone(), "http://unused");
        set_settings_override(
            caller_with(&["repo:configure"]),
            State(state),
            Path(repo_id),
            Json(SetSettingsBody {
                check_run_reporting: Some(None),
                ..Default::default()
            }),
        )
        .await;
        let row = crate::db::get_repo_settings(&pool, repo_id)
            .await
            .unwrap()
            .expect("row");
        assert_eq!(
            row.check_run_reporting, None,
            "an explicit null must clear the override back to file/default"
        );
        assert_eq!(
            row.review_on_push,
            Some(false),
            "clearing one field must not disturb another"
        );
    }

    /// `Option<Option<T>>` + the custom deserializer is what makes absent-vs-null distinguishable;
    /// plain `#[serde(default)]` collapses both to `None`.
    #[test]
    fn settings_body_distinguishes_absent_from_explicit_null() {
        let absent: SetSettingsBody = serde_json::from_str("{}").unwrap();
        assert!(absent.check_run_reporting.is_none(), "absent = leave alone");

        let null: SetSettingsBody =
            serde_json::from_str(r#"{"check_run_reporting": null}"#).unwrap();
        assert_eq!(
            null.check_run_reporting,
            Some(None),
            "explicit null = clear the override"
        );

        let set: SetSettingsBody =
            serde_json::from_str(r#"{"check_run_reporting": true}"#).unwrap();
        assert_eq!(set.check_run_reporting, Some(Some(true)));
    }

    /// The read endpoint reports which layer produced each value — the thing that makes a
    /// three-layer system debuggable.
    #[sqlx::test]
    async fn get_settings_reports_defaults_then_the_db_layer(pool: sqlx::PgPool) {
        let repo_id = insert_gitlab_repo(&pool).await;

        let state = gitlab_only_state(pool.clone(), "http://unused");
        let response = get_settings(caller_with(&["repo:read"]), State(state), Path(repo_id)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["settings"]["check_run_reporting"]["value"], true);
        assert_eq!(json["settings"]["check_run_reporting"]["source"], "default");

        crate::db::set_repo_settings(
            &pool,
            repo_id,
            &crate::db::RepoSettingsPatch {
                check_run_reporting: Some(Some(false)),
                ..Default::default()
            },
            "tester",
        )
        .await
        .unwrap();

        let state = gitlab_only_state(pool, "http://unused");
        let response = get_settings(caller_with(&["repo:read"]), State(state), Path(repo_id)).await;
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["settings"]["check_run_reporting"]["value"], false);
        assert_eq!(
            json["settings"]["check_run_reporting"]["source"], "db",
            "provenance must name the layer that won"
        );
    }

    async fn insert_gitlab_repo(pool: &sqlx::PgPool) -> i64 {
        let repo_id = crate::db::upsert_repository(
            pool,
            Platform::GitLab,
            3001,
            "acme",
            "widgets",
            "main",
            Some(3001),
        )
        .await
        .unwrap();
        sqlx::query("UPDATE repositories SET status = 'approved' WHERE id = $1")
            .bind(repo_id)
            .execute(pool)
            .await
            .unwrap();
        repo_id
    }

    /// GitLab's `update_repo_file` resolves the default branch first (needed explicitly in the write
    /// body, unlike GitHub) — every test that reaches the write path needs this mocked too.
    async fn mount_default_branch(mock: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v4/projects/acme%2Fwidgets"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "default_branch": "main" })),
            )
            .mount(mock)
            .await;
    }

    #[sqlx::test]
    async fn get_preset_requires_repo_read_permission(pool: sqlx::PgPool) {
        let repo_id = insert_gitlab_repo(&pool).await;
        let state = gitlab_only_state(pool, "http://unused");
        let response = get_preset(caller_with(&[]), State(state), Path(repo_id)).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn get_preset_returns_the_configured_preset(pool: sqlx::PgPool) {
        let repo_id = insert_gitlab_repo(&pool).await;
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/api/v4/projects/acme%2Fwidgets/repository/files/.lightbridge-code-review.jsonc/raw",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_string(r#"{"preset": "ultra", "entry_points": {"pr_open": "fast"}}"#),
            )
            .mount(&mock)
            .await;

        let state = gitlab_only_state(pool, &mock.uri());
        let response = get_preset(caller_with(&["repo:read"]), State(state), Path(repo_id)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["preset"], "ultra");
        assert_eq!(json["entry_points"]["pr_open"], "fast");
    }

    // No repo config file at all — degrades to null/empty, never an error (matches preset.rs's own
    // "never fails, degrades to the platform default" trust posture).
    #[sqlx::test]
    async fn get_preset_returns_null_when_no_config_file_exists(pool: sqlx::PgPool) {
        let repo_id = insert_gitlab_repo(&pool).await;
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/api/v4/projects/acme%2Fwidgets/repository/files/.lightbridge-code-review.jsonc/raw",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&mock)
            .await;

        let state = gitlab_only_state(pool, &mock.uri());
        let response = get_preset(caller_with(&["repo:read"]), State(state), Path(repo_id)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["preset"].is_null());
    }

    #[sqlx::test]
    async fn set_preset_requires_repo_configure_permission(pool: sqlx::PgPool) {
        let repo_id = insert_gitlab_repo(&pool).await;
        let state = gitlab_only_state(pool, "http://unused");
        let response = set_preset(
            caller_with(&["repo:read"]),
            State(state),
            Path(repo_id),
            Json(SetPresetBody {
                preset: Some("ultra".to_string()),
                entry_points: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn set_preset_rejects_an_empty_body(pool: sqlx::PgPool) {
        let repo_id = insert_gitlab_repo(&pool).await;
        let state = gitlab_only_state(pool, "http://unused");
        let response = set_preset(
            caller_with(&["repo:configure"]),
            State(state),
            Path(repo_id),
            Json(SetPresetBody {
                preset: None,
                entry_points: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn set_preset_returns_not_found_for_an_unknown_repo(pool: sqlx::PgPool) {
        let state = gitlab_only_state(pool, "http://unused");
        let response = set_preset(
            caller_with(&["repo:configure"]),
            State(state),
            Path(999_999),
            Json(SetPresetBody {
                preset: Some("ultra".to_string()),
                entry_points: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // End-to-end proof of the read-modify-write: an existing file with an unrelated field
    // (`conventions`) keeps that field, and `preset` is set/overwritten — proving ADR-0109's "other
    // author-set fields are preserved" property, not just that the write call happens.
    #[sqlx::test]
    async fn set_preset_preserves_other_fields_and_commits_the_new_preset(pool: sqlx::PgPool) {
        let repo_id = insert_gitlab_repo(&pool).await;
        let mock = wiremock::MockServer::start().await;
        // `update_repo_file`'s GitLab implementation resolves the default branch first (GitLab, unlike
        // GitHub, needs the target branch explicit in the write body), then reads content + a
        // `last_commit_id` via the METADATA endpoint (not `/raw` — the closure-based `update_repo_file`
        // reads and mutates as one call, see the trait doc for why this is load-bearing, not stylistic).
        mount_default_branch(&mock).await;
        let encoded_current = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .encode(r#"{"preset": "fast", "conventions": ["use tabs"]}"#)
        };
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/api/v4/projects/acme%2Fwidgets/repository/files/.lightbridge-code-review.jsonc",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": encoded_current,
                "last_commit_id": "abc123",
            })))
            // Exactly one read: content and the concurrency token (`last_commit_id`) MUST come from
            // the same snapshot, never two separate round-trips (the TOCTOU gap this fix closes).
            .expect(1)
            .mount(&mock)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/api/v4/projects/acme%2Fwidgets/repository/files/.lightbridge-code-review.jsonc",
            ))
            // Body-content assertions live in `merge_preset_fields_preserves_other_fields` below (a
            // plain unit test, no wiremock) — this mock only proves the HTTP round-trip completes.
            .and(wiremock::matchers::body_string_contains("\"branch\":\"main\""))
            .and(wiremock::matchers::body_string_contains(
                "\"last_commit_id\":\"abc123\"",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock)
            .await;

        let state = gitlab_only_state(pool, &mock.uri());
        let response = set_preset(
            caller_with(&["repo:configure"]),
            State(state),
            Path(repo_id),
            Json(SetPresetBody {
                preset: Some("ultra".to_string()),
                entry_points: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        mock.verify().await;
    }

    // Regression for the P1 the lightbridge-assistant review caught: a concurrent author edit between
    // set_preset's read and its write must surface as a conflict, never a silent overwrite. Simulates
    // GitLab rejecting a stale `last_commit_id` (its documented optimistic-concurrency behavior) —
    // proving the admin response is an error, not the false `200 OK` a caller-side split read+write
    // would have produced by unconditionally overwriting whatever the concurrent editor committed.
    #[sqlx::test]
    async fn set_preset_surfaces_a_concurrent_edit_as_an_error_not_a_silent_overwrite(
        pool: sqlx::PgPool,
    ) {
        let repo_id = insert_gitlab_repo(&pool).await;
        let mock = wiremock::MockServer::start().await;
        mount_default_branch(&mock).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/api/v4/projects/acme%2Fwidgets/repository/files/.lightbridge-code-review.jsonc",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "content": "e30=", // "{}"
                    "last_commit_id": "stale-commit",
                })),
            )
            .mount(&mock)
            .await;
        // GitLab rejects a stale `last_commit_id` with 400 — the platform's own conflict signal.
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/api/v4/projects/acme%2Fwidgets/repository/files/.lightbridge-code-review.jsonc",
            ))
            .and(wiremock::matchers::body_string_contains(
                "\"last_commit_id\":\"stale-commit\"",
            ))
            .respond_with(wiremock::ResponseTemplate::new(400))
            .mount(&mock)
            .await;

        let state = gitlab_only_state(pool, &mock.uri());
        let response = set_preset(
            caller_with(&["repo:configure"]),
            State(state),
            Path(repo_id),
            Json(SetPresetBody {
                preset: Some("ultra".to_string()),
                entry_points: None,
            }),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_GATEWAY,
            "a conflicting concurrent edit must surface as an error, never a silent 200 overwrite"
        );
    }

    // Pure test of the merge logic itself (no platform round-trip) — proves ADR-0109's "other
    // author-set fields are preserved" property precisely, including the exact preset value written.
    #[test]
    fn merge_preset_fields_preserves_other_fields() {
        let body = SetPresetBody {
            preset: Some("ultra".to_string()),
            entry_points: None,
        };
        let merged = merge_preset_fields(
            Some(r#"{"preset": "fast", "conventions": ["use tabs"]}"#),
            &body,
        );
        let parsed: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(parsed["preset"], "ultra");
        assert_eq!(parsed["conventions"], serde_json::json!(["use tabs"]));
    }

    #[test]
    fn merge_preset_fields_sets_entry_points_alongside_an_untouched_preset() {
        let body = SetPresetBody {
            preset: None,
            entry_points: Some(std::collections::HashMap::from([(
                "pr_open".to_string(),
                "fast".to_string(),
            )])),
        };
        let merged = merge_preset_fields(Some(r#"{"preset": "deep"}"#), &body);
        let parsed: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(
            parsed["preset"], "deep",
            "untouched when body.preset is None"
        );
        assert_eq!(parsed["entry_points"]["pr_open"], "fast");
    }

    #[test]
    fn merge_preset_fields_starts_fresh_when_no_file_exists_or_it_is_malformed() {
        let body = SetPresetBody {
            preset: Some("deep".to_string()),
            entry_points: None,
        };
        for current in [None, Some("not json at all")] {
            let merged = merge_preset_fields(current, &body);
            let parsed: serde_json::Value = serde_json::from_str(&merged).unwrap();
            assert_eq!(parsed["preset"], "deep");
            assert!(parsed.as_object().unwrap().len() == 1, "{parsed}");
        }
    }

    // No repo config exists yet — GET 404s, so the write starts from an empty object (create case,
    // GitLab's PUT-then-POST-on-404 fallback inside `update_repo_file` itself).
    #[sqlx::test]
    async fn set_preset_creates_the_file_when_none_exists_yet(pool: sqlx::PgPool) {
        let repo_id = insert_gitlab_repo(&pool).await;
        let mock = wiremock::MockServer::start().await;
        mount_default_branch(&mock).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/api/v4/projects/acme%2Fwidgets/repository/files/.lightbridge-code-review.jsonc",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&mock)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/api/v4/projects/acme%2Fwidgets/repository/files/.lightbridge-code-review.jsonc",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&mock)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/api/v4/projects/acme%2Fwidgets/repository/files/.lightbridge-code-review.jsonc",
            ))
            .respond_with(wiremock::ResponseTemplate::new(201))
            .mount(&mock)
            .await;

        let state = gitlab_only_state(pool, &mock.uri());
        let response = set_preset(
            caller_with(&["repo:configure"]),
            State(state),
            Path(repo_id),
            Json(SetPresetBody {
                preset: Some("deep".to_string()),
                entry_points: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[sqlx::test]
    async fn set_preset_surfaces_a_platform_write_failure_as_bad_gateway(pool: sqlx::PgPool) {
        let repo_id = insert_gitlab_repo(&pool).await;
        let mock = wiremock::MockServer::start().await;
        mount_default_branch(&mock).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/api/v4/projects/acme%2Fwidgets/repository/files/.lightbridge-code-review.jsonc",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&mock)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/api/v4/projects/acme%2Fwidgets/repository/files/.lightbridge-code-review.jsonc",
            ))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let state = gitlab_only_state(pool, &mock.uri());
        let response = set_preset(
            caller_with(&["repo:configure"]),
            State(state),
            Path(repo_id),
            Json(SetPresetBody {
                preset: Some("deep".to_string()),
                entry_points: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

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
            mcp_public_url: None,
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
    async fn get_repo_model_reports_none_when_nothing_is_overridden(pool: PgPool) {
        let repo_id = crate::db::upsert_repository(
            &pool,
            Platform::GitHub,
            10,
            "octo",
            "repo10",
            "main",
            None,
        )
        .await
        .unwrap();
        let state = test_state(pool, vec!["claude-opus-5".to_string()]);
        let response = get_repo_model(caller(&["repo:read"]), State(state), Path(repo_id)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["repository_id"], repo_id);
        assert_eq!(body["model"], serde_json::Value::Null);
        assert_eq!(body["source"], "none");
    }

    #[sqlx::test]
    async fn get_repo_model_reports_the_repo_override_when_set(pool: PgPool) {
        let repo_id = crate::db::upsert_repository(
            &pool,
            Platform::GitHub,
            11,
            "octo",
            "repo11",
            "main",
            None,
        )
        .await
        .unwrap();
        crate::db::set_repo_model_override(&pool, repo_id, "claude-opus-5", "tester")
            .await
            .unwrap();
        let state = test_state(pool, vec!["claude-opus-5".to_string()]);
        let response = get_repo_model(caller(&["repo:read"]), State(state), Path(repo_id)).await;
        let body = body_json(response).await;
        assert_eq!(body["model"], "claude-opus-5");
        assert_eq!(body["source"], "repo");
    }

    /// The repo lookup itself is silent on a missing repo override — it must fall through to the
    /// repo's `installation_id` and check the org layer, matching
    /// `crate::model::resolve_model_override`'s own precedence exactly, not a separate/divergent read.
    #[sqlx::test]
    async fn get_repo_model_falls_through_to_the_org_override(pool: PgPool) {
        let repo_id = crate::db::upsert_repository(
            &pool,
            Platform::GitHub,
            12,
            "octo",
            "repo12",
            "main",
            Some(4242),
        )
        .await
        .unwrap();
        crate::db::set_org_model_override(&pool, 4242, "gpt-5", "tester")
            .await
            .unwrap();
        let state = test_state(pool, vec!["gpt-5".to_string()]);
        let response = get_repo_model(caller(&["repo:read"]), State(state), Path(repo_id)).await;
        let body = body_json(response).await;
        assert_eq!(body["model"], "gpt-5");
        assert_eq!(body["source"], "org");
    }

    #[sqlx::test]
    async fn get_repo_model_without_permission_is_forbidden(pool: PgPool) {
        let repo_id = crate::db::upsert_repository(
            &pool,
            Platform::GitHub,
            13,
            "octo",
            "repo13",
            "main",
            None,
        )
        .await
        .unwrap();
        let state = test_state(pool, vec!["claude-opus-5".to_string()]);
        let response = get_repo_model(caller(&[]), State(state), Path(repo_id)).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn get_org_model_reports_none_when_nothing_is_overridden(pool: PgPool) {
        let state = test_state(pool, vec!["claude-opus-5".to_string()]);
        let response = get_org_model(caller(&["repo:read"]), State(state), Path(888)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["installation_id"], 888);
        assert_eq!(body["model"], serde_json::Value::Null);
        assert_eq!(body["source"], "none");
    }

    #[sqlx::test]
    async fn get_org_model_reports_the_override_when_set(pool: PgPool) {
        crate::db::set_org_model_override(&pool, 999, "claude-opus-5", "tester")
            .await
            .unwrap();
        let state = test_state(pool, vec!["claude-opus-5".to_string()]);
        let response = get_org_model(caller(&["repo:read"]), State(state), Path(999)).await;
        let body = body_json(response).await;
        assert_eq!(body["model"], "claude-opus-5");
        assert_eq!(body["source"], "org");
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
