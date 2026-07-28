//! Path-scoped webhook receivers (Epic #492 / #506 / #507).
//!
//! Three public handlers — one per forge — are mounted under `/api/v2/webhook/` by `main.rs`:
//!
//! - `POST /api/v2/webhook/github`                      → [`github_webhook`]
//! - `POST /api/v2/webhook/gitlab/{installation_id}`    → [`gitlab_webhook`]
//! - `POST /api/v2/webhook/bitbucket/{installation_id}` → [`bitbucket_webhook`]
//!
//! Platform is now known at the routing layer — no header-sniffing needed. Each handler verifies
//! its own signature (GitHub against `GITHUB_WEBHOOK_SECRET`; GitLab/Bitbucket against the
//! per-project/per-repo secret in `control-plane.json`, keyed by the path's `installation_id`),
//! dedupes on the platform's delivery ID via the shared [`record_or_dedup`] helper (itself
//! wrapped in the `StepRuntime` seam, ADR-0107/#502), then hands off to the same platform-specific
//! event routing as before. With a database, dedup + persistence happen atomically via the
//! `webhook_deliveries` PRIMARY KEY; without one (dev) it falls back to an in-memory set.
//!
//! The old unified `/webhook` and legacy `/github/webhook` routes are removed (Epic #492 hard
//! cutover). Existing webhook configurations must be repointed before deploying — see the rollout
//! checklist in EPIC_492_REPORT.md §5.2. Bitbucket's `installation_id` path segment is the
//! `platform::stable_id_from_key("workspace/repo_slug")` value (Bitbucket has no native numeric
//! project id like GitLab's), same identity `BitbucketRegistry` is keyed by.

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

/// `POST /api/v2/webhook/github` — GitHub webhook receiver (Epic #492 / #507).
///
/// Platform is now known at the path level; no header-sniffing. Verifies the HMAC-SHA256
/// signature via `GITHUB_WEBHOOK_SECRET`, dedupes, then routes to [`route_github_event`].
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
    if !verify_signature(
        state.github_webhook_secret.as_bytes(),
        &body,
        &header(&headers, "x-hub-signature-256"),
    ) {
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

    match record_or_dedup(&state, Platform::GitHub, &delivery_id, &event, &payload).await {
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "persistence error").into_response(),
        Some(false) => {
            crate::http::metrics::webhook_duplicate("github");
            tracing::info!(delivery_id, "github: duplicate delivery");
            return (StatusCode::ACCEPTED, "duplicate delivery").into_response();
        }
        Some(true) => {}
    }

    crate::http::metrics::webhook_delivery("github", &event);
    tracing::info!(delivery_id, %event, "github: accepted webhook");

    if state.db.is_some() {
        route_github_event(&state, &event, &payload, &delivery_id).await;
    }
    (StatusCode::ACCEPTED, "accepted").into_response()
}

/// `POST /api/v2/webhook/gitlab/{installation_id}` — GitLab webhook receiver (Epic #492 / #507).
///
/// The path now carries the installation (project) ID explicitly — no need to sniff it from the
/// JSON body before signature verification. Verifies the per-project token via the GitLab registry,
/// dedupes, then routes to [`route_gitlab_event`].
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
    // Parse + verify in one step: the registry looks up the per-project secret by project ID,
    // which now comes from the path rather than needing a pre-parse of the body.
    let payload =
        match verified_gitlab_payload_for_installation(&state, installation_id, &headers, &body) {
            Ok(p) => p,
            Err(GitlabPayloadError::InvalidJson) => {
                return (StatusCode::BAD_REQUEST, "invalid json payload").into_response();
            }
            Err(GitlabPayloadError::InvalidSignature) => {
                crate::http::metrics::webhook_signature_failure("gitlab");
                tracing::warn!(
                    platform = "gitlab",
                    installation_id,
                    "invalid webhook signature"
                );
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

    match record_or_dedup(&state, Platform::GitLab, &delivery_id, &event, &payload).await {
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "persistence error").into_response(),
        Some(false) => {
            crate::http::metrics::webhook_duplicate("gitlab");
            tracing::info!(delivery_id, "gitlab: duplicate delivery");
            return (StatusCode::ACCEPTED, "duplicate delivery").into_response();
        }
        Some(true) => {}
    }

    crate::http::metrics::webhook_delivery("gitlab", &event);
    tracing::info!(delivery_id, %event, installation_id, "gitlab: accepted webhook");

    if state.db.is_some() {
        route_gitlab_event(&state, &event, &payload, &delivery_id).await;
    }
    (StatusCode::ACCEPTED, "accepted").into_response()
}

/// `POST /api/v2/webhook/bitbucket/{installation_id}` — Bitbucket webhook receiver (Epic #492 /
/// #507, activated by Epic #353's Bitbucket `CodePlatform` implementation).
///
/// `installation_id` is `platform::stable_id_from_key("workspace/repo_slug")` — Bitbucket has no
/// native numeric project id like GitLab's, so the operator's webhook URL carries this derived
/// value instead (see [`crate::integrations::bitbucket`] and `docs/runbooks/bitbucket-platform-setup.md`).
/// Verifies the per-repo secret via the Bitbucket registry, dedupes, then routes to
/// [`route_bitbucket_event`].
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
                tracing::warn!(
                    platform = "bitbucket",
                    installation_id,
                    "invalid webhook signature"
                );
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

    match record_or_dedup(&state, Platform::Bitbucket, &delivery_id, &event, &payload).await {
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "persistence error").into_response(),
        Some(false) => {
            crate::http::metrics::webhook_duplicate("bitbucket");
            tracing::info!(delivery_id, "bitbucket: duplicate delivery");
            return (StatusCode::ACCEPTED, "duplicate delivery").into_response();
        }
        Some(true) => {}
    }

    crate::http::metrics::webhook_delivery("bitbucket", &event);
    tracing::info!(delivery_id, %event, installation_id, "bitbucket: accepted webhook");

    if state.db.is_some() {
        route_bitbucket_event(&state, &event, &payload, &delivery_id).await;
    }
    (StatusCode::ACCEPTED, "accepted").into_response()
}

/// Dedup helper shared by all per-forge handlers.
///
/// Returns `Some(true)` for a new delivery, `Some(false)` for a duplicate, `None` on a
/// persistence error (the caller should return 500).
///
/// The DB write is wrapped in the `StepRuntime` seam (ADR-0107/#502), keyed by the delivery's own
/// identity — `Passthrough` is the only runtime any role constructs today (`CheckpointRuntime`
/// promotion stays blocked on #363 regardless), and `Passthrough::step` is a bare `f().await`, so
/// this is a no-op wrap ahead of that promotion, applied uniformly across all three forges since
/// they now share this one helper.
async fn record_or_dedup(
    state: &AppState,
    platform: Platform,
    delivery_id: &str,
    event: &str,
    payload: &serde_json::Value,
) -> Option<bool> {
    match &state.db {
        Some(pool) => {
            let step_name = StepName::from(format!("webhook:{delivery_id}"));
            let step_result: Result<bool, StepError> = Passthrough
                .step(step_name, async || {
                    crate::db::record_delivery(pool, platform, delivery_id, event, payload)
                        .await
                        .map_err(|error| StepError::terminal(error.to_string()))
                })
                .await;
            step_result
                .map_err(|step_error| {
                    // The closure above only ever constructs `Terminal` (from the sqlx error's own
                    // Display text); `Transient` is unreachable today but handled so this stays
                    // exhaustive without silently swallowing a future variant.
                    let error = match step_error {
                        StepError::Terminal { reason } => reason,
                        StepError::Transient { source, .. } => source.to_string(),
                    };
                    tracing::error!(%error, delivery_id, "failed to persist delivery");
                })
                .ok()
        }
        None => Some(
            state
                .seen_deliveries
                .lock()
                .expect("dedup lock poisoned")
                .insert(delivery_id.to_string()),
        ),
    }
}

enum GitlabPayloadError {
    InvalidJson,
    InvalidSignature,
}

/// Parse and verify a GitLab webhook payload using the installation ID from the request path.
///
/// The `installation_id` (project ID) comes from the URL path segment
/// `/api/v2/webhook/gitlab/{installation_id}`, so we no longer need to pre-parse the body to
/// extract `project.id` before signature verification — the path already carries it.
fn verified_gitlab_payload_for_installation(
    state: &AppState,
    installation_id: i64,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<serde_json::Value, GitlabPayloadError> {
    let payload = match serde_json::from_slice(body) {
        Ok(p) => p,
        Err(error) => {
            tracing::error!(%error, installation_id, "gitlab webhook: invalid json payload");
            return Err(GitlabPayloadError::InvalidJson);
        }
    };

    if !verify_gitlab_project_webhook_with_registry(
        state.gitlab.as_ref(),
        headers,
        body,
        installation_id,
    ) {
        return Err(GitlabPayloadError::InvalidSignature);
    }

    Ok(payload)
}

/// Verify a GitLab webhook's `X-Gitlab-Token` against the per-project secret in the registry.
///
/// `project_id` now comes from the URL path segment rather than the JSON payload, so we no
/// longer need to parse the body before verification.
fn verify_gitlab_project_webhook_with_registry(
    registry: Option<&crate::integrations::gitlab::GitlabRegistry>,
    headers: &HeaderMap,
    body: &[u8],
    project_id: i64,
) -> bool {
    let Some(registry) = registry else {
        tracing::warn!(
            project_id,
            "GitLab webhook received but GitLab is not configured"
        );
        return false;
    };
    let Some(project) = registry.get(project_id) else {
        tracing::warn!(project_id, "GitLab webhook for unconfigured project");
        return false;
    };

    project.client.verify_webhook(headers, body)
}

enum BitbucketPayloadError {
    InvalidJson,
    InvalidSignature,
}

/// Parse and verify a Bitbucket webhook payload using the installation ID from the request path.
///
/// Mirrors [`verified_gitlab_payload_for_installation`]: the `installation_id`
/// (`platform::stable_id_from_key("workspace/repo_slug")`) comes from the URL path segment
/// `/api/v2/webhook/bitbucket/{installation_id}`, so we don't need to pre-parse the body to
/// derive the repo identity before signature verification — the path already carries it.
fn verified_bitbucket_payload_for_installation(
    state: &AppState,
    installation_id: i64,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<serde_json::Value, BitbucketPayloadError> {
    let payload = match serde_json::from_slice(body) {
        Ok(p) => p,
        Err(error) => {
            tracing::error!(%error, installation_id, "bitbucket webhook: invalid json payload");
            return Err(BitbucketPayloadError::InvalidJson);
        }
    };

    if !verify_bitbucket_project_webhook_with_registry(
        state.bitbucket.as_ref(),
        headers,
        body,
        installation_id,
    ) {
        return Err(BitbucketPayloadError::InvalidSignature);
    }

    Ok(payload)
}

/// Verify a Bitbucket webhook's `X-Hub-Signature` against the per-repo secret in the registry.
///
/// `installation_id` now comes from the URL path segment rather than the JSON payload, so we no
/// longer need to parse the body (or extract `repository.full_name`) before verification.
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
    let Some(project) = registry.get(installation_id) else {
        tracing::warn!(
            installation_id,
            "Bitbucket webhook for unconfigured repository"
        );
        return false;
    };

    project.client.verify_webhook(headers, body)
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
    if !matches!(action, "open" | "close") {
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
        "open" => {
            // Approval gate (Epic #75): a repo must be admin-approved before any review runs.
            if !approved_or_skip(pool, repository_id, delivery_id, mr_iid).await {
                return;
            }
            // Skip draft MRs (GitLab's equivalent of GitHub's draft PRs) — not ready for review.
            if attrs["draft"].as_bool() == Some(true) {
                tracing::info!(
                    delivery_id,
                    mr = mr_iid,
                    repository_id,
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
                tier: "fast".to_string(),
                trigger_comment_id: None,
                trace_context: lci_observability::current_traceparent(),
            };
            create_review_task(pool, task, delivery_id).await;
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

    let command_text = command_from_comment(body);
    let trigger_comment_id = payload["object_attributes"]["id"].as_i64();
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
        tier: "deep".to_string(),
        trigger_comment_id,
        trace_context: lci_observability::current_traceparent(),
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

/// `pullrequest:created` → the automatic first review. `pullrequest:fulfilled` (merged) /
/// `pullrequest:rejected` (declined) → cancel the PR's active tasks. Other actions (e.g.
/// `pullrequest:updated`) do nothing — a re-review is requested with an `@<handle>` comment
/// ([`handle_bitbucket_comment`]). Mirrors [`handle_gitlab_merge_request`] / [`handle_pull_request`].
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
        "pullrequest:created" => {
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
                    "Bitbucket PR author appears to be a bot; skipping automatic review"
                );
                crate::http::metrics::review_skipped_bot_author();
                return;
            }
            let base_sha = pr["destination"]["commit"]["hash"]
                .as_str()
                .map(str::to_string);
            let head_sha = pr["source"]["commit"]["hash"].as_str().map(str::to_string);
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
                tier: "fast".to_string(),
                trigger_comment_id: None,
                trace_context: lci_observability::current_traceparent(),
            };
            create_review_task(pool, task, delivery_id).await;
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

    let command_text = command_from_comment(body);
    let trigger_comment_id = payload["comment"]["id"].as_i64();
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
        tier: "deep".to_string(),
        trigger_comment_id,
        trace_context: lci_observability::current_traceparent(),
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

/// `pull_request` events. `opened` → the automatic first review. `closed` → cancel the PR's active
/// tasks (the reaper then stops their Jobs). `synchronize`/`reopened` do nothing — a re-review is
/// requested with an `@<handle>` comment ([`handle_issue_comment`]).
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
        "opened" => {
            let Some(installation_id) = installation_id_opt else {
                return;
            };
            // Approval gate (Epic #75): a repo must be admin-approved before any review runs.
            if !approved_or_skip(pool, repository_id, delivery_id, pr_number).await {
                return;
            }
            let pr = &payload["pull_request"];
            // RFC-0003: skip the automatic fast-tier review for bot-authored PRs (Dependabot, Renovate,
            // another GitHub App, or ourselves) — mechanical diffs burn LLM budget on low-signal
            // comments and risk bot-on-bot feedback loops. The `@mention` deep-review path is
            // untouched: a human can still ask for a full review on the same PR.
            if should_skip_bot_review(state.review.skip_bot_authored_prs(), pr) {
                tracing::info!(
                    delivery_id,
                    pr = pr_number,
                    repository_id,
                    "PR author is a bot; skipping automatic fast-tier review"
                );
                crate::http::metrics::review_skipped_bot_author();
                return;
            }
            let task = crate::db::NewTask {
                repository_id,
                installation_id,
                webhook_delivery_id: delivery_id.to_string(),
                target_type: "pull_request".to_string(),
                target_id: pr_number,
                command_text: "review".to_string(),
                base_sha: pr["base"]["sha"].as_str().map(str::to_string),
                head_sha: pr["head"]["sha"].as_str().map(str::to_string),
                run_epoch: 0, // the automatic first review
                // ADR-0062: the automatic on-open review is the FAST tier (SAST + a lean diff-only LLM
                // pass, no retrieval, turn-capped). The deep, repo-aware review is `@mention`-only.
                tier: "fast".to_string(),
                // ADR-0068: no trigger comment on the automatic review → the lifecycle reactions land on
                // the PR body itself.
                trigger_comment_id: None,
                trace_context: lci_observability::current_traceparent(),
            };
            create_review_task(pool, task, delivery_id).await;
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

    // A PR re-review needs the base/head SHAs to scope the diff (the comment payload omits them); a
    // plain issue has no diff, so the agent answers against the default branch.
    let (base_sha, head_sha) = if is_pr {
        let Some(github) = state.platforms.get(&Platform::GitHub) else {
            tracing::warn!(
                delivery_id,
                "github app not configured; cannot fetch PR SHAs"
            );
            return;
        };
        let repo_ref = RepoRef {
            platform: Platform::GitHub,
            full_name: format!("{owner}/{name}"),
            platform_repo_id: github_repo_id,
            installation_id,
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
        // ADR-0062: an `@mention` always triggers the DEEP tier — full retrieval, multi-turn — whether
        // the target is a PR (deep review) or an issue (conversational answer).
        tier: "deep".to_string(),
        trigger_comment_id,
        trace_context: lci_observability::current_traceparent(),
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

/// Insert a review task. Shared by the auto-open and manual-mention paths. No reaction is enqueued here:
/// ADR-0068 moves 👀 to *work-started* (the dispatcher launching the Job), so receipt no longer reacts.
#[tracing::instrument(name = "task.create", skip_all, fields(pr = task.target_id))]
async fn create_review_task(pool: &sqlx::PgPool, task: crate::db::NewTask, delivery_id: &str) {
    let (pr, run_epoch) = (task.target_id, task.run_epoch);
    match crate::db::create_task(pool, &task).await {
        Ok(Some(task_id)) => {
            crate::http::metrics::task_created();
            tracing::info!(delivery_id, %task_id, pr, run_epoch, "created review task");
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

/// Constant-time HMAC-SHA256 verification of the GitHub webhook signature.
/// An unset secret rejects everything (fail closed) rather than accepting all traffic.
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

    /// `github_webhook_body` verifies against `state.github_webhook_secret` directly (Epic #492 /
    /// #507's path-scoped design never routes GitHub's signature check through `state.platforms` /
    /// the `CodePlatform` trait at all) — so, unlike the old unified `/webhook` route, there is no
    /// separate "GitHub App not configured" fallback path to test here; a deployment with only
    /// `GITHUB_WEBHOOK_SECRET` set (no `GITHUB_APP_ID`/`GITHUB_APP_PRIVATE_KEY`) verifies exactly
    /// the same way as a fully-configured one, by construction.
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
    async fn github_webhook_body_verifies_a_correctly_signed_request_with_secret_only_state() {
        let state = github_secret_only_state("wh-secret");
        let body = br#"{"zen":"hi"}"#;
        let (headers, body) = github_request(b"wh-secret", body, "no-app-delivery-1");

        let response = github_webhook_body(state, headers, body).await;
        assert_eq!(
            response.status(),
            StatusCode::ACCEPTED,
            "a validly-signed webhook must verify against the raw secret regardless of whether a \
             GitHub App is configured"
        );
    }

    #[tokio::test]
    async fn github_webhook_body_rejects_a_tampered_signature() {
        let state = github_secret_only_state("wh-secret");
        let body = br#"{"zen":"hi"}"#;
        let (mut headers, body) = github_request(b"wh-secret", body, "no-app-delivery-2");
        headers.insert("x-hub-signature-256", "sha256=deadbeef".parse().unwrap());

        let response = github_webhook_body(state, headers, body).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn github_webhook_body_rejects_when_secret_is_unconfigured() {
        // No webhook secret at all — verify_signature fails closed (empty secret => reject),
        // surfacing as the same 401 an invalid signature would, not a separate 503 branch (there
        // is none in this path-scoped design — GitHub verification never consults `state.platforms`).
        let state = github_secret_only_state("");
        let body = br#"{"zen":"hi"}"#;
        let (headers, body) = github_request(b"anything", body, "no-app-delivery-3");

        let response = github_webhook_body(state, headers, body).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
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

    // Epic #492 / #507: platform is now known at the path level — no header detection needed.
    // These tests verify the signature-check helpers and registry lookup that replaced it.
    #[test]
    fn github_signature_check_is_the_entry_gate() {
        let mut mac = HmacSha256::new_from_slice(b"secret").unwrap();
        mac.update(b"payload");
        let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        assert!(verify_signature(b"secret", b"payload", &sig));
    }

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

    fn bitbucket_repo_a_id() -> i64 {
        crate::integrations::platform::stable_id_from_key("myteam/repo-a")
    }

    #[test]
    fn bitbucket_project_webhook_accepts_matching_repo_secret() {
        let registry = bitbucket_registry();
        let body = br#"{"repository":{"full_name":"myteam/repo-a"}}"#;

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
            bitbucket_repo_a_id(), // installation_id from the URL path, not the payload
        ));
    }

    #[test]
    fn bitbucket_project_webhook_rejects_wrong_repo_secret() {
        let registry = bitbucket_registry();
        let body = br#"{"repository":{"full_name":"myteam/repo-a"}}"#;

        use hmac::{Hmac, KeyInit, Mac};
        use sha2::Sha256;
        type HmacSha256 = Hmac<Sha256>;
        // Signed with repo-b's secret, but the path identifies repo-a.
        let mut mac = HmacSha256::new_from_slice(b"secret-b").unwrap();
        mac.update(body);
        let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

        let mut headers = HeaderMap::new();
        headers.insert("x-hub-signature", sig.parse().unwrap());

        assert!(!verify_bitbucket_project_webhook_with_registry(
            Some(&registry),
            &headers,
            body,
            bitbucket_repo_a_id(),
        ));
    }

    #[test]
    fn bitbucket_project_webhook_rejects_unconfigured_or_unknown_repo() {
        let headers = HeaderMap::new();

        // No Bitbucket registry at all (platform not configured).
        assert!(!verify_bitbucket_project_webhook_with_registry(
            None,
            &headers,
            b"{}",
            bitbucket_repo_a_id(),
        ));

        // A registry exists, but this installation_id isn't in it.
        let registry = bitbucket_registry();
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
                    api_url: None,
                    access_token: "token-a".to_string(),
                    webhook_secret: "secret-a".to_string(),
                    bot_handle: None,
                },
                crate::config::GitlabProjectConfig {
                    project_id: 1002,
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
    fn gitlab_project_webhook_accepts_matching_project_secret() {
        let registry = gitlab_registry();
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-token", "secret-a".parse().unwrap());

        assert!(verify_gitlab_project_webhook_with_registry(
            Some(&registry),
            &headers,
            b"{}",
            1001, // project_id from path, not body
        ));
    }

    #[test]
    fn gitlab_project_webhook_rejects_wrong_project_secret() {
        let registry = gitlab_registry();
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-token", "secret-b".parse().unwrap());

        assert!(!verify_gitlab_project_webhook_with_registry(
            Some(&registry),
            &headers,
            b"{}",
            1001, // project 1001 has secret-a, not secret-b
        ));
    }

    #[test]
    fn gitlab_project_webhook_rejects_unknown_project() {
        let registry = gitlab_registry();
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-token", "secret-a".parse().unwrap());

        assert!(!verify_gitlab_project_webhook_with_registry(
            Some(&registry),
            &headers,
            b"{}",
            9999, // not in registry
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
}
