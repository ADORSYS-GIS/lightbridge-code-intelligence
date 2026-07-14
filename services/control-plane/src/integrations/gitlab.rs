//! GitLab integration — `CodePlatform` implementation for GitLab.
//!
//! GitLab's API model differs from GitHub's in three ways that shape this file:
//!
//! 1. **Auth is configured per project** (`PRIVATE-TOKEN` header), not per-installation tokens.
//!    `control-plane.json` carries the configured GitLab projects; each project has its own access
//!    token and webhook secret. There is intentionally no `GITLAB_*` env fallback.
//! 2. **No "review" object** — inline comments are "discussion threads" with a `position` object
//!    (base/head/start SHA + path + line), and the review body is a plain MR note. `post_review`
//!    fetches the MR's `diff_refs` first, posts each inline as a discussion, then the body as a note.
//! 3. **Webhook auth is a plain token** (`X-Gitlab-Token` header), not HMAC. The webhook handler
//!    reads the payload's `project.id`, resolves the configured project, then calls
//!    `verify_webhook` on that project client.
//!
//! Notes on note addressing:
//! - GitLab notes are scoped to their parent (MR or issue) — there is NO global
//!   `/projects/{id}/notes/{note_id}` endpoint. Both `add_reaction` and `list_comment_reactions`
//!   require the parent MR/issue `iid`, which is carried from the task's `target_id` (in the
//!   outbox payload for outbound, and in `PollableComment` for inbound polling).
//! - `post_comment` tries MR notes first, then issue notes (the caller passes a single `issue_number`
//!   which is the MR `iid` for PRs or the issue `iid` for issues — we don't know which, so we probe).

#![allow(dead_code)]

use async_trait::async_trait;
use reqwest::Client;

use crate::config::GitlabSection;
use crate::integrations::platform::*;

/// GitLab API client for one configured project token + one base URL — no token minting.
#[derive(Clone)]
pub struct GitlabClient {
    /// Base API URL, e.g. `https://gitlab.com/api/v4` (no trailing slash).
    api_url: String,
    /// Access token sent as the `PRIVATE-TOKEN` header.
    token: String,
    /// Webhook secret used to verify GitLab webhook tokens (cached at init).
    webhook_secret: String,
    /// HTTP client (shared, connection-pooled).
    http: Client,
}

impl GitlabClient {
    /// Construct from validated file configuration. Fails loud if credentials cannot be used as
    /// HTTP headers; otherwise API calls would silently go out unauthenticated.
    pub fn new(api_url: String, token: String, webhook_secret: String) -> anyhow::Result<Self> {
        if reqwest::header::HeaderValue::from_str(&token).is_err() {
            anyhow::bail!("GitLab access token contains invalid header bytes");
        }
        if reqwest::header::HeaderValue::from_str(&webhook_secret).is_err() {
            anyhow::bail!("GitLab webhook secret contains invalid header bytes");
        }
        let http = Client::builder()
            .user_agent("lightbridge-code-intelligence")
            .build()?;
        Ok(Self {
            api_url: api_url.trim_end_matches('/').to_string(),
            token,
            webhook_secret,
            http,
        })
    }

    /// URL-encode the project path: `group/subgroup/repo` → `group%2Fsubgroup%2Frepo`.
    fn project_encoded(repo: &RepoRef) -> String {
        repo.full_name.replace('/', "%2F")
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.api_url, path)
    }

    /// Build the base API path for a noteable's parent (MR or issue).
    /// `noteable_type` is the task's `target_type` (`"pull_request"` → MR, anything else → issue).
    /// Defaults to MR when `noteable_type` is `None` (legacy rows).
    fn noteable_base(project: &str, iid: i64, noteable_type: Option<&str>) -> String {
        let is_mr = noteable_type.map(|t| t == "pull_request").unwrap_or(true);
        if is_mr {
            format!("/projects/{}/merge_requests/{}", project, iid)
        } else {
            format!("/projects/{}/issues/{}", project, iid)
        }
    }

    /// Build the award-emoji endpoint for a note on an MR or issue.
    fn note_award_emoji_path(
        project: &str,
        iid: i64,
        note_id: i64,
        noteable_type: Option<&str>,
    ) -> String {
        format!(
            "{}/notes/{}/award_emoji",
            Self::noteable_base(project, iid, noteable_type),
            note_id
        )
    }

    /// Normalize GitLab award-emoji names to the GitHub vocabulary used by the
    /// feedback-memory consumer (`rejected_findings_for_repo` filters `reaction = '-1'`).
    /// GitLab returns `"thumbsup"`/`"thumbsdown"`; GitHub returns `"+1"`/`"-1"`.
    fn normalize_emoji_name(name: &str) -> String {
        match name {
            "thumbsup" => "+1".to_string(),
            "thumbsdown" => "-1".to_string(),
            other => other.to_string(),
        }
    }

    /// Standard headers for every GitLab API call.
    fn api_headers(&self) -> reqwest::header::HeaderMap {
        let mut h = reqwest::header::HeaderMap::new();
        if let Ok(v) = reqwest::header::HeaderValue::from_str(&self.token) {
            h.insert("PRIVATE-TOKEN", v);
        }
        h
    }

    // --- Internal API helpers ---

    /// Fetch an MR's `diff_refs` (base_sha, head_sha, start_sha) — needed for inline comment
    /// positions. Also returns the MR's web_url for the review permalink.
    async fn fetch_mr_diff_refs(&self, project: &str, mr_iid: i64) -> anyhow::Result<DiffRefs> {
        let url = self.url(&format!("/projects/{}/merge_requests/{}", project, mr_iid));
        let resp = self
            .http
            .get(&url)
            .headers(self.api_headers())
            .send()
            .await?
            .error_for_status()?;
        let v: serde_json::Value = resp.json().await?;
        Ok(DiffRefs {
            base_sha: v
                .get("diff_refs")
                .and_then(|d| d.get("base_sha"))
                .and_then(|s| s.as_str())
                .map(|s| s.to_string()),
            head_sha: v
                .get("diff_refs")
                .and_then(|d| d.get("head_sha"))
                .and_then(|s| s.as_str())
                .map(|s| s.to_string()),
            start_sha: v
                .get("diff_refs")
                .and_then(|d| d.get("start_sha"))
                .and_then(|s| s.as_str())
                .map(|s| s.to_string()),
            web_url: v
                .get("web_url")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string()),
        })
    }
}

struct DiffRefs {
    base_sha: Option<String>,
    head_sha: Option<String>,
    start_sha: Option<String>,
    web_url: Option<String>,
}

/// One configured GitLab project and its private API/webhook client.
#[derive(Clone)]
pub struct GitlabProject {
    pub project_id: i64,
    pub path_with_namespace: String,
    pub bot_handle: String,
    pub client: GitlabClient,
}

/// File-configured GitLab project registry keyed by GitLab `project.id`.
#[derive(Clone, Default)]
pub struct GitlabRegistry {
    by_project_id: std::collections::HashMap<i64, GitlabProject>,
}

impl GitlabRegistry {
    pub fn from_config(section: &GitlabSection) -> anyhow::Result<Option<Self>> {
        section.validate()?;
        if !section.enabled {
            return Ok(None);
        }

        let mut by_project_id = std::collections::HashMap::new();
        for project in &section.projects {
            let api_url = section.resolved_api_url(project).to_string();
            let bot_handle = section.resolved_bot_handle(project).to_string();
            let client = GitlabClient::new(
                api_url,
                project.access_token.clone(),
                project.webhook_secret.clone(),
            )?;
            by_project_id.insert(
                project.project_id,
                GitlabProject {
                    project_id: project.project_id,
                    path_with_namespace: project.path_with_namespace.clone(),
                    bot_handle,
                    client,
                },
            );
        }

        tracing::info!(
            projects = by_project_id.len(),
            "configured GitLab projects from control-plane file config"
        );
        Ok(Some(Self { by_project_id }))
    }

    pub fn get(&self, project_id: i64) -> Option<&GitlabProject> {
        self.by_project_id.get(&project_id)
    }

    pub fn client_for_project(&self, project_id: i64) -> Option<&GitlabClient> {
        self.get(project_id).map(|project| &project.client)
    }

    pub fn client_for_repo(&self, repo: &RepoRef) -> Option<&GitlabClient> {
        self.client_for_project(repo.installation_id)
    }

    pub fn bot_handle(&self, project_id: i64) -> Option<&str> {
        self.get(project_id)
            .map(|project| project.bot_handle.as_str())
    }

    pub fn is_configured(&self) -> bool {
        !self.by_project_id.is_empty()
    }
}

/// `CodePlatform` adapter that preserves platform-level dispatch while selecting the concrete
/// GitLab client by `RepoRef.installation_id` (`project.id` for GitLab).
#[derive(Clone)]
pub struct GitlabPlatformRouter {
    registry: GitlabRegistry,
}

impl GitlabPlatformRouter {
    pub fn new(registry: GitlabRegistry) -> Self {
        Self { registry }
    }

    fn client<'a>(&'a self, repo: &RepoRef) -> anyhow::Result<&'a GitlabClient> {
        self.registry.client_for_repo(repo).ok_or_else(|| {
            anyhow::anyhow!("GitLab project {} is not configured", repo.installation_id)
        })
    }
}

#[async_trait]
impl CodePlatform for GitlabPlatformRouter {
    fn name(&self) -> &'static str {
        "gitlab"
    }

    fn verify_webhook(&self, _headers: &axum::http::HeaderMap, _body: &[u8]) -> bool {
        // Project-specific GitLab webhook verification needs the JSON payload's `project.id` to
        // choose the right secret, so `http::webhook` calls the resolved project client directly.
        false
    }

    fn delivery_id(&self, headers: &axum::http::HeaderMap) -> Option<String> {
        headers
            .get("x-gitlab-event-uuid")?
            .to_str()
            .ok()
            .map(|s| s.to_string())
    }

    fn event_type(&self, headers: &axum::http::HeaderMap) -> Option<String> {
        headers
            .get("x-gitlab-event")?
            .to_str()
            .ok()
            .map(|s| s.to_string())
    }

    async fn list_changed_files(
        &self,
        repo: &RepoRef,
        pr_number: i64,
    ) -> anyhow::Result<Vec<ChangedFile>> {
        self.client(repo)?.list_changed_files(repo, pr_number).await
    }

    async fn default_branch(&self, repo: &RepoRef) -> anyhow::Result<String> {
        self.client(repo)?.default_branch(repo).await
    }

    async fn pr_shas(
        &self,
        repo: &RepoRef,
        pr_number: i64,
    ) -> anyhow::Result<(Option<String>, Option<String>)> {
        self.client(repo)?.pr_shas(repo, pr_number).await
    }

    async fn post_review(
        &self,
        repo: &RepoRef,
        review: &ReviewPost,
    ) -> anyhow::Result<PostedReview> {
        self.client(repo)?.post_review(repo, review).await
    }

    async fn post_comment(
        &self,
        repo: &RepoRef,
        issue_number: i64,
        body: &str,
        noteable_type: Option<&str>,
    ) -> anyhow::Result<PostedComment> {
        self.client(repo)?
            .post_comment(repo, issue_number, body, noteable_type)
            .await
    }

    async fn add_reaction(
        &self,
        repo: &RepoRef,
        target: ReactionTarget,
        emoji: &str,
        noteable_type: Option<&str>,
    ) -> anyhow::Result<()> {
        self.client(repo)?
            .add_reaction(repo, target, emoji, noteable_type)
            .await
    }

    async fn add_labels(
        &self,
        repo: &RepoRef,
        issue_number: i64,
        labels: &[String],
    ) -> anyhow::Result<()> {
        self.client(repo)?
            .add_labels(repo, issue_number, labels)
            .await
    }

    async fn list_review_comments(
        &self,
        repo: &RepoRef,
        pr_number: i64,
        review_id: i64,
    ) -> anyhow::Result<Vec<ReviewCommentRef>> {
        self.client(repo)?
            .list_review_comments(repo, pr_number, review_id)
            .await
    }

    async fn list_comment_reactions(
        &self,
        repo: &RepoRef,
        comment_id: i64,
        is_review_comment: bool,
        iid: Option<i64>,
        noteable_type: Option<&str>,
    ) -> anyhow::Result<Vec<Reaction>> {
        self.client(repo)?
            .list_comment_reactions(repo, comment_id, is_review_comment, iid, noteable_type)
            .await
    }

    fn clone_url(&self, repo: &RepoRef) -> String {
        self.registry
            .client_for_repo(repo)
            .map(|client| client.clone_url(repo))
            .unwrap_or_default()
    }
}

#[async_trait]
impl CodePlatform for GitlabClient {
    fn name(&self) -> &'static str {
        "gitlab"
    }

    fn verify_webhook(&self, headers: &axum::http::HeaderMap, _body: &[u8]) -> bool {
        // GitLab sends the raw webhook secret in the `X-Gitlab-Token` header — no HMAC.
        if self.webhook_secret.is_empty() {
            return false; // fail-closed
        }
        let token = match headers.get("x-gitlab-token").and_then(|v| v.to_str().ok()) {
            Some(s) => s,
            None => return false,
        };
        // Constant-time comparison.
        use subtle::ConstantTimeEq;
        self.webhook_secret
            .as_bytes()
            .ct_eq(token.as_bytes())
            .into()
    }

    fn delivery_id(&self, headers: &axum::http::HeaderMap) -> Option<String> {
        headers
            .get("x-gitlab-event-uuid")?
            .to_str()
            .ok()
            .map(|s| s.to_string())
    }

    fn event_type(&self, headers: &axum::http::HeaderMap) -> Option<String> {
        headers
            .get("x-gitlab-event")?
            .to_str()
            .ok()
            .map(|s| s.to_string())
    }

    async fn list_changed_files(
        &self,
        repo: &RepoRef,
        pr_number: i64,
    ) -> anyhow::Result<Vec<ChangedFile>> {
        let project = Self::project_encoded(repo);
        let url = self.url(&format!(
            "/projects/{}/merge_requests/{}/changes",
            project, pr_number
        ));
        let resp = self
            .http
            .get(&url)
            .headers(self.api_headers())
            .send()
            .await?
            .error_for_status()?;
        let v: serde_json::Value = resp.json().await?;
        let changes = v
            .get("changes")
            .and_then(|c| c.as_array())
            .ok_or_else(|| anyhow::anyhow!("GitLab MR changes: missing 'changes' array"))?;
        Ok(changes
            .iter()
            .map(|c| ChangedFile {
                path: c
                    .get("new_path")
                    .and_then(|p| p.as_str())
                    .unwrap_or("")
                    .to_string(),
                patch: c
                    .get("diff")
                    .and_then(|d| d.as_str())
                    .map(|s| s.to_string()),
            })
            .collect())
    }

    async fn default_branch(&self, repo: &RepoRef) -> anyhow::Result<String> {
        let project = Self::project_encoded(repo);
        let url = self.url(&format!("/projects/{}", project));
        let resp = self
            .http
            .get(&url)
            .headers(self.api_headers())
            .send()
            .await?
            .error_for_status()?;
        let v: serde_json::Value = resp.json().await?;
        v.get("default_branch")
            .and_then(|b| b.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("GitLab project: missing 'default_branch'"))
    }

    async fn pr_shas(
        &self,
        repo: &RepoRef,
        pr_number: i64,
    ) -> anyhow::Result<(Option<String>, Option<String>)> {
        let project = Self::project_encoded(repo);
        let refs = self.fetch_mr_diff_refs(&project, pr_number).await?;
        Ok((refs.base_sha, refs.head_sha))
    }

    async fn post_review(
        &self,
        repo: &RepoRef,
        review: &ReviewPost,
    ) -> anyhow::Result<PostedReview> {
        let project = Self::project_encoded(repo);
        let mr_iid = review.pr_number;

        // Fetch diff_refs for the position object on inline comments.
        let refs = self.fetch_mr_diff_refs(&project, mr_iid).await?;

        // Post each inline comment as a discussion thread with a position object.
        for c in &review.comments {
            let position = serde_json::json!({
                "base_sha": refs.base_sha,
                "head_sha": refs.head_sha,
                "start_sha": refs.start_sha,
                "position_type": "text",
                "new_path": c.path,
                "old_path": c.path,
                "new_line": c.line,
            });
            let body = serde_json::json!({
                "body": c.body,
                "position": position,
            });
            let url = self.url(&format!(
                "/projects/{}/merge_requests/{}/discussions",
                project, mr_iid
            ));
            let _ = self
                .http
                .post(&url)
                .headers(self.api_headers())
                .json(&body)
                .send()
                .await?
                .error_for_status()?;
        }

        // Post the review body as a plain MR note (GitLab has no "review" aggregate).
        let note_url = self.url(&format!(
            "/projects/{}/merge_requests/{}/notes",
            project, mr_iid
        ));
        let note_body = serde_json::json!({ "body": review.body });
        let resp = self
            .http
            .post(&note_url)
            .headers(self.api_headers())
            .json(&note_body)
            .send()
            .await?
            .error_for_status()?;
        let v: serde_json::Value = resp.json().await?;
        let note_id = v.get("id").and_then(|i| i.as_i64());
        Ok(PostedReview {
            id: note_id,
            html_url: refs.web_url,
        })
    }

    async fn post_comment(
        &self,
        repo: &RepoRef,
        issue_number: i64,
        body: &str,
        noteable_type: Option<&str>,
    ) -> anyhow::Result<PostedComment> {
        let project = Self::project_encoded(repo);
        let payload = serde_json::json!({
            "body": body
        });

        let endpoint = format!(
            "{}/notes",
            Self::noteable_base(&project, issue_number, noteable_type)
        );
        let url = self.url(&endpoint);
        let resp = self
            .http
            .post(&url)
            .headers(self.api_headers())
            .json(&payload)
            .send()
            .await?
            .error_for_status()?;
        let v: serde_json::Value = resp.json().await?;
        Ok(PostedComment {
            id: v.get("id").and_then(|i| i.as_i64()),
            html_url: None,
        })
    }

    async fn add_reaction(
        &self,
        repo: &RepoRef,
        target: ReactionTarget,
        emoji: &str,
        noteable_type: Option<&str>,
    ) -> anyhow::Result<()> {
        let project = Self::project_encoded(repo);
        match target {
            ReactionTarget::Issue { number } => {
                let endpoint = format!(
                    "{}/award_emoji",
                    Self::noteable_base(&project, number, noteable_type)
                );
                let url = self.url(&endpoint);
                let _ = self
                    .http
                    .post(&url)
                    .headers(self.api_headers())
                    .json(&serde_json::json!({ "name": emoji }))
                    .send()
                    .await?
                    .error_for_status()?;
                Ok(())
            }
            ReactionTarget::Comment { comment_id, iid } => {
                // GitLab notes are scoped to their parent (MR or issue) — there is NO global
                // `/projects/{id}/notes/{note_id}` endpoint. The parent iid is carried in the
                // outbox payload (the task's `target_id`) so we can address the note directly.
                let iid = iid.ok_or_else(|| {
                    anyhow::anyhow!(
                        "GitLab comment reaction requires the parent MR/issue iid \
                         (missing from outbox payload — legacy row?)"
                    )
                })?;
                let endpoint =
                    Self::note_award_emoji_path(&project, iid, comment_id, noteable_type);
                let url = self.url(&endpoint);
                let _ = self
                    .http
                    .post(&url)
                    .headers(self.api_headers())
                    .json(&serde_json::json!({ "name": emoji }))
                    .send()
                    .await?
                    .error_for_status()?;
                Ok(())
            }
        }
    }

    async fn add_labels(
        &self,
        repo: &RepoRef,
        issue_number: i64,
        labels: &[String],
    ) -> anyhow::Result<()> {
        let project = Self::project_encoded(repo);
        // PUT on the MR with a `labels` param. (GitLab uses comma-joined labels.)
        let labels_csv = labels.join(",");
        let url = self.url(&format!(
            "/projects/{}/merge_requests/{}",
            project, issue_number
        ));
        let _ = self
            .http
            .put(&url)
            .headers(self.api_headers())
            .query(&[("labels", &labels_csv)])
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn list_review_comments(
        &self,
        repo: &RepoRef,
        pr_number: i64,
        _review_id: i64,
    ) -> anyhow::Result<Vec<ReviewCommentRef>> {
        let project = Self::project_encoded(repo);
        let url = self.url(&format!(
            "/projects/{}/merge_requests/{}/discussions?per_page=100",
            project, pr_number
        ));
        let resp = self
            .http
            .get(&url)
            .headers(self.api_headers())
            .send()
            .await?
            .error_for_status()?;
        let v: serde_json::Value = resp.json().await?;
        let discussions = v
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("GitLab discussions: expected array"))?;
        let mut out = Vec::new();
        for d in discussions {
            let notes = match d.get("notes").and_then(|n| n.as_array()) {
                Some(n) => n,
                None => continue,
            };
            for n in notes {
                // Only DiffNote-type notes are inline review comments.
                let is_diff = n
                    .get("type")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t == "DiffNote");
                if !is_diff {
                    continue;
                }
                let id = n.get("id").and_then(|i| i.as_i64()).unwrap_or(0);
                let position = n.get("position");
                let path = position
                    .and_then(|p| p.get("new_path"))
                    .and_then(|p| p.as_str())
                    .map(|s| s.to_string());
                let line = position
                    .and_then(|p| p.get("new_line"))
                    .and_then(|p| p.as_i64());
                out.push(ReviewCommentRef { id, path, line });
            }
        }
        Ok(out)
    }

    async fn list_comment_reactions(
        &self,
        repo: &RepoRef,
        comment_id: i64,
        _is_review_comment: bool,
        iid: Option<i64>,
        noteable_type: Option<&str>,
    ) -> anyhow::Result<Vec<Reaction>> {
        let project = Self::project_encoded(repo);

        // GitLab notes are scoped to their parent (MR or issue) — there is NO global
        // `/projects/{id}/notes/{note_id}` endpoint. The parent iid is carried from the task's
        // `target_id` (via `PollableComment`) so we can address the note's award emoji directly.
        let iid = iid.ok_or_else(|| {
            anyhow::anyhow!(
                "GitLab list_comment_reactions requires the parent MR/issue iid \
                 (missing from PollableComment — legacy row?)"
            )
        })?;
        let endpoint = format!(
            "{}?per_page=100",
            Self::note_award_emoji_path(&project, iid, comment_id, noteable_type)
        );
        let url = self.url(&endpoint);
        let resp = self
            .http
            .get(&url)
            .headers(self.api_headers())
            .send()
            .await?;

        // A 404 means the note was deleted (or never existed). Treat it as "no reactions"
        // so that `reconcile_comment_feedback` still runs and prunes stale feedback rows
        // (e.g. a 👎 the user has since removed). Without this, a deleted note would
        // error every poll cycle and the stale rejection would suppress the finding forever.
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(vec![]);
        }
        let resp = resp.error_for_status()?;
        let award_v: serde_json::Value = resp.json().await?;

        // Parse award emoji — normalize GitLab names to the GitHub vocabulary so
        // downstream filters (`reaction = '-1'`) fire correctly (ADR-0044).
        let reactions: Vec<Reaction> = award_v
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let name = item.get("name")?.as_str()?;
                let user = item.get("user")?.get("username")?.as_str()?;
                Some(Reaction {
                    content: Self::normalize_emoji_name(name),
                    user_login: user.to_string(),
                })
            })
            .collect();

        Ok(reactions)
    }

    fn clone_url(&self, repo: &RepoRef) -> String {
        // Embed the token for HTTPS clone (oauth2:TOKEN@host form).
        // Strip the `/api/v4` suffix to get the base host URL, then strip the
        // protocol prefix to avoid a doubled scheme in the final URL.
        // Note: Token and path are not URL-encoded. GitLab PATs are alphanumeric + hyphens,
        // and project paths are typically alphanumeric + hyphens + underscores, so this is
        // low-risk in practice. URL-encoding would break the OAuth2 format.
        let base = self
            .api_url
            .strip_suffix("/api/v4")
            .unwrap_or(&self.api_url);
        let host = base.split_once("://").map(|(_, h)| h).unwrap_or(base);
        format!(
            "https://oauth2:{}@{}/{}.git",
            self.token, host, repo.full_name
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GitlabProjectConfig, GitlabSection};

    fn project(project_id: i64, path: &str, token: &str, secret: &str) -> GitlabProjectConfig {
        GitlabProjectConfig {
            project_id,
            path_with_namespace: path.to_string(),
            api_url: None,
            access_token: token.to_string(),
            webhook_secret: secret.to_string(),
            bot_handle: None,
        }
    }

    #[test]
    fn disabled_config_builds_no_registry() {
        let section = GitlabSection::default();
        let registry = GitlabRegistry::from_config(&section).expect("disabled config is valid");
        assert!(registry.is_none());
    }

    #[test]
    fn registry_resolves_clients_and_handles_by_project_id() {
        let section = GitlabSection {
            enabled: true,
            default_api_url: Some("https://gitlab.example.com/api/v4".to_string()),
            default_bot_handle: Some("lightbridge-bot".to_string()),
            projects: vec![
                project(1001, "group/service-a", "token-a", "secret-a"),
                GitlabProjectConfig {
                    bot_handle: Some("lb-reviewer".to_string()),
                    ..project(1002, "group/service-b", "token-b", "secret-b")
                },
            ],
        };
        let registry = GitlabRegistry::from_config(&section)
            .expect("valid config builds")
            .expect("enabled config produces registry");

        assert!(registry.is_configured());
        assert_eq!(registry.bot_handle(1001), Some("lightbridge-bot"));
        assert_eq!(registry.bot_handle(1002), Some("lb-reviewer"));
        assert!(registry.client_for_project(1001).is_some());
        assert!(registry.client_for_project(9999).is_none());

        let repo = RepoRef {
            platform: Platform::GitLab,
            full_name: "group/service-b".to_string(),
            platform_repo_id: 1002,
            installation_id: 1002,
        };
        let clone_url = registry
            .client_for_repo(&repo)
            .expect("repo resolves through installation_id")
            .clone_url(&repo);
        assert!(clone_url.contains("oauth2:token-b@"));
        assert!(clone_url.ends_with("/group/service-b.git"));
    }
}
