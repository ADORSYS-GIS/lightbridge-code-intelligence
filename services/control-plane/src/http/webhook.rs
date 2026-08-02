//! Unified webhook receiver (GitHub + GitLab + Bitbucket).
//!
//! A single `/webhook` route detects the platform from headers, verifies the signature, dedupes
//! on the platform's delivery ID, then hands off to platform-specific event routing. With a
//! database, dedup + persistence happen atomically via the `webhook_deliveries` PRIMARY KEY;
//! without one (dev) it falls back to an in-memory set.
//!
//! Bitbucket goes on this same route via header detection, exactly like GitHub/GitLab — no
//! separate path-scoped route (that's ADR-0109's domain-unification scope, a separate epic, not
//! adopted here).
//!
//! The legacy `/github/webhook` route is kept as an alias during the transition and forwards to
//! the same unified handler.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use hmac::{Hmac, KeyInit, Mac};
use lci_agent_step::{Passthrough, StepError, StepRuntime};
use lci_agent_types::StepName;
use sha2::Sha256;
use tracing::Instrument;

use crate::AppState;
use crate::integrations::platform::{CodePlatform, Platform, RepoRef};

type HmacSha256 = Hmac<Sha256>;

/// Maximum webhook body size before HMAC / JSON verification. GitLab must parse JSON pre-auth to
/// read `project.id` for per-project secret selection; this caps attacker-controlled parse cost.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Constant-time HMAC-SHA256 verification of a GitHub webhook signature against a raw secret —
/// the same algorithm `GithubApp::verify_webhook` (integrations/github.rs) implements. Used as a
/// **fallback** when `GITHUB_WEBHOOK_SECRET` is configured but the GitHub App itself isn't (no
/// `GITHUB_APP_ID`/`GITHUB_APP_PRIVATE_KEY`, so `state.platforms` has no GitHub entry): signature
/// verification must never require App credentials — it never did before #504's CodePlatform
/// wiring (a P2 caught in review: verifying only via the registered App would 503 every webhook
/// in a secret-only deployment that used to work fine).
fn verify_signature(secret: &[u8], body: &[u8], signature: &str) -> bool {
    if secret.is_empty() {
        return false;
    }
    let Ok(mut mac) = HmacSha256::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    let expected = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    use subtle::ConstantTimeEq;
    expected.as_bytes().ct_eq(signature.as_bytes()).into()
}

/// `POST /webhook/github` — GitHub webhook receiver.
///
/// This is the ROOT span of the webhook→task→Job→turns→egress trace (ticket #246).
pub async fn github_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let span = tracing::info_span!(
        "webhook.receive",
        platform = "github",
        event = tracing::field::Empty,
        delivery_id = tracing::field::Empty,
    );
    github_webhook_body(state, headers, body)
        .instrument(span)
        .await
}

async fn github_webhook_body(state: AppState, headers: HeaderMap, body: Bytes) -> Response {
    let verified = if let Some(github) = state.platforms.get(&Platform::GitHub) {
        github.verify_webhook(&headers, &body)
    } else if !state.github_webhook_secret.is_empty() {
        verify_signature(
            state.github_webhook_secret.as_bytes(),
            &body,
            &header(&headers, "x-hub-signature-256"),
        )
    } else {
        tracing::error!("no github platform or webhook secret configured");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "github platform not configured",
        )
            .into_response();
    };
    if !verified {
        crate::http::metrics::webhook_signature_failure("github");
        tracing::warn!(platform = "github", "invalid webhook signature");
        return (StatusCode::UNAUTHORIZED, "invalid signature").into_response();
    }
    let delivery_id = header(&headers, "x-github-delivery");
    if delivery_id.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing delivery id").into_response();
    }
    let event = header(&headers, "x-github-event");
    tracing::Span::current()
        .record("delivery_id", &delivery_id)
        .record("event", &event);
    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(error) => {
            tracing::error!(%error, delivery_id, "github webhook: invalid json payload");
            return (StatusCode::BAD_REQUEST, "invalid json payload").into_response();
        }
    };
    match persist_delivery(&state, Platform::GitHub, &delivery_id, &event, &payload).await {
        DeliveryResult::Persisted => {}
        DeliveryResult::Duplicate => {
            crate::http::metrics::webhook_duplicate("github");
            tracing::info!(delivery_id, "github: duplicate delivery");
            return (StatusCode::ACCEPTED, "duplicate delivery").into_response();
        }
        DeliveryResult::Error => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "persistence error").into_response();
        }
    }
    crate::http::metrics::webhook_delivery("github", &event);
    tracing::info!(delivery_id, %event, "github: accepted webhook");
    if state.db.is_some() {
        route_github_event(&state, &event, &payload, &delivery_id).await;
    }
    (StatusCode::ACCEPTED, "accepted").into_response()
}

/// `POST /webhook/gitlab/{installation_id}` — GitLab webhook receiver.
///
/// The path carries the installation (project) ID — no body pre-parse before verification.
/// `installation_id` must match the `installation_id` (or `project_id` when absent) configured
/// for the project in `control-plane.json`.
pub async fn gitlab_webhook(
    State(state): State<AppState>,
    Path(installation_id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let span = tracing::info_span!(
        "webhook.receive",
        platform = "gitlab",
        installation_id,
        event = tracing::field::Empty,
        delivery_id = tracing::field::Empty,
    );
    gitlab_webhook_body(state, installation_id, headers, body)
        .instrument(span)
        .await
}

async fn gitlab_webhook_body(
    state: AppState,
    installation_id: i64,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let payload =
        match verified_gitlab_payload_for_installation(&state, installation_id, &headers, &body) {
            Ok(p) => p,
            Err(GitlabPayloadError::InvalidJson) => {
                return (StatusCode::BAD_REQUEST, "invalid json payload").into_response();
            }
            Err(GitlabPayloadError::UnknownInstallationId) => {
                tracing::warn!(installation_id, "gitlab webhook: unknown installation_id");
                return (StatusCode::NOT_FOUND, "unknown installation").into_response();
            }
            Err(GitlabPayloadError::InvalidSignature) => {
                crate::http::metrics::webhook_signature_failure("gitlab");
                tracing::warn!(installation_id, "invalid gitlab webhook signature");
                return (StatusCode::UNAUTHORIZED, "invalid signature").into_response();
            }
        };
    let delivery_id = header(&headers, "x-gitlab-event-uuid");
    if delivery_id.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing delivery id").into_response();
    }
    let event = header(&headers, "x-gitlab-event");
    tracing::Span::current()
        .record("delivery_id", &delivery_id)
        .record("event", &event);
    match persist_delivery(&state, Platform::GitLab, &delivery_id, &event, &payload).await {
        DeliveryResult::Persisted => {}
        DeliveryResult::Duplicate => {
            crate::http::metrics::webhook_duplicate("gitlab");
            tracing::info!(delivery_id, "gitlab: duplicate delivery");
            return (StatusCode::ACCEPTED, "duplicate delivery").into_response();
        }
        DeliveryResult::Error => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "persistence error").into_response();
        }
    }
    crate::http::metrics::webhook_delivery("gitlab", &event);
    tracing::info!(delivery_id, %event, installation_id, "gitlab: accepted webhook");
    if state.db.is_some() {
        // Verify the payload's claimed project identity matches the path-resolved project.
        // This prevents a holder of project A's secret from driving project B's repo.
        let payload_project_id = payload["project"]["id"].as_i64();
        let path_project_id = state
            .gitlab
            .as_ref()
            .and_then(|r| r.get_by_installation_id(installation_id))
            .map(|p| p.project_id);
        if let (Some(path_id), Some(payload_id)) = (path_project_id, payload_project_id)
            && path_id != payload_id
        {
            tracing::warn!(
                installation_id,
                path_project_id = path_id,
                payload_project_id = payload_id,
                "gitlab webhook: payload project.id does not match path installation_id"
            );
            return (StatusCode::BAD_REQUEST, "project identity mismatch").into_response();
        }
        route_gitlab_event(&state, &event, &payload, &delivery_id).await;
    }
    (StatusCode::ACCEPTED, "accepted").into_response()
}

/// `POST /webhook/bitbucket/{installation_id}` — Bitbucket webhook receiver.
///
/// `installation_id` is `platform::stable_id_from_key("workspace/repo_slug")`.
/// See `docs/runbooks/bitbucket-platform-setup.md` for the URL format.
pub async fn bitbucket_webhook(
    State(state): State<AppState>,
    Path(installation_id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let span = tracing::info_span!(
        "webhook.receive",
        platform = "bitbucket",
        installation_id,
        event = tracing::field::Empty,
        delivery_id = tracing::field::Empty,
    );
    bitbucket_webhook_body(state, installation_id, headers, body)
        .instrument(span)
        .await
}

async fn bitbucket_webhook_body(
    state: AppState,
    installation_id: i64,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let payload =
        match verified_bitbucket_payload_for_installation(&state, installation_id, &headers, &body)
        {
            Ok(p) => p,
            Err(BitbucketPayloadError::InvalidJson) => {
                return (StatusCode::BAD_REQUEST, "invalid json payload").into_response();
            }
            Err(BitbucketPayloadError::InvalidSignature) => {
                crate::http::metrics::webhook_signature_failure("bitbucket");
                tracing::warn!(installation_id, "invalid bitbucket webhook signature");
                return (StatusCode::UNAUTHORIZED, "invalid signature").into_response();
            }
        };
    let delivery_id = header(&headers, "x-request-uuid");
    if delivery_id.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing delivery id").into_response();
    }
    let event = header(&headers, "x-event-key");
    tracing::Span::current()
        .record("delivery_id", &delivery_id)
        .record("event", &event);
    match persist_delivery(&state, Platform::Bitbucket, &delivery_id, &event, &payload).await {
        DeliveryResult::Persisted => {}
        DeliveryResult::Duplicate => {
            crate::http::metrics::webhook_duplicate("bitbucket");
            tracing::info!(delivery_id, "bitbucket: duplicate delivery");
            return (StatusCode::ACCEPTED, "duplicate delivery").into_response();
        }
        DeliveryResult::Error => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "persistence error").into_response();
        }
    }
    crate::http::metrics::webhook_delivery("bitbucket", &event);
    tracing::info!(delivery_id, %event, installation_id, "bitbucket: accepted webhook");
    if state.db.is_some() {
        // Verify the payload's claimed repo identity matches the path installation_id.
        if let Some((full_name, _, _)) = bitbucket_repo_identity(&payload) {
            let payload_id = crate::integrations::platform::stable_id_from_key(&full_name);
            if payload_id != installation_id {
                tracing::warn!(
                    installation_id,
                    payload_id,
                    %full_name,
                    "bitbucket webhook: payload repo does not match path installation_id"
                );
                return (StatusCode::BAD_REQUEST, "repo identity mismatch").into_response();
            }
        }
        route_bitbucket_event(&state, &event, &payload, &delivery_id).await;
    }
    (StatusCode::ACCEPTED, "accepted").into_response()
}

enum DeliveryResult {
    Persisted,
    Duplicate,
    Error,
}

async fn persist_delivery(
    state: &AppState,
    platform: Platform,
    delivery_id: &str,
    event: &str,
    payload: &serde_json::Value,
) -> DeliveryResult {
    match &state.db {
        Some(pool) => {
            let step_name = StepName::from(format!("webhook:{delivery_id}"));
            let step_result = Passthrough
                .step(step_name, async || {
                    crate::db::record_delivery(pool, platform, delivery_id, event, payload)
                        .await
                        .map_err(|e| StepError::terminal(e.to_string()))
                })
                .await;
            match step_result {
                Ok(true) => DeliveryResult::Persisted,
                Ok(false) => DeliveryResult::Duplicate,
                Err(step_error) => {
                    let error = match step_error {
                        StepError::Terminal { reason } => reason,
                        StepError::Transient { source, .. } => source.to_string(),
                    };
                    tracing::error!(%error, delivery_id, "failed to persist delivery");
                    DeliveryResult::Error
                }
            }
        }
        None => {
            let is_new = state
                .seen_deliveries
                .lock()
                .expect("dedup lock poisoned")
                .insert(delivery_id.to_string());
            if is_new {
                DeliveryResult::Persisted
            } else {
                DeliveryResult::Duplicate
            }
        }
    }
}

enum GitlabPayloadError {
    InvalidJson,
    UnknownInstallationId,
    InvalidSignature,
}

fn verified_gitlab_payload_for_installation(
    state: &AppState,
    installation_id: i64,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<serde_json::Value, GitlabPayloadError> {
    // Verify against raw bytes first — the path carries installation_id so we no longer
    // need to parse the body to find the project secret.
    match verify_gitlab_webhook_with_registry(state.gitlab.as_ref(), headers, body, installation_id)
    {
        GitlabVerifyResult::Ok => {}
        GitlabVerifyResult::UnknownInstallationId => {
            return Err(GitlabPayloadError::UnknownInstallationId);
        }
        GitlabVerifyResult::InvalidSignature => {
            return Err(GitlabPayloadError::InvalidSignature);
        }
    }
    match serde_json::from_slice(body) {
        Ok(p) => Ok(p),
        Err(error) => {
            tracing::error!(%error, installation_id, "gitlab webhook: invalid json payload");
            Err(GitlabPayloadError::InvalidJson)
        }
    }
}
enum GitlabVerifyResult {
    Ok,
    UnknownInstallationId,
    InvalidSignature,
}

fn verify_gitlab_webhook_with_registry(
    registry: Option<&crate::integrations::gitlab::GitlabRegistry>,
    headers: &HeaderMap,
    body: &[u8],
    installation_id: i64,
) -> GitlabVerifyResult {
    let Some(registry) = registry else {
        tracing::warn!(
            installation_id,
            "GitLab webhook received but GitLab is not configured"
        );
        return GitlabVerifyResult::UnknownInstallationId;
    };
    let Some(project) = registry.get_by_installation_id(installation_id) else {
        tracing::warn!(
            installation_id,
            "GitLab webhook for unconfigured installation_id"
        );
        return GitlabVerifyResult::UnknownInstallationId;
    };
    if project.client.verify_webhook(headers, body) {
        GitlabVerifyResult::Ok
    } else {
        GitlabVerifyResult::InvalidSignature
    }
}

enum BitbucketPayloadError {
    InvalidJson,
    InvalidSignature,
}

fn verified_bitbucket_payload_for_installation(
    state: &AppState,
    installation_id: i64,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<serde_json::Value, BitbucketPayloadError> {
    // Verify against raw bytes first — path carries installation_id.
    if !verify_bitbucket_project_webhook_with_registry(
        state.bitbucket.as_ref(),
        headers,
        body,
        installation_id,
    ) {
        return Err(BitbucketPayloadError::InvalidSignature);
    }
    match serde_json::from_slice(body) {
        Ok(p) => Ok(p),
        Err(error) => {
            tracing::error!(%error, installation_id, "bitbucket webhook: invalid json payload");
            Err(BitbucketPayloadError::InvalidJson)
        }
    }
}
fn verify_bitbucket_project_webhook_with_registry(
    registry: Option<&crate::integrations::bitbucket::BitbucketRegistry>,
    headers: &HeaderMap,
    body: &[u8],
    installation_id: i64,
) -> bool {
    let Some(registry) = registry else {
        tracing::warn!(
            installation_id,
            "Bitbucket webhook received but Bitbucket is not configured"
        );
        return false;
    };
    let Some(repo) = registry.get(installation_id) else {
        tracing::warn!(
            installation_id,
            "Bitbucket webhook for unconfigured installation_id"
        );
        return false;
    };
    repo.client.verify_webhook(headers, body)
}

/// GitHub webhook → internal action mapping (the only events that do anything beyond being
/// persisted):
///
///   pull_request               opened                  → review task (the automatic FIRST review)
///   pull_request               closed                  → cancel the PR's active tasks
///   pull_request               synchronize | reopened  → nothing (re-review via @mention)
///   push                       to the default branch    → re-index task (keep the base index fresh)
///   issue_comment              created, body @<handle> → task: PR re-review, or an issue answer
///   installation               created                 → register the installed repos as pending
///   installation               deleted                 → disable the installation's repos
///   installation_repositories  added | removed         → register pending / disable those repos
///
/// Repos start **pending** and need admin approval before any review/index runs (Epic #75).
/// Everything else is persisted to `webhook_deliveries` only.
async fn route_github_event(
    state: &AppState,
    event: &str,
    payload: &serde_json::Value,
    delivery_id: &str,
) {
    match event {
        "pull_request" => handle_pull_request(state, payload, delivery_id).await,
        "push" => handle_push(state, payload, delivery_id).await,
        "issue_comment" => handle_issue_comment(state, payload, delivery_id).await,
        "installation" => handle_installation(state, payload, delivery_id).await,
        "installation_repositories" => {
            handle_installation_repositories(state, payload, delivery_id).await
        }
        _ => {}
    }
}

/// GitLab webhook → internal action mapping:
///
///   Merge Request Hook   open    → review task (the automatic FIRST review)
///   Merge Request Hook   close   → cancel the MR's active tasks
///   Merge Request Hook   update/reopen/merge → nothing (re-review via @mention)
///   Push Hook             to the default branch → re-index task
///   Note Hook             created, body @<handle> → task: MR re-review, or an issue answer
///
/// GitLab has no installation events — repos are registered as pending via the admin console
/// (manual approval, same as GitHub's approval gate Epic #75).
async fn route_gitlab_event(
    state: &AppState,
    event: &str,
    payload: &serde_json::Value,
    delivery_id: &str,
) {
    match event {
        "Merge Request Hook" => handle_gitlab_merge_request(state, payload, delivery_id).await,
        "Push Hook" => handle_gitlab_push(state, payload, delivery_id).await,
        "Note Hook" => handle_gitlab_note(state, payload, delivery_id).await,
        _ => {
            tracing::debug!(%delivery_id, %event, "GitLab event type not handled; persisted only");
        }
    }
}

/// Bitbucket Cloud webhook → internal action mapping (`X-Event-Key` values):
///
///   pullrequest:created          → review task (the automatic FIRST review)
///   pullrequest:fulfilled        → cancel the PR's active tasks (merged)
///   pullrequest:rejected         → cancel the PR's active tasks (declined)
///   pullrequest:updated          → nothing (re-review via @mention)
///   pullrequest:comment_created  → task: PR re-review, or an issue answer (@<handle> mention)
///   repo:push                    → re-index task when the default branch moves
///
/// Bitbucket has no installation events — repos are registered as pending via the admin console
/// (manual approval, same as GitHub/GitLab's approval gate, Epic #75).
async fn route_bitbucket_event(
    state: &AppState,
    event: &str,
    payload: &serde_json::Value,
    delivery_id: &str,
) {
    match event {
        "pullrequest:created" | "pullrequest:fulfilled" | "pullrequest:rejected" => {
            handle_bitbucket_pullrequest(state, event, payload, delivery_id).await
        }
        "repo:push" => handle_bitbucket_push(state, payload, delivery_id).await,
        "pullrequest:comment_created" => {
            handle_bitbucket_comment(state, payload, delivery_id).await
        }
        _ => {
            tracing::debug!(%delivery_id, %event, "Bitbucket event type not handled; persisted only");
        }
    }
}

/// Split a GitLab `path_with_namespace` (e.g. `group/subgroup/repo`) into `(owner, name)`.
/// `owner` is everything before the last `/`; `name` is the last segment. This way
/// `format!("{}/{}", owner, name)` reconstructs the full path.
fn gitlab_path_split(path_with_namespace: &str) -> Option<(&str, &str)> {
    let (owner, name) = path_with_namespace.rsplit_once('/')?;
    Some((owner, name))
}

/// Extract `(project_id, owner, name, default_branch)` from a GitLab webhook `project` object.
fn gitlab_project_identity(project: &serde_json::Value) -> Option<(i64, &str, &str, &str)> {
    let project_id = project["id"].as_i64()?;
    let path = project["path_with_namespace"].as_str()?;
    let default_branch = project["default_branch"].as_str().unwrap_or("main");
    let (owner, name) = gitlab_path_split(path)?;
    Some((project_id, owner, name, default_branch))
}

/// Extract `(full_name, workspace, repo_slug)` from a Bitbucket webhook `repository` object's
/// `full_name` field (`"workspace/repo_slug"`).
fn bitbucket_repo_identity(payload: &serde_json::Value) -> Option<(String, String, String)> {
    let full_name = payload["repository"]["full_name"].as_str()?.to_string();
    let (workspace, repo_slug) = full_name.rsplit_once('/')?;
    Some((
        full_name.clone(),
        workspace.to_string(),
        repo_slug.to_string(),
    ))
}

/// `Merge Request Hook`: `open` → the automatic first review. `close` → cancel the MR's active
/// tasks. Other actions (`update`, `reopen`, `merge`) do nothing — a re-review is requested with
/// an `@<handle>` note ([`handle_gitlab_note`]).
async fn handle_gitlab_merge_request(
    state: &crate::AppState,
    payload: &serde_json::Value,
    delivery_id: &str,
) {
    let Some(pool) = state.db.as_ref() else {
        return;
    };
    let attrs = &payload["object_attributes"];
    let action = attrs["action"].as_str().unwrap_or_default();
    if !matches!(action, "open" | "update" | "close") {
        return;
    }
    // Epic #566: GitLab sends `update` for label/title/description edits too, not only new commits —
    // `oldrev` is present only when the MR's head SHA actually moved, so it is the sole reliable
    // "new push" signal. A no-op `update` (metadata edit) is filtered out here, before any of the
    // approval/bot/draft checks below run.
    if action == "update" && attrs["oldrev"].as_str().is_none() {
        return;
    }
    let project = &payload["project"];
    let Some((project_id, owner, name, default_branch)) = gitlab_project_identity(project) else {
        tracing::warn!(
            delivery_id,
            "GitLab MR payload missing project fields; skipping"
        );
        return;
    };
    let Some(mr_iid) = attrs["iid"].as_i64() else {
        tracing::warn!(delivery_id, "GitLab MR payload missing iid; skipping");
        return;
    };
    // GitLab has no installation ID — use the project ID so the outbox/reconciler can look it up.
    let installation_id = project_id;
    let repository_id = match crate::db::upsert_repository(
        pool,
        Platform::GitLab,
        project_id,
        owner,
        name,
        default_branch,
        Some(installation_id),
    )
    .await
    {
        Ok(id) => id,
        Err(error) => {
            tracing::error!(%error, delivery_id, "GitLab: failed to upsert repository");
            return;
        }
    };

    match action {
        "open" | "update" => {
            let is_sync = action == "update";
            // Approval gate (Epic #75): a repo must be admin-approved before any review runs.
            if !approved_or_skip(pool, repository_id, delivery_id, mr_iid).await {
                return;
            }
            // Skip draft MRs (GitLab's equivalent of GitHub's draft PRs) — not ready for review.
            // Applies to both open and sync: a draft is, by construction, still being pushed to.
            if attrs["draft"].as_bool() == Some(true) {
                tracing::info!(
                    delivery_id,
                    mr = mr_iid,
                    repository_id,
                    is_sync,
                    "GitLab MR is draft; skipping automatic review"
                );
                return;
            }
            // RFC-0003: skip bot-authored MRs. GitLab bots typically have usernames ending in `_bot`.
            // Absent a clean `type` field, we fail open (treat as human) — never silently drop a real MR.
            // Check both the commit author's display name and the triggerer's username for robustness.
            let author = attrs["last_commit"]["author"]["name"]
                .as_str()
                .unwrap_or("");
            let trigger_username = payload["user"]["username"].as_str().unwrap_or("");
            if should_skip_gitlab_bot_review(state.review.skip_bot_authored_prs(), author)
                || should_skip_gitlab_bot_review(
                    state.review.skip_bot_authored_prs(),
                    trigger_username,
                )
            {
                tracing::info!(
                    delivery_id,
                    mr = mr_iid,
                    repository_id,
                    "GitLab MR author appears to be a bot; skipping automatic review"
                );
                crate::http::metrics::review_skipped_bot_author();
                return;
            }
            // GitLab MR webhook payload includes diff_refs (base_sha/head_sha/start_sha), but some
            // events (e.g. push-triggered MR updates, or payloads from older GitLab versions) omit
            // diff_refs. Fall back to the GitLab API to fetch the MR's diff refs so the agent-runner
            // gets a non-empty base_sha — without it, `clone::pr_diff` returns None and the review
            // runs on an empty diff.
            let mut base_sha = attrs["diff_refs"]["base_sha"].as_str().map(str::to_string);
            let mut head_sha = attrs["diff_refs"]["head_sha"]
                .as_str()
                .or_else(|| attrs["last_commit"]["id"].as_str())
                .map(str::to_string);
            if base_sha.is_none() {
                if let Some(gitlab) = state
                    .gitlab
                    .as_ref()
                    .and_then(|registry| registry.client_for_project(project_id))
                {
                    let repo_ref = RepoRef {
                        platform: Platform::GitLab,
                        full_name: format!("{owner}/{name}"),
                        platform_repo_id: project_id,
                        installation_id,
                    };
                    match gitlab.pr_shas(&repo_ref, mr_iid).await {
                        Ok((api_base, api_head)) => {
                            if base_sha.is_none() {
                                base_sha = api_base;
                            }
                            if head_sha.is_none() {
                                head_sha = api_head;
                            }
                            tracing::info!(
                                delivery_id,
                                mr = mr_iid,
                                "GitLab MR payload missing diff_refs; fetched SHAs from API"
                            );
                        }
                        Err(error) => {
                            tracing::warn!(
                                %error,
                                delivery_id,
                                mr = mr_iid,
                                "GitLab MR payload missing diff_refs and API fetch failed; \
                                 review may run on an empty diff"
                            );
                        }
                    }
                } else {
                    tracing::warn!(
                        delivery_id,
                        mr = mr_iid,
                        project_id,
                        "GitLab MR payload missing diff_refs and no GitLab project client configured; \
                         review may run on an empty diff"
                    );
                }
            }
            let repo_ref = RepoRef {
                platform: Platform::GitLab,
                full_name: format!("{owner}/{name}"),
                platform_repo_id: project_id,
                installation_id,
            };
            let entry = if is_sync {
                crate::preset::EntryPoint::PrSync
            } else {
                crate::preset::EntryPoint::PrOpen
            };
            let (preset, settings) = crate::settings::resolve_preset_and_settings(
                pool,
                state
                    .gitlab
                    .as_ref()
                    .and_then(|registry| registry.client_for_project(project_id))
                    .map(|client| client as &dyn CodePlatform),
                &repo_ref,
                base_sha.as_deref().unwrap_or(default_branch),
                entry,
                repository_id,
            )
            .await;
            // Epic #566: a repo can opt out of the automatic on-open review (stays @mention-only) and,
            // independently, opt IN to a review on later pushes (off by default — see settings.rs).
            // Checked here rather than at dispatch so an opted-out repo creates no task at all.
            let (setting, disabled_msg) = if is_sync {
                (
                    &settings.review_on_push,
                    "review-on-push disabled for this repo; skipping (mention-triggered reviews still run)",
                )
            } else {
                (
                    &settings.review_on_pr_open,
                    "automatic on-open review disabled for this repo; skipping (mention-triggered reviews still run)",
                )
            };
            if !setting.value {
                tracing::info!(delivery_id, repository_id, is_sync, source = ?setting.source, "{disabled_msg}");
                return;
            }
            let model_override =
                crate::model::resolve_model_override(pool, repository_id, installation_id).await;
            let task = crate::db::NewTask {
                repository_id,
                installation_id,
                webhook_delivery_id: delivery_id.to_string(),
                target_type: "pull_request".to_string(),
                target_id: mr_iid,
                command_text: "review".to_string(),
                base_sha,
                head_sha,
                run_epoch: 0,
                preset,
                entry_point: entry.as_str().to_string(),
                trigger_comment_id: None,
                trace_context: lci_observability::current_traceparent(),
                model_override,
                check_runs_enabled: settings.check_run_reporting.value,
                run_after_secs: None,
            };
            create_review_task(pool, task, delivery_id, is_sync, &settings).await;
        }
        "close" => match crate::db::cancel_active_tasks_for_pr(pool, repository_id, mr_iid).await {
            Ok(ids) if !ids.is_empty() => tracing::info!(
                delivery_id,
                mr = mr_iid,
                cancelled = ids.len(),
                "GitLab MR closed; cancelled active tasks"
            ),
            Ok(_) => {}
            Err(error) => {
                tracing::error!(%error, delivery_id, mr = mr_iid, "GitLab: failed to cancel MR tasks")
            }
        },
        _ => {}
    }
}

/// `Push Hook`: re-index the repo when its **default branch** moves, same as GitHub push events.
async fn handle_gitlab_push(
    state: &crate::AppState,
    payload: &serde_json::Value,
    delivery_id: &str,
) {
    let Some(pool) = state.db.as_ref() else {
        return;
    };
    // A branch deletion carries no commits to index.
    if payload["after"].as_str() == Some("0000000000000000000000000000000000000000") {
        return;
    }
    let project = &payload["project"];
    let Some((project_id, owner, name, default_branch)) = gitlab_project_identity(project) else {
        tracing::warn!(
            delivery_id,
            "GitLab push payload missing project fields; skipping"
        );
        return;
    };
    let Some(git_ref) = payload["ref"].as_str() else {
        tracing::warn!(delivery_id, "GitLab push payload missing ref; skipping");
        return;
    };
    // Only the default branch matters for the base index.
    if git_ref != format!("refs/heads/{default_branch}") {
        return;
    }
    let installation_id = project_id;
    let repository_id = match crate::db::upsert_repository(
        pool,
        Platform::GitLab,
        project_id,
        owner,
        name,
        default_branch,
        Some(installation_id),
    )
    .await
    {
        Ok(id) => id,
        Err(error) => {
            tracing::error!(%error, delivery_id, "GitLab push: failed to upsert repository");
            return;
        }
    };
    // Approval gate (Epic #75).
    if !approved_or_skip(pool, repository_id, delivery_id, 0).await {
        return;
    }
    match crate::db::create_index_task(pool, repository_id, installation_id).await {
        Ok(Some(task_id)) => tracing::info!(
            delivery_id, repo = %format!("{owner}/{name}"), %task_id,
            "GitLab default-branch push → re-index queued"
        ),
        Ok(None) => tracing::info!(
            delivery_id, repo = %format!("{owner}/{name}"),
            "GitLab default-branch push → index already in flight; skipped"
        ),
        Err(error) => {
            tracing::error!(%error, delivery_id, "GitLab push: failed to create index task")
        }
    }
}

/// `Note Hook` whose body starts with `@<handle>` → a manual run. Works on an **MR thread** (a
/// diff-scoped re-review — we fetch the MR's base/head SHAs via the GitLab API) and on a **plain
/// issue** (no diff, the agent answers). Mirrors [`handle_issue_comment`] for GitHub.
async fn handle_gitlab_note(
    state: &crate::AppState,
    payload: &serde_json::Value,
    delivery_id: &str,
) {
    let Some(pool) = state.db.as_ref() else {
        return;
    };
    let project = &payload["project"];
    let Some((project_id, owner, name, default_branch)) = gitlab_project_identity(project) else {
        tracing::warn!(
            delivery_id,
            "GitLab note payload missing project fields; skipping"
        );
        return;
    };
    let installation_id = project_id;
    // GitLab note hooks don't carry an `action` — the hook fires on creation only.
    let body = payload["object_attributes"]["note"]
        .as_str()
        .unwrap_or_default();
    let Some(bot_handle) = state
        .gitlab
        .as_ref()
        .and_then(|registry| registry.bot_handle(project_id))
    else {
        tracing::warn!(
            delivery_id,
            project_id,
            "GitLab note project is not configured; skipping"
        );
        return;
    };
    if !mentions_handle(body, bot_handle) {
        return;
    }
    // `noteable_type` tells us MR vs issue: "MergeRequest" or "Issue".
    let noteable_type = payload["object_attributes"]["noteable_type"]
        .as_str()
        .unwrap_or_default();
    let is_pr = noteable_type == "MergeRequest";
    let target_type = if is_pr { "pull_request" } else { "issue" };

    // The MR/issue iid: `merge_request.iid` for MR notes, `issue.iid` for issue notes.
    let target_iid = if is_pr {
        payload["merge_request"]["iid"].as_i64()
    } else {
        payload["issue"]["iid"].as_i64()
    };
    let Some(target_iid) = target_iid else {
        tracing::warn!(
            delivery_id,
            "GitLab note payload missing MR/issue iid; skipping"
        );
        return;
    };

    let repository_id = match crate::db::upsert_repository(
        pool,
        Platform::GitLab,
        project_id,
        owner,
        name,
        default_branch,
        Some(installation_id),
    )
    .await
    {
        Ok(id) => id,
        Err(error) => {
            tracing::error!(%error, delivery_id, "GitLab: failed to upsert repository");
            return;
        }
    };
    // Approval gate (Epic #75).
    if !approved_or_skip(pool, repository_id, delivery_id, target_iid).await {
        return;
    }

    // An MR re-review needs the base/head SHAs to scope the diff; a plain issue has no diff.
    let (base_sha, head_sha) = if is_pr {
        let Some(gitlab) = state
            .gitlab
            .as_ref()
            .and_then(|registry| registry.client_for_project(project_id))
        else {
            tracing::warn!(
                delivery_id,
                project_id,
                "GitLab project client not configured; cannot fetch MR SHAs"
            );
            return;
        };
        let repo_ref = crate::integrations::platform::RepoRef {
            platform: Platform::GitLab,
            full_name: format!("{owner}/{name}"),
            platform_repo_id: project_id,
            installation_id,
        };
        match CodePlatform::pr_shas(gitlab, &repo_ref, target_iid).await {
            Ok(shas) => shas,
            Err(error) => {
                tracing::error!(%error, delivery_id, mr = target_iid, "GitLab: fetch MR SHAs failed");
                return;
            }
        }
    } else {
        (None, None)
    };

    let repo_ref = crate::integrations::platform::RepoRef {
        platform: Platform::GitLab,
        full_name: format!("{owner}/{name}"),
        platform_repo_id: project_id,
        installation_id,
    };
    let (preset, settings) = crate::settings::resolve_preset_and_settings(
        pool,
        state
            .gitlab
            .as_ref()
            .and_then(|registry| registry.client_for_project(project_id))
            .map(|client| client as &dyn CodePlatform),
        &repo_ref,
        base_sha.as_deref().unwrap_or(default_branch),
        crate::preset::EntryPoint::Mention,
        repository_id,
    )
    .await;

    let command_text = command_from_comment(body);
    let trigger_comment_id = payload["object_attributes"]["id"].as_i64();
    let model_override =
        crate::model::resolve_model_override(pool, repository_id, installation_id).await;
    let task = crate::db::NewTask {
        repository_id,
        installation_id,
        webhook_delivery_id: delivery_id.to_string(),
        target_type: target_type.to_string(),
        target_id: target_iid,
        command_text,
        base_sha,
        head_sha,
        run_epoch: 0,
        preset,
        entry_point: crate::preset::EntryPoint::Mention.as_str().to_string(),
        trigger_comment_id,
        trace_context: lci_observability::current_traceparent(),
        model_override,
        check_runs_enabled: settings.check_run_reporting.value,
        run_after_secs: None,
    };
    tracing::info!(
        delivery_id,
        target = target_iid,
        kind = target_type,
        "GitLab @mention requested"
    );
    create_explicit_review_task(pool, task, delivery_id).await;
}

/// Bitbucket Cloud webhook payloads don't reliably carry the repo's default branch (unlike
/// GitHub/GitLab, whose `repository`/`project` objects always include it) — so this fetches it via
/// the configured `BitbucketClient`, mirroring `handle_gitlab_merge_request`'s on-demand API
/// fallback for missing `diff_refs`. Falls back to `"main"` on any error (unconfigured project, API
/// failure) — a review/index task should never be dropped over a cosmetic field.
async fn bitbucket_default_branch_or_fallback(
    state: &crate::AppState,
    installation_id: i64,
    full_name: &str,
) -> String {
    let Some(client) = state
        .bitbucket
        .as_ref()
        .and_then(|registry| registry.client_for_project(installation_id))
    else {
        return "main".to_string();
    };
    let repo_ref = RepoRef {
        platform: Platform::Bitbucket,
        full_name: full_name.to_string(),
        platform_repo_id: installation_id,
        installation_id,
    };
    match CodePlatform::default_branch(client, &repo_ref).await {
        Ok(branch) => branch,
        Err(error) => {
            tracing::warn!(
                %error,
                repo = full_name,
                "Bitbucket: failed to fetch default branch; falling back to 'main'"
            );
            "main".to_string()
        }
    }
}

/// `pullrequest:created` → the automatic first review. `pullrequest:updated` (epic #566) → a review
/// of the new commits, gated on the repo's `review_on_push` setting (off by default) and skipped for
/// bots. `pullrequest:fulfilled` (merged) / `pullrequest:rejected` (declined) → cancel the PR's
/// active tasks. Mirrors [`handle_gitlab_merge_request`] / [`handle_pull_request`].
///
/// Unlike GitLab's `oldrev`, Bitbucket's `pullrequest:updated` payload carries no reliable "did the
/// head SHA actually move" signal (it also fires on reviewer/description edits) — the idempotency
/// index is the guard here instead: a same-head redelivery collapses via `create_task`'s
/// `ON CONFLICT DO NOTHING`, so a metadata-only update that doesn't change `source.commit.hash`
/// creates no new task, just a harmless dedup no-op.
async fn handle_bitbucket_pullrequest(
    state: &crate::AppState,
    event: &str,
    payload: &serde_json::Value,
    delivery_id: &str,
) {
    let Some(pool) = state.db.as_ref() else {
        return;
    };
    let Some((full_name, workspace, repo_slug)) = bitbucket_repo_identity(payload) else {
        tracing::warn!(
            delivery_id,
            "Bitbucket PR payload missing repository.full_name; skipping"
        );
        return;
    };
    let Some(pr_number) = payload["pullrequest"]["id"].as_i64() else {
        tracing::warn!(
            delivery_id,
            "Bitbucket PR payload missing pullrequest.id; skipping"
        );
        return;
    };
    // Bitbucket has no numeric project id (unlike GitLab's `project.id`) — derive a stable one from
    // `workspace/repo_slug` and use it as both the platform repo id and the installation id, the
    // same dual role GitLab's `project_id` plays.
    let installation_id = crate::integrations::platform::stable_id_from_key(&full_name);
    let default_branch =
        bitbucket_default_branch_or_fallback(state, installation_id, &full_name).await;
    let repository_id = match crate::db::upsert_repository(
        pool,
        Platform::Bitbucket,
        installation_id,
        &workspace,
        &repo_slug,
        &default_branch,
        Some(installation_id),
    )
    .await
    {
        Ok(id) => id,
        Err(error) => {
            tracing::error!(%error, delivery_id, "Bitbucket: failed to upsert repository");
            return;
        }
    };

    match event {
        "pullrequest:created" | "pullrequest:updated" => {
            let is_sync = event == "pullrequest:updated";
            // Approval gate (Epic #75): a repo must be admin-approved before any review runs.
            if !approved_or_skip(pool, repository_id, delivery_id, pr_number).await {
                return;
            }
            let pr = &payload["pullrequest"];
            // RFC-0003: skip bot-authored PRs. Bitbucket has no clean `type: "Bot"` field either —
            // reuse the same bot-suffix heuristic GitLab uses (`should_skip_gitlab_bot_review`),
            // applied to the PR author's nickname/display name. Fails open on an empty/garbled
            // author, same as GitLab and GitHub.
            let author = pr["author"]["nickname"]
                .as_str()
                .or_else(|| pr["author"]["display_name"].as_str())
                .unwrap_or("");
            if should_skip_gitlab_bot_review(state.review.skip_bot_authored_prs(), author) {
                tracing::info!(
                    delivery_id,
                    pr = pr_number,
                    repository_id,
                    is_sync,
                    "Bitbucket PR author appears to be a bot; skipping automatic review"
                );
                crate::http::metrics::review_skipped_bot_author();
                return;
            }
            let base_sha = pr["destination"]["commit"]["hash"]
                .as_str()
                .map(str::to_string);
            let head_sha = pr["source"]["commit"]["hash"].as_str().map(str::to_string);
            let repo_ref = RepoRef {
                platform: Platform::Bitbucket,
                full_name: full_name.clone(),
                platform_repo_id: installation_id,
                installation_id,
            };
            let entry = if is_sync {
                crate::preset::EntryPoint::PrSync
            } else {
                crate::preset::EntryPoint::PrOpen
            };
            let (preset, settings) = crate::settings::resolve_preset_and_settings(
                pool,
                state
                    .bitbucket
                    .as_ref()
                    .and_then(|registry| registry.client_for_project(installation_id))
                    .map(|client| client as &dyn CodePlatform),
                &repo_ref,
                base_sha.as_deref().unwrap_or(&default_branch),
                entry,
                repository_id,
            )
            .await;
            // Epic #566: a repo can opt out of the automatic on-open review (stays @mention-only) and,
            // independently, opt IN to a review on later pushes (off by default — see settings.rs).
            // Checked here rather than at dispatch so an opted-out repo creates no task at all.
            let (setting, disabled_msg) = if is_sync {
                (
                    &settings.review_on_push,
                    "review-on-push disabled for this repo; skipping (mention-triggered reviews still run)",
                )
            } else {
                (
                    &settings.review_on_pr_open,
                    "automatic on-open review disabled for this repo; skipping (mention-triggered reviews still run)",
                )
            };
            if !setting.value {
                tracing::info!(delivery_id, repository_id, is_sync, source = ?setting.source, "{disabled_msg}");
                return;
            }
            let model_override =
                crate::model::resolve_model_override(pool, repository_id, installation_id).await;
            let task = crate::db::NewTask {
                repository_id,
                installation_id,
                webhook_delivery_id: delivery_id.to_string(),
                target_type: "pull_request".to_string(),
                target_id: pr_number,
                command_text: "review".to_string(),
                base_sha,
                head_sha,
                run_epoch: 0,
                preset,
                entry_point: entry.as_str().to_string(),
                trigger_comment_id: None,
                trace_context: lci_observability::current_traceparent(),
                model_override,
                check_runs_enabled: settings.check_run_reporting.value,
                run_after_secs: None,
            };
            create_review_task(pool, task, delivery_id, is_sync, &settings).await;
        }
        "pullrequest:fulfilled" | "pullrequest:rejected" => {
            match crate::db::cancel_active_tasks_for_pr(pool, repository_id, pr_number).await {
                Ok(ids) if !ids.is_empty() => tracing::info!(
                    delivery_id,
                    pr = pr_number,
                    cancelled = ids.len(),
                    "Bitbucket PR closed; cancelled active tasks"
                ),
                Ok(_) => {}
                Err(error) => tracing::error!(
                    %error, delivery_id, pr = pr_number, "Bitbucket: failed to cancel PR tasks"
                ),
            }
        }
        _ => {}
    }
}

/// `repo:push`: re-index the repo when its **default branch** moves, same as GitHub/GitLab push
/// events.
async fn handle_bitbucket_push(
    state: &crate::AppState,
    payload: &serde_json::Value,
    delivery_id: &str,
) {
    let Some(pool) = state.db.as_ref() else {
        return;
    };
    let Some((full_name, workspace, repo_slug)) = bitbucket_repo_identity(payload) else {
        tracing::warn!(
            delivery_id,
            "Bitbucket push payload missing repository.full_name; skipping"
        );
        return;
    };
    let installation_id = crate::integrations::platform::stable_id_from_key(&full_name);
    let default_branch =
        bitbucket_default_branch_or_fallback(state, installation_id, &full_name).await;

    let Some(changes) = payload["push"]["changes"].as_array() else {
        tracing::warn!(
            delivery_id,
            "Bitbucket push payload missing push.changes; skipping"
        );
        return;
    };
    // Only a push that moves the default branch re-indexes (a branch deletion carries `new: null`).
    let moves_default_branch = changes.iter().any(|change| {
        !change["new"].is_null() && change["new"]["name"].as_str() == Some(default_branch.as_str())
    });
    if !moves_default_branch {
        return;
    }

    let repository_id = match crate::db::upsert_repository(
        pool,
        Platform::Bitbucket,
        installation_id,
        &workspace,
        &repo_slug,
        &default_branch,
        Some(installation_id),
    )
    .await
    {
        Ok(id) => id,
        Err(error) => {
            tracing::error!(%error, delivery_id, "Bitbucket push: failed to upsert repository");
            return;
        }
    };
    // Approval gate (Epic #75).
    if !approved_or_skip(pool, repository_id, delivery_id, 0).await {
        return;
    }
    match crate::db::create_index_task(pool, repository_id, installation_id).await {
        Ok(Some(task_id)) => tracing::info!(
            delivery_id, repo = %full_name, %task_id,
            "Bitbucket default-branch push → re-index queued"
        ),
        Ok(None) => tracing::info!(
            delivery_id, repo = %full_name,
            "Bitbucket default-branch push → index already in flight; skipped"
        ),
        Err(error) => {
            tracing::error!(%error, delivery_id, "Bitbucket push: failed to create index task")
        }
    }
}

/// `pullrequest:comment_created` whose body starts with `@<handle>` → a manual re-review run.
/// Mirrors [`handle_gitlab_note`] / [`handle_issue_comment`]; Bitbucket's Issue Tracker comments are
/// out of scope here (this project's minimal Bitbucket slice covers PR comments only).
async fn handle_bitbucket_comment(
    state: &crate::AppState,
    payload: &serde_json::Value,
    delivery_id: &str,
) {
    let Some(pool) = state.db.as_ref() else {
        return;
    };
    let Some((full_name, workspace, repo_slug)) = bitbucket_repo_identity(payload) else {
        tracing::warn!(
            delivery_id,
            "Bitbucket comment payload missing repository.full_name; skipping"
        );
        return;
    };
    let installation_id = crate::integrations::platform::stable_id_from_key(&full_name);
    let body = payload["comment"]["content"]["raw"]
        .as_str()
        .unwrap_or_default();
    let Some(bot_handle) = state
        .bitbucket
        .as_ref()
        .and_then(|registry| registry.bot_handle(installation_id))
    else {
        tracing::warn!(
            delivery_id,
            repo = %full_name,
            "Bitbucket comment repo is not configured; skipping"
        );
        return;
    };
    if !mentions_handle(body, bot_handle) {
        return;
    }
    let Some(pr_number) = payload["pullrequest"]["id"].as_i64() else {
        tracing::warn!(
            delivery_id,
            "Bitbucket comment payload missing pullrequest.id; skipping"
        );
        return;
    };
    let default_branch =
        bitbucket_default_branch_or_fallback(state, installation_id, &full_name).await;
    let repository_id = match crate::db::upsert_repository(
        pool,
        Platform::Bitbucket,
        installation_id,
        &workspace,
        &repo_slug,
        &default_branch,
        Some(installation_id),
    )
    .await
    {
        Ok(id) => id,
        Err(error) => {
            tracing::error!(%error, delivery_id, "Bitbucket: failed to upsert repository");
            return;
        }
    };
    // Approval gate (Epic #75): even an explicit @mention can't run on an unapproved repo.
    if !approved_or_skip(pool, repository_id, delivery_id, pr_number).await {
        return;
    }

    let Some(client) = state
        .bitbucket
        .as_ref()
        .and_then(|registry| registry.client_for_project(installation_id))
    else {
        tracing::warn!(
            delivery_id,
            "Bitbucket project client not configured; cannot fetch PR SHAs"
        );
        return;
    };
    let repo_ref = RepoRef {
        platform: Platform::Bitbucket,
        full_name: full_name.clone(),
        platform_repo_id: installation_id,
        installation_id,
    };
    let (base_sha, head_sha) = match CodePlatform::pr_shas(client, &repo_ref, pr_number).await {
        Ok(shas) => shas,
        Err(error) => {
            tracing::error!(%error, delivery_id, pr = pr_number, "Bitbucket: fetch PR SHAs failed");
            return;
        }
    };

    let (preset, settings) = crate::settings::resolve_preset_and_settings(
        pool,
        Some(client as &dyn CodePlatform),
        &repo_ref,
        base_sha.as_deref().unwrap_or(&default_branch),
        crate::preset::EntryPoint::Mention,
        repository_id,
    )
    .await;

    let command_text = command_from_comment(body);
    let trigger_comment_id = payload["comment"]["id"].as_i64();
    let model_override =
        crate::model::resolve_model_override(pool, repository_id, installation_id).await;
    let task = crate::db::NewTask {
        repository_id,
        installation_id,
        webhook_delivery_id: delivery_id.to_string(),
        target_type: "pull_request".to_string(),
        target_id: pr_number,
        command_text,
        base_sha,
        head_sha,
        run_epoch: 0,
        preset,
        entry_point: crate::preset::EntryPoint::Mention.as_str().to_string(),
        trigger_comment_id,
        trace_context: lci_observability::current_traceparent(),
        model_override,
        check_runs_enabled: settings.check_run_reporting.value,
        run_after_secs: None,
    };
    tracing::info!(
        delivery_id,
        target = pr_number,
        kind = "pull_request",
        "Bitbucket @mention requested"
    );
    create_explicit_review_task(pool, task, delivery_id).await;
}

/// RFC-0003 for GitLab: skip the automatic fast-tier review for bot-authored MRs. GitLab doesn't
/// have a clean `type: "Bot"` field like GitHub; we check the commit author name for a `_bot`
/// suffix or known bot patterns. Absent/garbled signals **fail open** (treated as human).
fn should_skip_gitlab_bot_review(skip_bot_authored_prs: bool, author: &str) -> bool {
    if !skip_bot_authored_prs {
        return false;
    }
    // GitLab bot accounts typically end with `_bot` (e.g. `gitlab-bot`, `dependabot-bot`).
    // Also check for common bot name patterns. Fail open: an empty/unknown author is human.
    if author.is_empty() {
        return false;
    }
    author.ends_with("_bot") || author.ends_with("-bot") || author == "GitLab"
}

/// True when a comment body is addressed to the app — its first non-space text is `@<handle>`
/// (case-insensitive). A leading `@<handle>` is how a human asks for a re-review.
fn mentions_handle(body: &str, handle: &str) -> bool {
    let mention = format!("@{}", handle.to_ascii_lowercase());
    body.trim_start().to_ascii_lowercase().starts_with(&mention)
}

/// Upper bound on the free-text instruction carried from a comment into the agent prompt. The text
/// only steers reasoning (write-back is still diff-validated, ADR-0022), but we cap it so a giant
/// comment can't blow up the prompt.
const MAX_INSTRUCTION_CHARS: usize = 2_000;

/// The command carried from an `@<handle> …` comment into the task/prompt: the WHOLE comment body,
/// trimmed and length-bounded (#138). We pass the full message — NOT just the text after the handle —
/// so the agent (which knows its own name from the system prompt) sees the complete request, including
/// co-mentions like `@<handle> & /gemini please review this` that stripping the handle would mangle.
/// `mentions_handle` already gates that the comment is addressed to us and the mention leads, and since
/// the body therefore starts with `@<handle>` it can never exactly equal the reserved `index` command.
fn command_from_comment(body: &str) -> String {
    body.trim().chars().take(MAX_INSTRUCTION_CHARS).collect()
}

/// `pull_request` events. `opened` → the automatic first review. `synchronize` (epic #566) → a
/// review of the new commits, gated on the repo's `review_on_push` setting (off by default) and
/// skipped for bots/drafts. `closed` → cancel the PR's active tasks (the reaper then stops their
/// Jobs). `reopened` does nothing — a re-review is requested with an `@<handle>` comment
/// ([`handle_issue_comment`]).
async fn handle_pull_request(
    state: &crate::AppState,
    payload: &serde_json::Value,
    delivery_id: &str,
) {
    let Some(pool) = state.db.as_ref() else {
        return;
    };
    let action = payload["action"].as_str().unwrap_or_default();
    if !matches!(action, "opened" | "closed") {
        return;
    }
    let repo = &payload["repository"];
    let (Some(github_repo_id), Some(owner), Some(name), Some(default_branch), Some(pr_number)) = (
        repo["id"].as_i64(),
        repo["owner"]["login"].as_str(),
        repo["name"].as_str(),
        repo["default_branch"].as_str(),
        payload["pull_request"]["number"].as_i64(),
    ) else {
        tracing::warn!(
            delivery_id,
            "pull_request payload missing repo/number fields; skipping"
        );
        return;
    };
    // installation.id is present on PR events; record it so index-on-approve can mint a clone token.
    let installation_id_opt = payload["installation"]["id"].as_i64();
    let repository_id = match crate::db::upsert_repository(
        pool,
        crate::integrations::platform::Platform::GitHub,
        github_repo_id,
        owner,
        name,
        default_branch,
        installation_id_opt,
    )
    .await
    {
        Ok(id) => id,
        Err(error) => {
            tracing::error!(%error, delivery_id, "failed to upsert repository");
            return;
        }
    };

    match action {
        "opened" | "synchronize" => {
            let Some(installation_id) = installation_id_opt else {
                return;
            };
            // Approval gate (Epic #75): a repo must be admin-approved before any review runs.
            if !approved_or_skip(pool, repository_id, delivery_id, pr_number).await {
                return;
            }
            let pr = &payload["pull_request"];
            let is_sync = action == "synchronize";
            // RFC-0003: skip the automatic review for bot-authored PRs (Dependabot, Renovate, another
            // GitHub App, or ourselves) — mechanical diffs burn LLM budget on low-signal comments and
            // risk bot-on-bot feedback loops. The `@mention` deep-review path is untouched: a human
            // can still ask for a full review on the same PR. Applies to both open and sync.
            if should_skip_bot_review(state.review.skip_bot_authored_prs(), pr) {
                tracing::info!(
                    delivery_id,
                    pr = pr_number,
                    repository_id,
                    is_sync,
                    "PR author is a bot; skipping automatic review"
                );
                crate::http::metrics::review_skipped_bot_author();
                return;
            }
            // Epic #566: a draft PR is, by construction, still being pushed to repeatedly — reviewing
            // every intermediate push burns budget on code the author knows isn't ready. Scoped to
            // `synchronize` only (a NEW entry point, so this is not a behavior change to `opened`,
            // which has never draft-gated).
            if is_sync && pr["draft"].as_bool() == Some(true) {
                tracing::info!(
                    delivery_id,
                    pr = pr_number,
                    repository_id,
                    "PR is a draft; skipping the on-push review (mention-triggered reviews still run)"
                );
                return;
            }
            let base_sha = pr["base"]["sha"].as_str().map(str::to_string);
            let head_sha = pr["head"]["sha"].as_str().map(str::to_string);
            let entry = if is_sync {
                crate::preset::EntryPoint::PrSync
            } else {
                crate::preset::EntryPoint::PrOpen
            };
            // ADR-0103: resolve the repo's configured preset+settings, reading
            // `.lightbridge-code-review.jsonc` at the BASE ref (fork-safe by construction — never the
            // PR head) via a single small file fetch, falling back to the platform default (`fast`,
            // reproducing today's ADR-0062 behavior) when the repo declares nothing.
            let repo_ref = RepoRef {
                platform: Platform::GitHub,
                full_name: format!("{owner}/{name}"),
                platform_repo_id: github_repo_id,
                installation_id,
            };
            let (preset, settings) = crate::settings::resolve_preset_and_settings(
                pool,
                state
                    .platforms
                    .get(&Platform::GitHub)
                    .map(std::sync::Arc::as_ref),
                &repo_ref,
                base_sha.as_deref().unwrap_or(default_branch),
                entry,
                repository_id,
            )
            .await;
            // Epic #566: a repo can opt out of the automatic on-open review (stays @mention-only) and,
            // independently, opt IN to a review on later pushes (off by default — see settings.rs).
            // Checked here rather than at dispatch so an opted-out repo creates no task at all.
            let (setting, disabled_msg) = if is_sync {
                (
                    &settings.review_on_push,
                    "review-on-push disabled for this repo; skipping (mention-triggered reviews still run)",
                )
            } else {
                (
                    &settings.review_on_pr_open,
                    "automatic on-open review disabled for this repo; skipping (mention-triggered reviews still run)",
                )
            };
            if !setting.value {
                tracing::info!(delivery_id, repository_id, is_sync, source = ?setting.source, "{disabled_msg}");
                return;
            }
            let model_override =
                crate::model::resolve_model_override(pool, repository_id, installation_id).await;
            let task = crate::db::NewTask {
                repository_id,
                installation_id,
                webhook_delivery_id: delivery_id.to_string(),
                target_type: "pull_request".to_string(),
                target_id: pr_number,
                command_text: "review".to_string(),
                base_sha,
                head_sha,
                // Every PUSH gets its own row via the idempotency index's head_sha column (redeliveries
                // of the SAME head still collapse via create_task's ON CONFLICT DO NOTHING); run_epoch
                // stays 0 for every automatic review, same as `opened`.
                run_epoch: 0,
                preset,
                entry_point: entry.as_str().to_string(),
                // ADR-0068: no trigger comment on an automatic review → the lifecycle reactions land on
                // the PR body itself.
                trigger_comment_id: None,
                trace_context: lci_observability::current_traceparent(),
                model_override,
                check_runs_enabled: settings.check_run_reporting.value,
                // Storm-strategy delay lands in #571 (supersede/debounce); `every` — this slice's only
                // live strategy — never delays.
                run_after_secs: None,
            };
            create_review_task(pool, task, delivery_id, is_sync, &settings).await;
        }
        "closed" => {
            match crate::db::cancel_active_tasks_for_pr(pool, repository_id, pr_number).await {
                Ok(ids) if !ids.is_empty() => tracing::info!(
                    delivery_id,
                    pr = pr_number,
                    cancelled = ids.len(),
                    "PR closed; cancelled active tasks (reaper stops their Jobs)"
                ),
                Ok(_) => {}
                Err(error) => {
                    tracing::error!(%error, delivery_id, pr = pr_number, "failed to cancel PR tasks")
                }
            }
        }
        _ => {}
    }
}

/// `push` events: re-index the repo when its **default branch** moves (e.g. a merged PR), so the
/// semantic + graph index stays fresh instead of going stale and returning 0 hits (dogfood run
/// 7c15f9bb — reviews reused a hollow base index). Only the default branch (feature/PR-branch pushes
/// don't change the base index), only approved repos, and `create_index_task` dedups against an
/// in-flight index so a burst of pushes can't pile up.
async fn handle_push(state: &crate::AppState, payload: &serde_json::Value, delivery_id: &str) {
    let Some(pool) = state.db.as_ref() else {
        return;
    };
    // A branch/tag deletion carries no commits to index.
    if payload["deleted"].as_bool() == Some(true) {
        return;
    }
    let repo = &payload["repository"];
    let (Some(github_repo_id), Some(owner), Some(name), Some(default_branch), Some(git_ref)) = (
        repo["id"].as_i64(),
        repo["owner"]["login"].as_str(),
        repo["name"].as_str(),
        repo["default_branch"].as_str(),
        payload["ref"].as_str(),
    ) else {
        tracing::warn!(
            delivery_id,
            "push payload missing repo/ref fields; skipping"
        );
        return;
    };
    // Only the default branch matters for the base index.
    if git_ref != format!("refs/heads/{default_branch}") {
        return;
    }
    let Some(installation_id) = payload["installation"]["id"].as_i64() else {
        return;
    };
    let repository_id = match crate::db::upsert_repository(
        pool,
        crate::integrations::platform::Platform::GitHub,
        github_repo_id,
        owner,
        name,
        default_branch,
        Some(installation_id),
    )
    .await
    {
        Ok(id) => id,
        Err(error) => {
            tracing::error!(%error, delivery_id, "push: failed to upsert repository");
            return;
        }
    };
    // Approval gate (Epic #75): same as reviews — nothing runs on an unapproved repo. (`pr = 0`: this
    // is a push, not a PR; the arg is only used for the skip log line.)
    if !approved_or_skip(pool, repository_id, delivery_id, 0).await {
        return;
    }
    match crate::db::create_index_task(pool, repository_id, installation_id).await {
        Ok(Some(task_id)) => tracing::info!(
            delivery_id, repo = %format!("{owner}/{name}"), %task_id,
            "default-branch push → re-index queued"
        ),
        Ok(None) => tracing::info!(
            delivery_id, repo = %format!("{owner}/{name}"),
            "default-branch push → index already in flight; skipped"
        ),
        Err(error) => tracing::error!(%error, delivery_id, "push: failed to create index task"),
    }
}

/// `issue_comment` whose body starts with `@<handle>` → a manual run. Works on a **PR thread** (a
/// diff-scoped re-review — we fetch the PR's base/head SHAs, which the comment payload omits) and on a
/// **plain issue** (ADR-0033 slice 3: no diff, so the agent answers and finalize posts a single reply
/// comment). The next `run_epoch` lets a fresh task through the idempotency index even when nothing
/// changed.
async fn handle_issue_comment(
    state: &crate::AppState,
    payload: &serde_json::Value,
    delivery_id: &str,
) {
    let Some(pool) = state.db.as_ref() else {
        return;
    };
    if payload["action"].as_str() != Some("created") {
        return;
    }
    let body = payload["comment"]["body"].as_str().unwrap_or_default();
    if !mentions_handle(body, &state.app_handle) {
        return; // not addressed to us
    }
    // A PR thread carries a `pull_request` object on the issue; a plain issue does not.
    let is_pr = !payload["issue"]["pull_request"].is_null();
    let target_type = if is_pr { "pull_request" } else { "issue" };

    let repo = &payload["repository"];
    let (
        Some(github_repo_id),
        Some(owner),
        Some(name),
        Some(default_branch),
        Some(installation_id),
        Some(number),
    ) = (
        repo["id"].as_i64(),
        repo["owner"]["login"].as_str(),
        repo["name"].as_str(),
        repo["default_branch"].as_str(),
        payload["installation"]["id"].as_i64(),
        payload["issue"]["number"].as_i64(),
    )
    else {
        tracing::warn!(
            delivery_id,
            "issue_comment payload missing fields; skipping"
        );
        return;
    };

    let repository_id = match crate::db::upsert_repository(
        pool,
        crate::integrations::platform::Platform::GitHub,
        github_repo_id,
        owner,
        name,
        default_branch,
        Some(installation_id),
    )
    .await
    {
        Ok(id) => id,
        Err(error) => {
            tracing::error!(%error, delivery_id, "failed to upsert repository");
            return;
        }
    };
    // Approval gate (Epic #75): even an explicit @mention can't run on an unapproved repo.
    if !approved_or_skip(pool, repository_id, delivery_id, number).await {
        return;
    }

    let repo_ref = RepoRef {
        platform: Platform::GitHub,
        full_name: format!("{owner}/{name}"),
        platform_repo_id: github_repo_id,
        installation_id,
    };
    let github_platform = state.platforms.get(&Platform::GitHub);

    // A PR re-review needs the base/head SHAs to scope the diff (the comment payload omits them); a
    // plain issue has no diff, so the agent answers against the default branch.
    let (base_sha, head_sha) = if is_pr {
        let Some(github) = github_platform else {
            tracing::warn!(
                delivery_id,
                "github app not configured; cannot fetch PR SHAs"
            );
            return;
        };
        match github.pr_shas(&repo_ref, number).await {
            Ok(shas) => shas,
            Err(error) => {
                tracing::error!(%error, delivery_id, pr = number, "fetch PR SHAs failed");
                return;
            }
        }
    } else {
        (None, None)
    };

    // ADR-0103: resolve the repo's configured preset for the mention entry point, reading the repo
    // config at the PR's BASE ref when there is one (fork-safe), else the default branch.
    let (preset, settings) = crate::settings::resolve_preset_and_settings(
        pool,
        github_platform.map(std::sync::Arc::as_ref),
        &repo_ref,
        base_sha.as_deref().unwrap_or(default_branch),
        crate::preset::EntryPoint::Mention,
        repository_id,
    )
    .await;

    // Carry the WHOLE comment into the task → prompt (#138): the agent knows its own name from the
    // system prompt, so it interprets "@<handle> please review this" — and co-mentions like
    // "@<handle> & /gemini …" — itself; stripping the handle mangled those. The agent decides
    // review-vs-answer from the text and acts via its tools; the run kind is recorded at finalize
    // (emergent, ADR-0037), not classified here.
    let command_text = command_from_comment(body);
    // ADR-0068: the id of the comment that @mentioned us — the lifecycle reactions (👀/👍/👎/😕) react
    // on THIS comment, not the PR body, so the acknowledgment sits on the human's request. Absent on a
    // malformed payload → `None`, and the reactions fall back to the PR/issue body.
    let trigger_comment_id = payload["comment"]["id"].as_i64();
    let model_override =
        crate::model::resolve_model_override(pool, repository_id, installation_id).await;
    // An @mention is an explicit human command: it must ALWAYS create a task. True webhook
    // redeliveries are already deduped upstream by the `webhook_deliveries` delivery-id PRIMARY KEY,
    // so content-idempotency adds nothing here — and previously dropped legitimate re-requests when
    // the same wording landed on an unchanged head. `create_explicit_task` folds the next epoch into
    // the INSERT, so every mention lands a fresh, non-colliding row atomically. `run_epoch` is
    // ignored by that path (the INSERT computes it).
    let task = crate::db::NewTask {
        repository_id,
        installation_id,
        webhook_delivery_id: delivery_id.to_string(),
        target_type: target_type.to_string(),
        target_id: number,
        command_text,
        base_sha,
        head_sha,
        run_epoch: 0,
        preset,
        entry_point: crate::preset::EntryPoint::Mention.as_str().to_string(),
        trigger_comment_id,
        trace_context: lci_observability::current_traceparent(),
        model_override,
        check_runs_enabled: settings.check_run_reporting.value,
        run_after_secs: None,
    };
    tracing::info!(
        delivery_id,
        target = number,
        kind = target_type,
        "@mention requested"
    );
    create_explicit_review_task(pool, task, delivery_id).await;
}

/// Insert an **explicit @mention** task (always lands a row, never content-deduped). The auto open path
/// uses [`create_review_task`] instead, which keeps content-idempotency. No reaction is enqueued here:
/// ADR-0068 moves 👀 to *work-started* (the dispatcher launching the Job), so receipt no longer reacts.
#[tracing::instrument(name = "task.create", skip_all, fields(pr = task.target_id))]
async fn create_explicit_review_task(
    pool: &sqlx::PgPool,
    task: crate::db::NewTask,
    delivery_id: &str,
) {
    let pr = task.target_id;
    match crate::db::create_explicit_task(pool, &task).await {
        Ok(task_id) => {
            crate::http::metrics::task_created();
            tracing::info!(delivery_id, %task_id, pr, "created explicit review task");
        }
        Err(error) => tracing::error!(%error, delivery_id, pr, "failed to create explicit task"),
    }
}

/// Insert an automatic (`pr_open`/`pr_sync`) review task. No reaction is enqueued here: ADR-0068
/// moves 👀 to *work-started* (the dispatcher launching the Job), so receipt no longer reacts.
///
/// `is_sync` + `settings` apply epic #566's push-storm strategy to a **new** task only — an idempotent
/// re-delivery (`Ok(None)`) means no new head landed, so there is nothing to debounce or supersede:
/// - `debounce` stamps `run_after_secs` on `task` *before* it's inserted, so the claim query
///   (already `WHERE run_after <= now()`) simply won't offer it up until the quiet period elapses.
/// - `supersede` runs *after* a genuine insert, cancelling the PR's other automatic runs and
///   resolving each one's check run to `Cancelled` so a killed run doesn't hang "in progress" forever.
#[tracing::instrument(name = "task.create", skip_all, fields(pr = task.target_id))]
async fn create_review_task(
    pool: &sqlx::PgPool,
    mut task: crate::db::NewTask,
    delivery_id: &str,
    is_sync: bool,
    settings: &crate::settings::ResolvedSettings,
) {
    if is_sync && settings.push_strategy.value == crate::settings::PushStrategy::Debounce {
        task.run_after_secs = Some(settings.push_debounce.value.as_secs());
    }
    let (repository_id, pr, run_epoch) = (task.repository_id, task.target_id, task.run_epoch);
    let head_sha = task.head_sha.clone();
    match crate::db::create_task(pool, &task).await {
        Ok(Some(task_id)) => {
            crate::http::metrics::task_created();
            tracing::info!(delivery_id, %task_id, pr, run_epoch, "created review task");
            if is_sync
                && settings.push_strategy.value == crate::settings::PushStrategy::Supersede
                && let Some(keep_head) = head_sha.as_deref()
            {
                supersede_older_reviews(pool, repository_id, pr, keep_head, delivery_id).await;
            }
        }
        Ok(None) => tracing::info!(
            delivery_id,
            pr,
            run_epoch,
            "review task already exists; skipping (idempotent)"
        ),
        Err(error) => tracing::error!(%error, delivery_id, pr, "failed to create task"),
    }
}

/// Cancel a PR's other active automatic review runs in favor of the one just created at `keep_head`
/// (epic #566's `supersede` push-storm strategy), and resolve each cancelled task's check run to
/// `Cancelled`. Best-effort — a failure here logs and returns; the new review task is already
/// committed and must run regardless of whether its predecessors get cleaned up.
async fn supersede_older_reviews(
    pool: &sqlx::PgPool,
    repository_id: i64,
    pr: i64,
    keep_head: &str,
    delivery_id: &str,
) {
    match crate::db::cancel_superseded_pr_reviews(pool, repository_id, pr, keep_head).await {
        Ok(ids) if !ids.is_empty() => {
            tracing::info!(
                delivery_id,
                pr,
                cancelled = ids.len(),
                "superseded earlier automatic review(s) for this PR"
            );
            for id in ids {
                crate::http::internal::resolve_cancelled_check_run(pool, id).await;
            }
        }
        Ok(_) => {}
        Err(error) => {
            tracing::error!(%error, delivery_id, pr, "failed to cancel superseded reviews")
        }
    }
}

/// The approval gate (Epic #75): returns `true` only when the repo is admin-approved. A
/// pending/disabled repo (or a query error — fail closed) logs and returns `false`, so no review/index
/// task is created. This is what stops the tool from running on repos nobody opted in.
async fn approved_or_skip(
    pool: &sqlx::PgPool,
    repository_id: i64,
    delivery_id: &str,
    pr: i64,
) -> bool {
    match crate::db::repository_approved(pool, repository_id).await {
        Ok(true) => true,
        Ok(false) => {
            tracing::info!(
                delivery_id,
                pr,
                repository_id,
                "repository not approved; skipping (awaiting admin approval)"
            );
            false
        }
        Err(error) => {
            tracing::error!(%error, delivery_id, repository_id, "approval check failed; skipping (fail closed)");
            false
        }
    }
}

/// Detects a bot-authored PR (RFC-0003) from the `opened` payload's `pull_request.user` object: the
/// GitHub API's own `type == "Bot"` field, with a `[bot]` login-suffix backstop for the cases where
/// `type` is absent or unexpected. No extra GitHub API call — both fields already ride the `opened`
/// payload. Absent/garbled signals **fail open** (treated as human) — never silently drop a real PR's
/// automatic review.
fn pr_author_is_bot(pull_request: &serde_json::Value) -> bool {
    let user = &pull_request["user"];
    if user["type"].as_str() == Some("Bot") {
        return true;
    }
    user["login"]
        .as_str()
        .is_some_and(|login| login.ends_with("[bot]"))
}

/// The gate decision (RFC-0003): skip the automatic fast-tier review iff the knob is enabled AND the
/// PR author is a bot. Split from `pr_author_is_bot` so the config interaction is unit-testable on
/// its own.
fn should_skip_bot_review(skip_bot_authored_prs: bool, pull_request: &serde_json::Value) -> bool {
    skip_bot_authored_prs && pr_author_is_bot(pull_request)
}

/// `installation` events: `created` (the App was installed on an account) registers the selected
/// repos as **pending** approval; `deleted` (uninstalled) disables them. Repos default to pending so
/// nothing runs until an admin approves (Epic #75). The installation payload's repo objects carry no
/// `default_branch`; a placeholder is fine — the first PR webhook fills it in.
async fn handle_installation(
    state: &crate::AppState,
    payload: &serde_json::Value,
    delivery_id: &str,
) {
    let Some(pool) = state.db.as_ref() else {
        return;
    };
    let action = payload["action"].as_str().unwrap_or_default();
    let repos = payload["repositories"].as_array();
    let installation_id = payload["installation"]["id"].as_i64();
    match action {
        "created" => register_pending(pool, repos, installation_id, delivery_id).await,
        "deleted" => disable_repos(state, repos, delivery_id).await,
        _ => {} // suspend/unsuspend/new_permissions_accepted → persisted only
    }
}

/// `installation_repositories` events: repos added to / removed from an existing installation.
/// Added → pending (await approval); removed → disabled.
async fn handle_installation_repositories(
    state: &crate::AppState,
    payload: &serde_json::Value,
    delivery_id: &str,
) {
    let Some(pool) = state.db.as_ref() else {
        return;
    };
    let installation_id = payload["installation"]["id"].as_i64();
    register_pending(
        pool,
        payload["repositories_added"].as_array(),
        installation_id,
        delivery_id,
    )
    .await;
    disable_repos(
        state,
        payload["repositories_removed"].as_array(),
        delivery_id,
    )
    .await;
}

/// Register each repo (a webhook repo object: `id`, `full_name`) as pending approval, insert-only so
/// an already-approved repo is untouched. Records `installation_id` (for later index-on-approve).
async fn register_pending(
    pool: &sqlx::PgPool,
    repos: Option<&Vec<serde_json::Value>>,
    installation_id: Option<i64>,
    delivery_id: &str,
) {
    for repo in repos.into_iter().flatten() {
        let Some((github_repo_id, owner, name)) = repo_identity(repo) else {
            continue;
        };
        match crate::db::register_pending_repository(
            pool,
            crate::integrations::platform::Platform::GitHub,
            github_repo_id,
            owner,
            name,
            "",
            installation_id,
        )
        .await
        {
            Ok(true) => {
                tracing::info!(delivery_id, repo = %format!("{owner}/{name}"), "registered pending repository (awaiting approval)")
            }
            Ok(false) => {} // already known — leave its status as-is
            Err(error) => {
                tracing::error!(%error, delivery_id, "register pending repository failed")
            }
        }
    }
}

/// Mark each repo `disabled` (removed from the installation) and purge its index data (Epic #75,
/// Milestone B): cancel in-flight tasks + delete its `code_chunks` / Neo4j graph. The purge is
/// spawned so it can't block the webhook's deadline.
async fn disable_repos(
    state: &crate::AppState,
    repos: Option<&Vec<serde_json::Value>>,
    delivery_id: &str,
) {
    let Some(pool) = state.db.as_ref() else {
        return;
    };
    for repo in repos.into_iter().flatten() {
        let Some(github_repo_id) = repo["id"].as_i64() else {
            continue;
        };
        match crate::db::set_repository_status_by_platform_id(
            pool,
            crate::integrations::platform::Platform::GitHub,
            github_repo_id,
            "disabled",
        )
        .await
        {
            Ok(Some(repository_id)) => {
                tracing::info!(
                    delivery_id,
                    github_repo_id,
                    repository_id,
                    "repository disabled (removed from installation); purging index data"
                );
                crate::queue::lifecycle::spawn_purge(state, repository_id);
            }
            Ok(None) => {} // not known locally — nothing to disable/purge
            Err(error) => {
                tracing::error!(%error, delivery_id, github_repo_id, "disable repository failed")
            }
        }
    }
}

/// Extract `(github_repo_id, owner, name)` from a webhook repo object. The installation payload uses
/// `full_name` ("owner/name") rather than a nested owner object.
fn repo_identity(repo: &serde_json::Value) -> Option<(i64, &str, &str)> {
    let id = repo["id"].as_i64()?;
    let full_name = repo["full_name"].as_str()?;
    let (owner, name) = full_name.split_once('/')?;
    Some((id, owner, name))
}

fn header(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mention_must_lead_the_comment() {
        assert!(mentions_handle(
            "@lightbridge-assistant please review",
            "lightbridge-assistant"
        ));
        assert!(
            mentions_handle("  @Lightbridge-Assistant rerun", "lightbridge-assistant"),
            "leading space + case-insensitive"
        );
        // A mid-sentence mention is NOT a command (avoids re-running on casual references).
        assert!(!mentions_handle(
            "cc @lightbridge-assistant",
            "lightbridge-assistant"
        ));
        assert!(!mentions_handle(
            "just a normal comment",
            "lightbridge-assistant"
        ));
        assert!(!mentions_handle(
            "@someone-else go",
            "lightbridge-assistant"
        ));
    }

    #[test]
    fn command_from_comment_keeps_the_whole_message() {
        // The full comment is preserved (handle NOT stripped), surrounding whitespace trimmed — so the
        // agent sees its own name and any co-mentions and interprets them itself.
        assert_eq!(
            command_from_comment("@lightbridge-assistant please review this"),
            "@lightbridge-assistant please review this"
        );
        // Co-mention that the old handle-stripping mangled into "& /gemini please review this".
        assert_eq!(
            command_from_comment("  @lightbridge-assistant & /gemini please review this  "),
            "@lightbridge-assistant & /gemini please review this"
        );
        // Multiline body kept intact (trimmed at the ends).
        assert_eq!(
            command_from_comment("@lightbridge-assistant review this\nand check error handling"),
            "@lightbridge-assistant review this\nand check error handling"
        );
    }

    #[test]
    fn command_from_comment_bounds_length() {
        let long = format!("@bot {}", "x".repeat(MAX_INSTRUCTION_CHARS + 500));
        assert_eq!(
            command_from_comment(&long).chars().count(),
            MAX_INSTRUCTION_CHARS
        );
    }

    #[test]
    fn repo_identity_parses_full_name() {
        let repo = serde_json::json!({ "id": 99, "full_name": "octo/Hello-World" });
        assert_eq!(repo_identity(&repo), Some((99, "octo", "Hello-World")));
        // Missing id / full_name, or a malformed full_name → None (skipped, not panicked).
        assert_eq!(repo_identity(&serde_json::json!({ "id": 1 })), None);
        assert_eq!(
            repo_identity(&serde_json::json!({ "id": 1, "full_name": "noslash" })),
            None
        );
    }

    #[test]
    fn rejects_when_secret_unset() {
        assert!(!verify_signature(b"", b"body", "sha256=anything"));
    }

    #[test]
    fn accepts_a_valid_signature() {
        let secret = b"it is a secret";
        let body = b"payload";
        let mut mac = HmacSha256::new_from_slice(secret).unwrap();
        mac.update(body);
        let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        assert!(verify_signature(secret, body, &sig));
    }

    #[test]
    fn rejects_a_tampered_signature() {
        assert!(!verify_signature(b"secret", b"payload", "sha256=deadbeef"));
    }

    /// A deployment with `GITHUB_WEBHOOK_SECRET` set but no GitHub App (`GITHUB_APP_ID`/
    /// `GITHUB_APP_PRIVATE_KEY`, so `state.github` is `None` and `state.platforms` has no GitHub
    /// entry) must still verify a correctly-signed webhook — signature verification never required
    /// App credentials before #504's `CodePlatform` wiring, and must not start requiring them now
    /// (a real regression a bot review caught: this used to 503 every such webhook).
    fn github_secret_only_state(secret: &str) -> AppState {
        AppState {
            github_webhook_secret: std::sync::Arc::new(secret.to_string()),
            seen_deliveries: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
            jwt: None,
            db: None,
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
            model_allowlist: std::sync::Arc::new(Vec::new()),
        }
    }

    fn github_request(secret: &[u8], body: &[u8], delivery_id: &str) -> (HeaderMap, Bytes) {
        let mut mac = HmacSha256::new_from_slice(secret).unwrap();
        mac.update(body);
        let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "ping".parse().unwrap());
        headers.insert("x-github-delivery", delivery_id.parse().unwrap());
        headers.insert("x-hub-signature-256", sig.parse().unwrap());
        (headers, Bytes::from(body.to_vec()))
    }

    #[tokio::test]
    async fn github_webhook_verifies_via_secret_fallback_when_no_app_is_configured() {
        let state = github_secret_only_state("wh-secret");
        let body = br#"{"zen":"hi"}"#;
        let (headers, body) = github_request(b"wh-secret", body, "no-app-delivery-1");

        let response = github_webhook_body(state, headers, body).await;
        assert_eq!(
            response.status(),
            StatusCode::ACCEPTED,
            "a validly-signed webhook must verify via the secret fallback, not 503, when only \
             GITHUB_WEBHOOK_SECRET is set and no GitHub App is registered"
        );
    }

    #[tokio::test]
    async fn github_webhook_secret_fallback_still_rejects_a_tampered_signature() {
        let state = github_secret_only_state("wh-secret");
        let body = br#"{"zen":"hi"}"#;
        let (mut headers, body) = github_request(b"wh-secret", body, "no-app-delivery-2");
        headers.insert("x-hub-signature-256", "sha256=deadbeef".parse().unwrap());

        let response = github_webhook_body(state, headers, body).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn github_webhook_503s_only_when_truly_unconfigured() {
        // No App AND no webhook secret — genuinely nothing to verify against.
        let state = github_secret_only_state("");
        let body = br#"{"zen":"hi"}"#;
        let (headers, body) = github_request(b"anything", body, "no-app-delivery-3");

        let response = github_webhook_body(state, headers, body).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn pr_author_is_bot_detects_type_bot() {
        let pr = serde_json::json!({ "user": { "login": "dependabot[bot]", "type": "Bot" } });
        assert!(pr_author_is_bot(&pr));
    }

    #[test]
    fn pr_author_is_bot_backstops_on_login_suffix() {
        // `type` absent/unexpected but the login still carries the `[bot]` suffix.
        let pr = serde_json::json!({ "user": { "login": "renovate[bot]", "type": "User" } });
        assert!(pr_author_is_bot(&pr));
        let pr = serde_json::json!({ "user": { "login": "some-app[bot]" } });
        assert!(pr_author_is_bot(&pr));
    }

    #[test]
    fn pr_author_is_bot_treats_human_as_not_bot() {
        let pr = serde_json::json!({ "user": { "login": "octocat", "type": "User" } });
        assert!(!pr_author_is_bot(&pr));
    }

    #[test]
    fn pr_author_is_bot_fails_open_on_garbled_payload() {
        // No `type`, no `[bot]` login, or no `user` at all — never silently drop a real PR.
        assert!(!pr_author_is_bot(&serde_json::json!({})));
        assert!(!pr_author_is_bot(
            &serde_json::json!({ "user": { "login": "octocat" } })
        ));
        assert!(!pr_author_is_bot(&serde_json::json!({ "user": {} })));
    }

    #[test]
    fn should_skip_bot_review_only_when_enabled_and_bot() {
        let bot_pr = serde_json::json!({ "user": { "login": "dependabot[bot]", "type": "Bot" } });
        let human_pr = serde_json::json!({ "user": { "login": "octocat", "type": "User" } });

        assert!(should_skip_bot_review(true, &bot_pr));
        // Knob disabled → auto-review proceeds exactly as today, even for a bot author.
        assert!(!should_skip_bot_review(false, &bot_pr));
        // Human author is never skipped, knob on or off.
        assert!(!should_skip_bot_review(true, &human_pr));
        assert!(!should_skip_bot_review(false, &human_pr));
    }

    // Platform detection from headers is no longer used — platform is known at the path level
    // (/webhook/github, /webhook/gitlab/{id}, /webhook/bitbucket/{id}).
    // The old detect_platform tests are removed accordingly.

    #[test]
    fn bitbucket_repo_identity_parses_full_name() {
        let payload = serde_json::json!({ "repository": { "full_name": "myteam/my-repo" } });
        assert_eq!(
            bitbucket_repo_identity(&payload),
            Some((
                "myteam/my-repo".to_string(),
                "myteam".to_string(),
                "my-repo".to_string()
            ))
        );
    }

    #[test]
    fn bitbucket_repo_identity_missing_full_name() {
        assert_eq!(bitbucket_repo_identity(&serde_json::json!({})), None);
        assert_eq!(
            bitbucket_repo_identity(
                &serde_json::json!({ "repository": { "full_name": "noslash" } })
            ),
            None
        );
    }

    fn bitbucket_registry() -> crate::integrations::bitbucket::BitbucketRegistry {
        let section = crate::config::BitbucketSection {
            enabled: true,
            default_api_url: Some("https://api.bitbucket.example.com/2.0".to_string()),
            default_bot_handle: Some("lightbridge-bot".to_string()),
            projects: vec![
                crate::config::BitbucketProjectConfig {
                    workspace: "myteam".to_string(),
                    repo_slug: "repo-a".to_string(),
                    api_url: None,
                    email: "bot@example.com".to_string(),
                    api_token: "token-a".to_string(),
                    webhook_secret: "secret-a".to_string(),
                    bot_handle: None,
                },
                crate::config::BitbucketProjectConfig {
                    workspace: "myteam".to_string(),
                    repo_slug: "repo-b".to_string(),
                    api_url: None,
                    email: "bot@example.com".to_string(),
                    api_token: "token-b".to_string(),
                    webhook_secret: "secret-b".to_string(),
                    bot_handle: None,
                },
            ],
        };
        crate::integrations::bitbucket::BitbucketRegistry::from_config(&section)
            .expect("valid config")
            .expect("enabled registry")
    }

    #[test]
    fn bitbucket_project_webhook_accepts_matching_repo_secret() {
        let registry = bitbucket_registry();
        let body = br#"{"repository":{"full_name":"myteam/repo-a"}}"#;
        let installation_id = crate::integrations::platform::stable_id_from_key("myteam/repo-a");
        use hmac::{Hmac, KeyInit, Mac};
        use sha2::Sha256;
        type HmacSha256 = Hmac<Sha256>;
        let mut mac = HmacSha256::new_from_slice(b"secret-a").unwrap();
        mac.update(body);
        let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let mut headers = HeaderMap::new();
        headers.insert("x-hub-signature", sig.parse().unwrap());
        assert!(verify_bitbucket_project_webhook_with_registry(
            Some(&registry),
            &headers,
            body,
            installation_id,
        ));
    }
    #[test]
    fn bitbucket_project_webhook_rejects_wrong_repo_secret() {
        let registry = bitbucket_registry();
        let body = br#"{"repository":{"full_name":"myteam/repo-a"}}"#;
        let installation_id = crate::integrations::platform::stable_id_from_key("myteam/repo-a");
        use hmac::{Hmac, KeyInit, Mac};
        use sha2::Sha256;
        type HmacSha256 = Hmac<Sha256>;
        let mut mac = HmacSha256::new_from_slice(b"secret-b").unwrap();
        mac.update(body);
        let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let mut headers = HeaderMap::new();
        headers.insert("x-hub-signature", sig.parse().unwrap());
        assert!(!verify_bitbucket_project_webhook_with_registry(
            Some(&registry),
            &headers,
            body,
            installation_id,
        ));
    }
    #[test]
    fn bitbucket_project_webhook_rejects_unknown_installation_id() {
        let registry = bitbucket_registry();
        let headers = HeaderMap::new();
        assert!(!verify_bitbucket_project_webhook_with_registry(
            Some(&registry),
            &headers,
            b"{}",
            9999,
        ));
    }

    fn gitlab_registry() -> crate::integrations::gitlab::GitlabRegistry {
        let section = crate::config::GitlabSection {
            enabled: true,
            default_api_url: Some("https://gitlab.example.com/api/v4".to_string()),
            default_bot_handle: Some("lightbridge-bot".to_string()),
            projects: vec![
                crate::config::GitlabProjectConfig {
                    project_id: 1001,
                    installation_id: None, // effective = 1001
                    api_url: None,
                    access_token: "token-a".to_string(),
                    webhook_secret: "secret-a".to_string(),
                    bot_handle: None,
                },
                crate::config::GitlabProjectConfig {
                    project_id: 1002,
                    installation_id: Some(9000), // custom installation_id
                    api_url: None,
                    access_token: "token-b".to_string(),
                    webhook_secret: "secret-b".to_string(),
                    bot_handle: None,
                },
            ],
        };
        crate::integrations::gitlab::GitlabRegistry::from_config(&section)
            .expect("valid config")
            .expect("enabled registry")
    }

    #[test]
    fn gitlab_project_webhook_accepts_matching_secret() {
        let registry = gitlab_registry();
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-token", "secret-a".parse().unwrap());
        assert!(matches!(
            verify_gitlab_webhook_with_registry(Some(&registry), &headers, b"{}", 1001),
            GitlabVerifyResult::Ok
        ));
    }

    #[test]
    fn gitlab_project_webhook_rejects_wrong_secret() {
        let registry = gitlab_registry();
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-token", "wrong".parse().unwrap());
        assert!(matches!(
            verify_gitlab_webhook_with_registry(Some(&registry), &headers, b"{}", 1001),
            GitlabVerifyResult::InvalidSignature
        ));
    }

    #[test]
    fn gitlab_project_webhook_rejects_unknown_installation_id() {
        let registry = gitlab_registry();
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-token", "secret-a".parse().unwrap());
        assert!(matches!(
            verify_gitlab_webhook_with_registry(Some(&registry), &headers, b"{}", 9999),
            GitlabVerifyResult::UnknownInstallationId
        ));
    }

    #[test]
    fn gitlab_project_webhook_rejects_raw_project_id_when_custom_installation_id_set() {
        // project 1002 uses installation_id=9000; the raw project_id must not be accepted.
        let registry = gitlab_registry();
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-token", "secret-b".parse().unwrap());
        assert!(matches!(
            verify_gitlab_webhook_with_registry(Some(&registry), &headers, b"{}", 1002),
            GitlabVerifyResult::UnknownInstallationId
        ));
    }

    #[test]
    fn gitlab_project_webhook_accepts_custom_installation_id() {
        let registry = gitlab_registry();
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-token", "secret-b".parse().unwrap());
        assert!(matches!(
            verify_gitlab_webhook_with_registry(Some(&registry), &headers, b"{}", 9000),
            GitlabVerifyResult::Ok
        ));
    }

    #[test]
    fn gitlab_path_split_simple() {
        assert_eq!(gitlab_path_split("group/repo"), Some(("group", "repo")));
    }

    #[test]
    fn gitlab_path_split_nested_subgroup() {
        assert_eq!(
            gitlab_path_split("group/sub/repo"),
            Some(("group/sub", "repo"))
        );
    }

    #[test]
    fn gitlab_path_split_no_slash() {
        assert_eq!(gitlab_path_split("noslash"), None);
    }

    #[test]
    fn gitlab_project_identity_extracts_fields() {
        let project = serde_json::json!({
            "id": 42,
            "path_with_namespace": "group/sub/repo",
            "default_branch": "main"
        });
        assert_eq!(
            gitlab_project_identity(&project),
            Some((42, "group/sub", "repo", "main"))
        );
    }

    #[test]
    fn gitlab_project_identity_missing_id() {
        let project = serde_json::json!({
            "path_with_namespace": "group/repo",
            "default_branch": "main"
        });
        assert_eq!(gitlab_project_identity(&project), None);
    }

    #[test]
    fn gitlab_project_identity_missing_path() {
        let project = serde_json::json!({
            "id": 42,
            "default_branch": "main"
        });
        assert_eq!(gitlab_project_identity(&project), None);
    }

    #[test]
    fn gitlab_project_identity_defaults_branch() {
        let project = serde_json::json!({
            "id": 42,
            "path_with_namespace": "group/repo"
        });
        assert_eq!(
            gitlab_project_identity(&project),
            Some((42, "group", "repo", "main"))
        );
    }

    #[test]
    fn gitlab_bot_review_skips_known_bots() {
        assert!(should_skip_gitlab_bot_review(true, "dependabot_bot"));
        assert!(should_skip_gitlab_bot_review(true, "gitlab-bot"));
        assert!(should_skip_gitlab_bot_review(true, "GitLab"));
    }

    #[test]
    fn gitlab_bot_review_fails_open_on_empty() {
        assert!(!should_skip_gitlab_bot_review(true, ""));
    }

    #[test]
    fn gitlab_bot_review_treats_humans_as_not_bot() {
        assert!(!should_skip_gitlab_bot_review(true, "octocat"));
        assert!(!should_skip_gitlab_bot_review(true, "Alice"));
    }

    #[test]
    fn gitlab_bot_review_disabled_knob_never_skips() {
        assert!(!should_skip_gitlab_bot_review(false, "dependabot_bot"));
    }

    // ── #502 webhook-ingress step wrap: proves wrapping the delivery dedup+persist write in
    // `Passthrough.step(...)` is a pure naming exercise, not a behavior change (needs Postgres via
    // `DATABASE_URL`) — mirrors the ADR-0107 proof style in `queue::dispatcher`/`queue::reconciler`
    // ────────────────────────────────────────────────────────────────────────────────────────────

    use sqlx::PgPool;

    /// Build a minimal `AppState` wired only for a GitLab per-project webhook — the dedup+persist
    /// block under test is shared code, platform-agnostic, and GitLab's own signature check goes
    /// through `state.gitlab` rather than `state.platforms`, so no GitHub App / `platforms` entry is
    /// needed to exercise it.
    fn gitlab_only_state(pool: PgPool) -> AppState {
        AppState {
            github_webhook_secret: std::sync::Arc::new(String::new()),
            seen_deliveries: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
            jwt: None,
            db: Some(pool),
            allow_no_db: true,
            github: None,
            gitlab: Some(gitlab_registry()),
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
        }
    }

    /// A GitLab webhook whose event type (`Job Hook`) `route_gitlab_event` doesn't handle — it falls
    /// into the catch-all debug-log arm — so the request exercises only the dedup+persist block under
    /// test, not the downstream MR/push/note handling.
    fn gitlab_job_hook_request() -> (HeaderMap, Bytes) {
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-event", "Job Hook".parse().unwrap());
        headers.insert("x-gitlab-token", "secret-a".parse().unwrap());
        headers.insert("x-gitlab-event-uuid", "wrap-test-uuid".parse().unwrap());
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({ "project": { "id": 1001 } })).unwrap(),
        );
        (headers, body)
    }

    /// The wrapped dedup+persist write behaves exactly as the un-wrapped code did: a fresh delivery
    /// is accepted and persisted, and a replay of the *same* delivery id is deduped — no second row —
    /// proving `Passthrough.step(...)` (verbatim `f().await`) changed nothing observable.
    #[sqlx::test]
    async fn webhook_ingress_step_wrap_dedups_a_replayed_delivery(pool: PgPool) {
        let state = gitlab_only_state(pool.clone());
        let (headers, body) = gitlab_job_hook_request();

        let first = gitlab_webhook_body(state.clone(), 1001, headers.clone(), body.clone()).await;
        assert_eq!(first.status(), StatusCode::ACCEPTED);

        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM webhook_deliveries WHERE delivery_id = $1")
                .bind("wrap-test-uuid")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 1, "the fresh delivery is persisted exactly once");

        let second = gitlab_webhook_body(state, 1001, headers, body).await;
        assert_eq!(
            second.status(),
            StatusCode::ACCEPTED,
            "a replayed delivery id is still a 202 (duplicate delivery), same as before the wrap"
        );

        let count_after_replay: i64 =
            sqlx::query_scalar("SELECT count(*) FROM webhook_deliveries WHERE delivery_id = $1")
                .bind("wrap-test-uuid")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            count_after_replay, 1,
            "the replay must not insert a second row — the wrap must not change dedup semantics"
        );
    }

    /// A persistence error inside the wrapped step still surfaces as the same 500 the un-wrapped code
    /// returned. A payload containing a bare NUL byte is rejected by Postgres's `jsonb` input
    /// ("unsupported Unicode escape sequence"), giving a real, deterministic `sqlx::Error` without
    /// tearing down the pool — the same failure mode `record_delivery` can hit in production.
    #[sqlx::test]
    async fn webhook_ingress_step_wrap_surfaces_persistence_errors_as_500(pool: PgPool) {
        let state = gitlab_only_state(pool);
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-event", "Job Hook".parse().unwrap());
        headers.insert("x-gitlab-token", "secret-a".parse().unwrap());
        headers.insert("x-gitlab-event-uuid", "wrap-error-uuid".parse().unwrap());
        let body = Bytes::from(
            serde_json::to_vec(
                &serde_json::json!({ "project": { "id": 1001 }, "poison": "\u{0000}" }),
            )
            .unwrap(),
        );

        let response = gitlab_webhook_body(state, 1001, headers, body).await;
        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "a persistence failure inside the step wrap still returns 500, same as before the wrap"
        );
    }

    /// End-to-end: an MR-open webhook fetches the repo's `.lightbridge-code-review.jsonc` from the
    /// GitLab API (ADR-0030/ADR-0103) and the created task carries the resolved `preset` — proving the
    /// whole chain (webhook → `resolve_preset` → `CodePlatform::get_repo_file` → JSONC parse → DB
    /// insert), not just the pure resolver function in isolation.
    #[sqlx::test]
    async fn mr_open_resolves_a_custom_preset_from_repo_config(pool: PgPool) {
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/api/v4/projects/acme%2Fwidgets/repository/files/.lightbridge-code-review.jsonc/raw",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_string(r#"{"preset": "ultra"}"#),
            )
            .mount(&mock)
            .await;

        let section = crate::config::GitlabSection {
            enabled: true,
            default_api_url: Some("https://gitlab.example.com/api/v4".to_string()),
            default_bot_handle: Some("lightbridge-bot".to_string()),
            projects: vec![crate::config::GitlabProjectConfig {
                project_id: 2001,
                installation_id: None,
                api_url: Some(format!("{}/api/v4", mock.uri())),
                access_token: "token-preset-test".to_string(),
                webhook_secret: "preset-secret".to_string(),
                bot_handle: None,
            }],
        };
        let registry = crate::integrations::gitlab::GitlabRegistry::from_config(&section)
            .expect("valid config")
            .expect("enabled registry");

        let repo_id = crate::db::upsert_repository(
            &pool,
            Platform::GitLab,
            2001,
            "acme",
            "widgets",
            "main",
            Some(2001),
        )
        .await
        .unwrap();
        sqlx::query("UPDATE repositories SET status = 'approved' WHERE id = $1")
            .bind(repo_id)
            .execute(&pool)
            .await
            .unwrap();

        let mut state = gitlab_only_state(pool.clone());
        state.gitlab = Some(registry);

        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-event", "Merge Request Hook".parse().unwrap());
        headers.insert("x-gitlab-token", "preset-secret".parse().unwrap());
        headers.insert("x-gitlab-event-uuid", "preset-test-uuid".parse().unwrap());
        let payload = serde_json::json!({
            "object_attributes": {
                "action": "open",
                "iid": 7,
                "diff_refs": { "base_sha": "base123", "head_sha": "head456" },
                "last_commit": { "author": { "name": "A Human" } },
            },
            "project": {
                "id": 2001,
                "path_with_namespace": "acme/widgets",
                "default_branch": "main",
            },
            "user": { "username": "a-human" },
        });
        let body = Bytes::from(serde_json::to_vec(&payload).unwrap());

        let response = gitlab_webhook_body(state, 2001, headers, body).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let (preset, entry_point): (String, String) = sqlx::query_as(
            "SELECT preset, entry_point FROM tasks WHERE repository_id = $1 AND target_id = 7",
        )
        .bind(repo_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            preset, "ultra",
            "the repo's .lightbridge-code-review.jsonc preset was resolved end-to-end"
        );
        assert_eq!(entry_point, "pr_open");
    }

    /// Epic #566: a repo that turns the automatic on-open review OFF creates no task at all when an
    /// MR opens. Exercised end-to-end through the real webhook path, once per config layer.
    ///
    /// `mr_open_with_review_on_open_disabled_creates_no_task` covers the DB-override layer;
    /// the file layer is covered by the sibling test below, so a regression in either layer is caught.
    #[sqlx::test]
    async fn mr_open_with_review_on_open_disabled_creates_no_task(pool: PgPool) {
        let mock = wiremock::MockServer::start().await;
        // No repo config file — so the OFF decision can only come from the DB override.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/api/v4/projects/acme%2Fgated/repository/files/.lightbridge-code-review.jsonc/raw",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&mock)
            .await;

        let repo_id = seed_gated_gitlab_repo(&pool, "gated", 2401).await;
        crate::db::set_repo_settings(
            &pool,
            repo_id,
            &crate::db::RepoSettingsPatch {
                review_on_pr_open: Some(Some(false)),
                ..Default::default()
            },
            "tester",
        )
        .await
        .unwrap();

        let state = gated_gitlab_state(pool.clone(), &mock, 2401);
        let response = gitlab_webhook_body(
            state,
            2401,
            gated_mr_headers("gated-off"),
            gated_mr_body("acme/gated", 2401),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::ACCEPTED,
            "the webhook is still accepted"
        );

        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks WHERE repository_id = $1")
            .bind(repo_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            count, 0,
            "review_on_pr_open=false must create NO task for an opened MR"
        );
    }

    /// The control for the test above: with no override and no config, the built-in default (`true`)
    /// applies and the task IS created. Without this pair, a bug that stopped creating tasks entirely
    /// would still let the OFF assertion pass.
    #[sqlx::test]
    async fn mr_open_creates_a_task_when_review_on_open_is_left_at_its_default(pool: PgPool) {
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/api/v4/projects/acme%2Fungated/repository/files/.lightbridge-code-review.jsonc/raw",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&mock)
            .await;

        let repo_id = seed_gated_gitlab_repo(&pool, "ungated", 2402).await;
        let state = gated_gitlab_state(pool.clone(), &mock, 2402);
        let response = gitlab_webhook_body(
            state,
            2402,
            gated_mr_headers("gated-on"),
            gated_mr_body("acme/ungated", 2402),
        )
        .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks WHERE repository_id = $1")
            .bind(repo_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            count, 1,
            "the default (true) still creates the on-open review"
        );
    }

    /// The FILE layer can disable it too, with no DB row involved.
    #[sqlx::test]
    async fn mr_open_respects_review_on_open_false_from_the_repo_config_file(pool: PgPool) {
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/api/v4/projects/acme%2Ffilegated/repository/files/.lightbridge-code-review.jsonc/raw",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(
                r#"{ "triggers": { "review_on_open": false } }"#,
            ))
            .mount(&mock)
            .await;

        let repo_id = seed_gated_gitlab_repo(&pool, "filegated", 2403).await;
        let state = gated_gitlab_state(pool.clone(), &mock, 2403);
        let response = gitlab_webhook_body(
            state,
            2403,
            gated_mr_headers("gated-file"),
            gated_mr_body("acme/filegated", 2403),
        )
        .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks WHERE repository_id = $1")
            .bind(repo_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            count, 0,
            "the repo config file alone must be able to disable the on-open review"
        );
    }

    // --- shared fixtures for the review_on_open gate tests ---

    async fn seed_gated_gitlab_repo(pool: &PgPool, name: &str, project_id: i64) -> i64 {
        let repo_id = crate::db::upsert_repository(
            pool,
            Platform::GitLab,
            project_id,
            "acme",
            name,
            "main",
            Some(project_id),
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

    fn gated_gitlab_state(pool: PgPool, mock: &wiremock::MockServer, project_id: i64) -> AppState {
        let section = crate::config::GitlabSection {
            enabled: true,
            default_api_url: Some("https://gitlab.example.com/api/v4".to_string()),
            default_bot_handle: Some("lightbridge-bot".to_string()),
            projects: vec![crate::config::GitlabProjectConfig {
                project_id,
                installation_id: None,
                api_url: Some(format!("{}/api/v4", mock.uri())),
                access_token: "token-gate-test".to_string(),
                webhook_secret: "gate-secret".to_string(),
                bot_handle: None,
            }],
        };
        let registry = crate::integrations::gitlab::GitlabRegistry::from_config(&section)
            .expect("valid config")
            .expect("enabled registry");
        let mut state = gitlab_only_state(pool);
        state.gitlab = Some(registry);
        state
    }

    fn gated_mr_headers(uuid: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-event", "Merge Request Hook".parse().unwrap());
        headers.insert("x-gitlab-token", "gate-secret".parse().unwrap());
        headers.insert("x-gitlab-event-uuid", uuid.parse().unwrap());
        headers
    }

    fn gated_mr_body(path_with_namespace: &str, project_id: i64) -> Bytes {
        gated_mr_body_with_action(path_with_namespace, project_id, "open", None)
    }

    /// Epic #566: an `update` event optionally carrying `oldrev` — GitLab's only reliable "the head
    /// SHA actually moved" signal (it also sends `update` for label/title/description edits, which
    /// carry no `oldrev`).
    fn gated_mr_body_with_action(
        path_with_namespace: &str,
        project_id: i64,
        action: &str,
        oldrev: Option<&str>,
    ) -> Bytes {
        gated_mr_body_full(
            path_with_namespace,
            project_id,
            action,
            "head-gate-2",
            oldrev,
        )
    }

    /// Same as [`gated_mr_body_with_action`] but with the head SHA parametrized, so a test can push
    /// through two distinct heads (e.g. an `open` then an `update`) on the same MR.
    fn gated_mr_body_full(
        path_with_namespace: &str,
        project_id: i64,
        action: &str,
        head_sha: &str,
        oldrev: Option<&str>,
    ) -> Bytes {
        let mut object_attributes = serde_json::json!({
            "action": action,
            "iid": 11,
            "diff_refs": { "base_sha": "base-gate", "head_sha": head_sha },
            "last_commit": { "author": { "name": "A Human" } },
        });
        if let Some(oldrev) = oldrev {
            object_attributes["oldrev"] = serde_json::Value::String(oldrev.to_string());
        }
        let payload = serde_json::json!({
            "object_attributes": object_attributes,
            "project": {
                "id": project_id,
                "path_with_namespace": path_with_namespace,
                "default_branch": "main",
            },
            "user": { "username": "a-human" },
        });
        Bytes::from(serde_json::to_vec(&payload).unwrap())
    }

    // --- epic #566: review-on-push (MR `update`) ---

    /// The single most important test in this group: `review_on_push` defaults to OFF, so an `update`
    /// event with a genuine `oldrev` — the strongest possible "a push happened" signal — must still
    /// create NO task when the repo has configured nothing.
    #[sqlx::test]
    async fn mr_update_with_oldrev_is_a_noop_by_default(pool: PgPool) {
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/api/v4/projects/acme%2Fsyncdefault/repository/files/.lightbridge-code-review.jsonc/raw",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&mock)
            .await;

        let repo_id = seed_gated_gitlab_repo(&pool, "syncdefault", 2404).await;
        let state = gated_gitlab_state(pool.clone(), &mock, 2404);
        let response = gitlab_webhook_body(
            state,
            2404,
            gated_mr_headers("sync-default"),
            gated_mr_body_with_action("acme/syncdefault", 2404, "update", Some("old-sha")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks WHERE repository_id = $1")
            .bind(repo_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            count, 0,
            "review_on_push defaults to OFF; a push must not create a task unless opted in"
        );
    }

    /// An `update` with NO `oldrev` is a metadata-only edit (label/title/description) — even with
    /// `review_on_push` explicitly enabled, it must not create a task.
    #[sqlx::test]
    async fn mr_update_without_oldrev_is_ignored_as_metadata_only(pool: PgPool) {
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/api/v4/projects/acme%2Fmetaonly/repository/files/.lightbridge-code-review.jsonc/raw",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&mock)
            .await;

        let repo_id = seed_gated_gitlab_repo(&pool, "metaonly", 2405).await;
        crate::db::set_repo_settings(
            &pool,
            repo_id,
            &crate::db::RepoSettingsPatch {
                review_on_push: Some(Some(true)),
                ..Default::default()
            },
            "tester",
        )
        .await
        .unwrap();

        let state = gated_gitlab_state(pool.clone(), &mock, 2405);
        let response = gitlab_webhook_body(
            state,
            2405,
            gated_mr_headers("sync-metaonly"),
            gated_mr_body_with_action("acme/metaonly", 2405, "update", None),
        )
        .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks WHERE repository_id = $1")
            .bind(repo_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            count, 0,
            "an update with no oldrev is a metadata edit, not a push — no task, even when opted in"
        );
    }

    /// The positive case: `review_on_push` enabled via DB override, an `update` WITH `oldrev` — a
    /// genuine push — creates exactly one task, tagged with the new `pr_sync` entry point.
    #[sqlx::test]
    async fn mr_update_with_oldrev_creates_a_pr_sync_task_when_enabled(pool: PgPool) {
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/api/v4/projects/acme%2Fsyncon/repository/files/.lightbridge-code-review.jsonc/raw",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&mock)
            .await;

        let repo_id = seed_gated_gitlab_repo(&pool, "syncon", 2406).await;
        crate::db::set_repo_settings(
            &pool,
            repo_id,
            &crate::db::RepoSettingsPatch {
                review_on_push: Some(Some(true)),
                ..Default::default()
            },
            "tester",
        )
        .await
        .unwrap();

        let state = gated_gitlab_state(pool.clone(), &mock, 2406);
        let response = gitlab_webhook_body(
            state,
            2406,
            gated_mr_headers("sync-on"),
            gated_mr_body_with_action("acme/syncon", 2406, "update", Some("old-sha")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let (entry_point, preset, head_sha): (String, String, Option<String>) = sqlx::query_as(
            "SELECT entry_point, preset, head_sha FROM tasks WHERE repository_id = $1",
        )
        .bind(repo_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(entry_point, "pr_sync");
        assert_eq!(
            preset, "fast",
            "pr_sync defaults to the cheap tier — it fires once per push"
        );
        assert_eq!(head_sha.as_deref(), Some("head-gate-2"));
    }

    /// A draft MR must not be reviewed on push either — same rationale as the existing open-time
    /// draft skip, now also covering the sync path.
    #[sqlx::test]
    async fn mr_update_skips_a_draft_mr_even_when_review_on_push_is_enabled(pool: PgPool) {
        let mock = wiremock::MockServer::start().await;
        let repo_id = seed_gated_gitlab_repo(&pool, "syncdraft", 2407).await;
        crate::db::set_repo_settings(
            &pool,
            repo_id,
            &crate::db::RepoSettingsPatch {
                review_on_push: Some(Some(true)),
                ..Default::default()
            },
            "tester",
        )
        .await
        .unwrap();

        let mut body_value: serde_json::Value = serde_json::from_slice(&gated_mr_body_with_action(
            "acme/syncdraft",
            2407,
            "update",
            Some("old-sha"),
        ))
        .unwrap();
        body_value["object_attributes"]["draft"] = serde_json::Value::Bool(true);
        let body = Bytes::from(serde_json::to_vec(&body_value).unwrap());

        let state = gated_gitlab_state(pool.clone(), &mock, 2407);
        let response = gitlab_webhook_body(state, 2407, gated_mr_headers("sync-draft"), body).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks WHERE repository_id = $1")
            .bind(repo_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "a draft MR must not be reviewed on push");
    }

    // --- epic #566: push-storm strategies (`supersede` / `debounce`) ---

    /// The positive case for `supersede` (the default push strategy): a push while the PR's earlier
    /// automatic review is still queued cancels that earlier task — rather than letting both run and
    /// post two reviews for what's now a stale head — and resolves its check run to `Cancelled` so it
    /// doesn't hang "in progress" on the PR forever.
    #[sqlx::test]
    async fn mr_update_with_supersede_strategy_cancels_the_prs_earlier_automatic_review(
        pool: PgPool,
    ) {
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/api/v4/projects/acme%2Fsupersede/repository/files/.lightbridge-code-review.jsonc/raw",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&mock)
            .await;

        let repo_id = seed_gated_gitlab_repo(&pool, "supersede", 2408).await;
        crate::db::set_repo_settings(
            &pool,
            repo_id,
            &crate::db::RepoSettingsPatch {
                review_on_push: Some(Some(true)),
                push_strategy: Some(Some("supersede".to_string())),
                ..Default::default()
            },
            "tester",
        )
        .await
        .unwrap();

        // Opening the MR lands the PR's first automatic review at "head-gate-1" — never dispatched in
        // this test, so it's still sitting `queued` when the push below arrives.
        let state = gated_gitlab_state(pool.clone(), &mock, 2408);
        let opened = gitlab_webhook_body(
            state.clone(),
            2408,
            gated_mr_headers("supersede-open"),
            gated_mr_body_full("acme/supersede", 2408, "open", "head-gate-1", None),
        )
        .await;
        assert_eq!(opened.status(), StatusCode::ACCEPTED);
        let older_task: uuid::Uuid = sqlx::query_scalar(
            "SELECT id FROM tasks WHERE repository_id = $1 AND head_sha = 'head-gate-1'",
        )
        .bind(repo_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        // A push supersedes it.
        let updated = gitlab_webhook_body(
            state,
            2408,
            gated_mr_headers("supersede-push"),
            gated_mr_body_full(
                "acme/supersede",
                2408,
                "update",
                "head-gate-2",
                Some("head-gate-1"),
            ),
        )
        .await;
        assert_eq!(updated.status(), StatusCode::ACCEPTED);

        let older_status: String = sqlx::query_scalar("SELECT status FROM tasks WHERE id = $1")
            .bind(older_task)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            older_status, "cancelled",
            "the earlier automatic review must be superseded by the new push"
        );

        let newer_status: String = sqlx::query_scalar(
            "SELECT status FROM tasks WHERE repository_id = $1 AND head_sha = 'head-gate-2'",
        )
        .bind(repo_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            newer_status, "queued",
            "the new push's own task must survive — supersede must never cancel itself"
        );

        let cancelled_conclusion: String = sqlx::query_scalar(
            "SELECT payload->>'conclusion' FROM outbox \
             WHERE task_id = $1 AND kind = 'check_run_resolve'",
        )
        .bind(older_task)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            cancelled_conclusion, "cancelled",
            "the superseded task's check run must resolve to cancelled, not hang in progress"
        );
    }

    /// The positive case for `debounce`: a push under the `debounce` strategy lands its task with a
    /// future `run_after` set from the repo's configured quiet period — the dispatcher's claim query
    /// (`WHERE run_after <= now()`) simply won't offer it up until that window elapses.
    #[sqlx::test]
    async fn mr_update_with_debounce_strategy_delays_the_new_tasks_run_after(pool: PgPool) {
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/api/v4/projects/acme%2Fdebounce/repository/files/.lightbridge-code-review.jsonc/raw",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&mock)
            .await;

        let repo_id = seed_gated_gitlab_repo(&pool, "debounce", 2409).await;
        crate::db::set_repo_settings(
            &pool,
            repo_id,
            &crate::db::RepoSettingsPatch {
                review_on_push: Some(Some(true)),
                push_strategy: Some(Some("debounce".to_string())),
                push_debounce_seconds: Some(Some(120)),
                ..Default::default()
            },
            "tester",
        )
        .await
        .unwrap();

        let state = gated_gitlab_state(pool.clone(), &mock, 2409);
        let response = gitlab_webhook_body(
            state,
            2409,
            gated_mr_headers("debounce-push"),
            gated_mr_body_full(
                "acme/debounce",
                2409,
                "update",
                "head-gate-debounce",
                Some("old-sha"),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let (status, run_after, created_at): (String, time::OffsetDateTime, time::OffsetDateTime) =
            sqlx::query_as(
                "SELECT status, run_after, created_at FROM tasks WHERE repository_id = $1",
            )
            .bind(repo_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            status, "queued",
            "debounce delays claimability via run_after, not the stored status"
        );
        assert!(
            run_after - created_at >= time::Duration::seconds(115),
            "run_after must reflect the configured 120s debounce window (got {:?} after creation), \
             not fire immediately",
            run_after - created_at
        );

        let claimed = crate::db::claim_next_task(&pool, "w", std::time::Duration::from_secs(60))
            .await
            .unwrap();
        assert!(
            claimed.is_none(),
            "a debounced task must not be claimable before its run_after elapses"
        );
    }

    /// The same MR-open flow, but with no `.lightbridge-code-review.jsonc` on the repo at all (the
    /// GitLab API 404s) — the task falls back to the platform-default `pr_open` mapping (`fast`),
    /// reproducing today's ADR-0062 behavior exactly for a repo that configures nothing.
    #[sqlx::test]
    async fn mr_open_falls_back_to_the_platform_default_preset_when_no_repo_config_exists(
        pool: PgPool,
    ) {
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/api/v4/projects/acme%2Fno-config/repository/files/.lightbridge-code-review.jsonc/raw",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&mock)
            .await;

        let section = crate::config::GitlabSection {
            enabled: true,
            default_api_url: Some("https://gitlab.example.com/api/v4".to_string()),
            default_bot_handle: Some("lightbridge-bot".to_string()),
            projects: vec![crate::config::GitlabProjectConfig {
                project_id: 2002,
                installation_id: None,
                api_url: Some(format!("{}/api/v4", mock.uri())),
                access_token: "token-no-config-test".to_string(),
                webhook_secret: "no-config-secret".to_string(),
                bot_handle: None,
            }],
        };
        let registry = crate::integrations::gitlab::GitlabRegistry::from_config(&section)
            .expect("valid config")
            .expect("enabled registry");

        let repo_id = crate::db::upsert_repository(
            &pool,
            Platform::GitLab,
            2002,
            "acme",
            "no-config",
            "main",
            Some(2002),
        )
        .await
        .unwrap();
        sqlx::query("UPDATE repositories SET status = 'approved' WHERE id = $1")
            .bind(repo_id)
            .execute(&pool)
            .await
            .unwrap();

        let mut state = gitlab_only_state(pool.clone());
        state.gitlab = Some(registry);

        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-event", "Merge Request Hook".parse().unwrap());
        headers.insert("x-gitlab-token", "no-config-secret".parse().unwrap());
        headers.insert(
            "x-gitlab-event-uuid",
            "no-config-test-uuid".parse().unwrap(),
        );
        let payload = serde_json::json!({
            "object_attributes": {
                "action": "open",
                "iid": 9,
                "diff_refs": { "base_sha": "base789", "head_sha": "head012" },
                "last_commit": { "author": { "name": "A Human" } },
            },
            "project": {
                "id": 2002,
                "path_with_namespace": "acme/no-config",
                "default_branch": "main",
            },
            "user": { "username": "a-human" },
        });
        let body = Bytes::from(serde_json::to_vec(&payload).unwrap());

        let response = gitlab_webhook_body(state, 2002, headers, body).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let preset: String = sqlx::query_scalar(
            "SELECT preset FROM tasks WHERE repository_id = $1 AND target_id = 9",
        )
        .bind(repo_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            preset, "fast",
            "no repo config → the platform-default pr_open mapping applies (ADR-0062 behavior preserved)"
        );
    }
}
