//! Unified webhook receiver (GitHub + GitLab).
//!
//! A single `/webhook` route detects the platform from headers, verifies the signature, dedupes
//! on the platform's delivery ID, then hands off to platform-specific event routing. With a
//! database, dedup + persistence happen atomically via the `webhook_deliveries` PRIMARY KEY;
//! without one (dev) it falls back to an in-memory set.
//!
//! The legacy `/github/webhook` route is kept as an alias during the transition and forwards to
//! the same unified handler.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use crate::AppState;
use crate::integrations::platform::{CodePlatform, Platform, RepoRef};

type HmacSha256 = Hmac<Sha256>;

/// Detect the platform from webhook headers.
/// GitHub sends `X-GitHub-Event`; GitLab sends `X-Gitlab-Event`.
fn detect_platform(headers: &HeaderMap) -> Option<Platform> {
    if headers.contains_key("x-github-event") {
        Some(Platform::GitHub)
    } else if headers.contains_key("x-gitlab-event") {
        Some(Platform::GitLab)
    } else {
        None
    }
}

/// Unified webhook receiver. Detects the platform from headers and dispatches.
pub async fn webhook_router(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let platform = match detect_platform(&headers) {
        Some(p) => p,
        None => {
            tracing::warn!(
                "webhook: unknown platform (no X-GitHub-Event or X-Gitlab-Event header)"
            );
            return (StatusCode::BAD_REQUEST, "unknown platform").into_response();
        }
    };

    tracing::info!(%platform, "webhook received");

    // GitLab needs the payload's `project.id` to select the configured per-project webhook secret.
    // The payload is not trusted or acted on until verification succeeds.
    let gitlab_payload = if platform == Platform::GitLab {
        match serde_json::from_slice(&body) {
            Ok(payload) => Some(payload),
            Err(error) => {
                tracing::error!(%error, %platform, "webhook payload is not valid JSON");
                return (StatusCode::BAD_REQUEST, "invalid json payload").into_response();
            }
        }
    } else {
        None
    };

    // Verify signature — platform-specific.
    let valid = match platform {
        Platform::GitHub => verify_signature(
            state.github_webhook_secret.as_bytes(),
            &body,
            &header(&headers, "x-hub-signature-256"),
        ),
        Platform::GitLab => verify_gitlab_project_webhook(
            &state,
            &headers,
            &body,
            gitlab_payload
                .as_ref()
                .expect("GitLab payload parsed before verification"),
        ),
    };
    if !valid {
        crate::http::metrics::webhook_signature_failure(&platform.to_string());
        tracing::warn!(%platform, "invalid webhook signature");
        return (StatusCode::UNAUTHORIZED, "invalid signature").into_response();
    }

    // Delivery ID for dedup — platform-specific header.
    let delivery_id = match platform {
        Platform::GitHub => header(&headers, "x-github-delivery"),
        Platform::GitLab => header(&headers, "x-gitlab-event-uuid"),
    };
    if delivery_id.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing delivery id").into_response();
    }

    // Event type — platform-specific header.
    let event = match platform {
        Platform::GitHub => header(&headers, "x-github-event"),
        Platform::GitLab => header(&headers, "x-gitlab-event"),
    };

    // Parse the payload up front: reject non-JSON bodies (never persist `null`).
    let payload: serde_json::Value = match gitlab_payload {
        Some(payload) => payload,
        None => match serde_json::from_slice(&body) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::error!(%error, %platform, %delivery_id, "webhook payload is not valid JSON");
                return (StatusCode::BAD_REQUEST, "invalid json payload").into_response();
            }
        },
    };

    // Dedup (and persist, when a database is configured). `is_new` is false for a replayed
    // delivery id — platforms retry, so this is the exactly-once guard.
    let is_new = match &state.db {
        Some(pool) => {
            match crate::db::record_delivery(pool, platform, &delivery_id, &event, &payload).await {
                Ok(is_new) => is_new,
                Err(error) => {
                    tracing::error!(%error, %delivery_id, "failed to persist delivery");
                    return (StatusCode::INTERNAL_SERVER_ERROR, "persistence error")
                        .into_response();
                }
            }
        }
        None => state
            .seen_deliveries
            .lock()
            .expect("dedup lock poisoned")
            .insert(delivery_id.clone()),
    };
    if !is_new {
        crate::http::metrics::webhook_duplicate(&platform.to_string());
        tracing::info!(%platform, %delivery_id, "duplicate delivery");
        return (StatusCode::ACCEPTED, "duplicate delivery").into_response();
    }

    crate::http::metrics::webhook_delivery(&platform.to_string(), &event);
    tracing::info!(%platform, %delivery_id, %event, "accepted webhook");

    // Route by platform.
    if state.db.is_some() {
        match platform {
            Platform::GitHub => route_github_event(&state, &event, &payload, &delivery_id).await,
            Platform::GitLab => route_gitlab_event(&state, &event, &payload, &delivery_id).await,
        }
    }

    (StatusCode::ACCEPTED, "accepted").into_response()
}

/// Legacy GitHub webhook route — forwards to the unified [`webhook_router`]. Kept during the
/// transition so existing webhook configurations don't break; remove in a later release.
pub async fn github_webhook_legacy(
    state: State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    webhook_router(state, headers, body).await
}

fn verify_gitlab_project_webhook(
    state: &AppState,
    headers: &HeaderMap,
    body: &[u8],
    payload: &serde_json::Value,
) -> bool {
    verify_gitlab_project_webhook_with_registry(state.gitlab.as_ref(), headers, body, payload)
}

fn verify_gitlab_project_webhook_with_registry(
    registry: Option<&crate::integrations::gitlab::GitlabRegistry>,
    headers: &HeaderMap,
    body: &[u8],
    payload: &serde_json::Value,
) -> bool {
    let Some(project_id) = payload["project"]["id"].as_i64() else {
        tracing::warn!("GitLab webhook missing project.id");
        return false;
    };
    let Some(registry) = registry else {
        tracing::warn!(project_id, "GitLab webhook received but GitLab is not configured");
        return false;
    };
    let Some(project) = registry.get(project_id) else {
        tracing::warn!(project_id, "GitLab webhook for unconfigured project");
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
    };
    tracing::info!(
        delivery_id,
        target = target_iid,
        kind = target_type,
        "GitLab @mention requested"
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
        let Some(app) = state.github.as_ref() else {
            tracing::warn!(
                delivery_id,
                "github app not configured; cannot fetch PR SHAs"
            );
            return;
        };
        let token = match app.installation_token(installation_id).await {
            Ok(token) => token,
            Err(error) => {
                tracing::error!(%error, delivery_id, "mint token for re-review failed");
                return;
            }
        };
        match app.pull_request_shas(&token, owner, name, number).await {
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

    #[test]
    fn detect_platform_recognises_github_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "pull_request".parse().unwrap());
        assert_eq!(detect_platform(&headers), Some(Platform::GitHub));
    }

    #[test]
    fn detect_platform_recognises_gitlab_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-event", "Merge Request Hook".parse().unwrap());
        assert_eq!(detect_platform(&headers), Some(Platform::GitLab));
    }

    #[test]
    fn detect_platform_returns_none_for_unknown() {
        let headers = HeaderMap::new();
        assert_eq!(detect_platform(&headers), None);
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
        let payload = serde_json::json!({ "project": { "id": 1001 } });

        assert!(verify_gitlab_project_webhook_with_registry(
            Some(&registry),
            &headers,
            b"{}",
            &payload
        ));
    }

    #[test]
    fn gitlab_project_webhook_rejects_wrong_project_secret() {
        let registry = gitlab_registry();
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-token", "secret-b".parse().unwrap());
        let payload = serde_json::json!({ "project": { "id": 1001 } });

        assert!(!verify_gitlab_project_webhook_with_registry(
            Some(&registry),
            &headers,
            b"{}",
            &payload
        ));
    }

    #[test]
    fn gitlab_project_webhook_rejects_missing_or_unknown_project() {
        let registry = gitlab_registry();
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-token", "secret-a".parse().unwrap());

        assert!(!verify_gitlab_project_webhook_with_registry(
            Some(&registry),
            &headers,
            b"{}",
            &serde_json::json!({ "project": {} })
        ));
        assert!(!verify_gitlab_project_webhook_with_registry(
            Some(&registry),
            &headers,
            b"{}",
            &serde_json::json!({ "project": { "id": 9999 } })
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
}
