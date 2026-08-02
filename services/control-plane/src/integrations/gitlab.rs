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
    /// Construct from already-validated file configuration.
    pub fn new(api_url: String, token: String, webhook_secret: String) -> anyhow::Result<Self> {
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

    /// Map [`CheckConclusion`] onto GitLab's commit-status `state` vocabulary
    /// (`pending|running|success|failed|canceled`) — narrower than GitHub's: there is no
    /// neutral/timed-out state, so `Neutral` → `success` and `TimedOut` → `failed` (documented lossy
    /// mapping, same capability-gap treatment `add_reaction`/`add_labels` use elsewhere in this file).
    fn commit_status_state(conclusion: CheckConclusion) -> &'static str {
        match conclusion {
            CheckConclusion::Success | CheckConclusion::Neutral => "success",
            CheckConclusion::Failure | CheckConclusion::TimedOut => "failed",
            CheckConclusion::Cancelled => "canceled",
        }
    }

    /// The commit-status body, with `target_url`/`description` inserted **only when present**.
    ///
    /// Mirrors the GitHub fix for the same bug class: `serde_json::json!` serializes an
    /// `Option::None` as an explicit `null`, and GitHub was proven (live, 2026-08-02) to reject a
    /// null optional URL with a 422 rather than treating it as absent. GitLab's own tolerance here
    /// was never verified against a live instance, so this takes the same omit-don't-null shape
    /// rather than betting that GitLab is more lenient than GitHub. Pure, so it is unit-tested.
    fn commit_status_payload(
        state: &str,
        description: Option<&str>,
        target_url: Option<&str>,
    ) -> serde_json::Value {
        let mut payload = serde_json::json!({
            "state": state,
            "name": CHECK_RUN_NAME,
        });
        if let Some(description) = description {
            payload["description"] = serde_json::Value::String(description.to_string());
        }
        if let Some(target_url) = target_url {
            payload["target_url"] = serde_json::Value::String(target_url.to_string());
        }
        payload
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
    /// The value used as the `{installation_id}` path segment in the webhook URL.
    pub installation_id: i64,
    pub bot_handle: String,
    pub web_base_url: String,
    pub client: GitlabClient,
}

/// File-configured GitLab project registry keyed by GitLab `project.id`.
#[derive(Clone, Default)]
pub struct GitlabRegistry {
    by_project_id: std::collections::HashMap<i64, GitlabProject>,
    /// Secondary index keyed by `installation_id` for fast webhook path-segment lookup.
    by_installation_id: std::collections::HashMap<i64, i64>,
    /// Default web (non-API) base URL for projects that omit `api_url`.
    default_web_base_url: String,
}

impl GitlabRegistry {
    pub fn from_config(section: &GitlabSection) -> anyhow::Result<Option<Self>> {
        section.validate()?;
        if !section.enabled {
            return Ok(None);
        }

        let mut by_project_id = std::collections::HashMap::new();
        let mut by_installation_id = std::collections::HashMap::new();
        for project in &section.projects {
            let api_url = section.resolved_api_url(project).to_string();
            let bot_handle = section.resolved_bot_handle(project).to_string();
            let web_base_url = web_base_url_from_api_url(&api_url);
            let installation_id = project.effective_installation_id();
            let client = GitlabClient::new(
                api_url,
                project.access_token.clone(),
                project.webhook_secret.clone(),
            )?;
            by_installation_id.insert(installation_id, project.project_id);
            by_project_id.insert(
                project.project_id,
                GitlabProject {
                    project_id: project.project_id,
                    installation_id,
                    bot_handle,
                    web_base_url,
                    client,
                },
            );
        }

        tracing::info!(
            projects = by_project_id.len(),
            "configured GitLab projects from control-plane file config"
        );
        Ok(Some(Self {
            by_project_id,
            by_installation_id,
            default_web_base_url: web_base_url_from_api_url(section.default_api_url()),
        }))
    }

    pub fn get(&self, project_id: i64) -> Option<&GitlabProject> {
        self.by_project_id.get(&project_id)
    }

    /// Look up a project by the `installation_id` value in the webhook URL path segment.
    pub fn get_by_installation_id(&self, installation_id: i64) -> Option<&GitlabProject> {
        let project_id = self.by_installation_id.get(&installation_id)?;
        self.by_project_id.get(project_id)
    }

    /// Default web (non-API) base URL for building repo/MR deep links when no project-specific host
    /// applies. The frontend reads this via `GET /config` rather than carrying its own `GITLAB_URL`
    /// env var.
    pub fn web_base_url(&self) -> String {
        self.default_web_base_url.clone()
    }

    pub fn web_base_url_for_project(&self, project_id: i64) -> Option<&str> {
        self.get(project_id)
            .map(|project| project.web_base_url.as_str())
    }

    pub fn project_web_base_urls(&self) -> std::collections::BTreeMap<String, String> {
        self.by_project_id
            .iter()
            .map(|(project_id, project)| (project_id.to_string(), project.web_base_url.clone()))
            .collect()
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

fn web_base_url_from_api_url(api_url: &str) -> String {
    api_url
        .trim_end_matches('/')
        .trim_end_matches("/api/v4")
        .to_string()
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

    async fn get_repo_file(
        &self,
        repo: &RepoRef,
        ref_: &str,
        path: &str,
    ) -> anyhow::Result<Option<String>> {
        self.client(repo)?.get_repo_file(repo, ref_, path).await
    }

    async fn update_repo_file(
        &self,
        repo: &RepoRef,
        path: &str,
        mutate: Box<dyn FnOnce(Option<String>) -> String + Send>,
        message: &str,
    ) -> anyhow::Result<()> {
        self.client(repo)?
            .update_repo_file(repo, path, mutate, message)
            .await
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

    async fn start_check_run(
        &self,
        repo: &RepoRef,
        req: CheckRunStart<'_>,
    ) -> anyhow::Result<Option<i64>> {
        self.client(repo)?.start_check_run(repo, req).await
    }

    async fn resolve_check_run(
        &self,
        repo: &RepoRef,
        req: CheckRunResolve<'_>,
    ) -> anyhow::Result<()> {
        self.client(repo)?.resolve_check_run(repo, req).await
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
        match self.registry.client_for_repo(repo) {
            Some(client) => client.clone_url(repo),
            None => {
                tracing::warn!(
                    project_id = repo.installation_id,
                    "GitLab clone URL requested for unconfigured project"
                );
                String::new()
            }
        }
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
            tracing::warn!("GitLab verify_webhook failed: configured webhook_secret is empty");
            return false; // fail-closed
        }
        let header_value = match headers.get("x-gitlab-token") {
            Some(v) => v,
            None => {
                tracing::warn!("GitLab verify_webhook failed: X-Gitlab-Token header is missing");
                return false;
            }
        };
        let token = match header_value.to_str() {
            Ok(s) => s,
            Err(_) => {
                tracing::warn!(
                    "GitLab verify_webhook failed: X-Gitlab-Token header contains invalid UTF-8"
                );
                return false;
            }
        };
        // Constant-time comparison.
        use subtle::ConstantTimeEq;
        let is_valid: bool = self
            .webhook_secret
            .as_bytes()
            .ct_eq(token.as_bytes())
            .into();

        if !is_valid {
            let expected_trimmed_len = self.webhook_secret.trim().len();
            let token_trimmed_len = token.trim().len();
            tracing::warn!(
                "GitLab verify_webhook failed: token mismatch. Expected len {} (trimmed {}), got len {} (trimmed {})",
                self.webhook_secret.len(),
                expected_trimmed_len,
                token.len(),
                token_trimmed_len
            );
        }

        is_valid
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

    async fn get_repo_file(
        &self,
        repo: &RepoRef,
        ref_: &str,
        path: &str,
    ) -> anyhow::Result<Option<String>> {
        let project = Self::project_encoded(repo);
        // GitLab's file-path segment requires `/` percent-encoded (mirrors `project_encoded` above);
        // `.lightbridge-code-review.jsonc` itself has no slashes, but a nested path would.
        let file_path = path.replace('/', "%2F");
        let url = self.url(&format!(
            "/projects/{project}/repository/files/{file_path}/raw?ref={ref_}"
        ));
        let response = self
            .http
            .get(&url)
            .headers(self.api_headers())
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let text = response.error_for_status()?.text().await?;
        Ok(Some(text))
    }

    async fn update_repo_file(
        &self,
        repo: &RepoRef,
        path: &str,
        mutate: Box<dyn FnOnce(Option<String>) -> String + Send>,
        message: &str,
    ) -> anyhow::Result<()> {
        // GitLab requires the target branch explicitly in the body (unlike GitHub, which defaults to
        // the repo's default branch server-side) — ADR-0109 always commits to the default branch.
        let branch = self.default_branch(repo).await?;
        let project = Self::project_encoded(repo);
        let file_path = path.replace('/', "%2F");
        let url = self.url(&format!("/projects/{project}/repository/files/{file_path}"));

        // Read content + `last_commit_id` as ONE call (the metadata endpoint, not `/raw` — it carries
        // both), call `mutate` on that exact snapshot, then write guarded by that same
        // `last_commit_id` — mirrors GitHub's sha-guarded PUT (see the trait doc for why the read and
        // the mutation must happen together, not as two separate round-trips).
        let existing = self
            .http
            .get(&url)
            .headers(self.api_headers())
            .query(&[("ref", branch.as_str())])
            .send()
            .await?;
        let (current_content, last_commit_id) =
            if existing.status() == reqwest::StatusCode::NOT_FOUND {
                (None, None)
            } else {
                let value: serde_json::Value = existing.error_for_status()?.json().await?;
                let content = value["content"].as_str().and_then(|encoded| {
                    use base64::Engine;
                    let stripped: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
                    base64::engine::general_purpose::STANDARD
                        .decode(stripped)
                        .ok()
                        .map(|decoded| String::from_utf8_lossy(&decoded).into_owned())
                });
                let last_commit_id = value["last_commit_id"].as_str().map(str::to_string);
                (content, last_commit_id)
            };
        let new_content = mutate(current_content);

        let mut body = serde_json::json!({
            "branch": branch,
            "content": new_content,
            "commit_message": message,
        });
        // `last_commit_id` is GitLab's optimistic-concurrency guard for an UPDATE — a stale value
        // surfaces as a 400 from the PUT below. Only meaningful when the file already exists; a
        // create (the POST fallback) has no prior commit to guard against.
        if let Some(last_commit_id) = &last_commit_id {
            body["last_commit_id"] = serde_json::Value::String(last_commit_id.clone());
        }
        // GitLab uses different HTTP methods for create (POST) vs update (PUT) on the same path —
        // unlike GitHub's single PUT. Try PUT (the overwhelmingly common case: the config file already
        // exists once a repo is under review) and fall back to POST only on a 404 (the file has never
        // been created).
        let put_response = self
            .http
            .put(&url)
            .headers(self.api_headers())
            .json(&body)
            .send()
            .await?;
        if put_response.status() == reqwest::StatusCode::NOT_FOUND {
            self.http
                .post(&url)
                .headers(self.api_headers())
                .json(&body)
                .send()
                .await?
                .error_for_status()?;
        } else {
            put_response.error_for_status()?;
        }
        Ok(())
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

    async fn start_check_run(
        &self,
        repo: &RepoRef,
        req: CheckRunStart<'_>,
    ) -> anyhow::Result<Option<i64>> {
        let project = Self::project_encoded(repo);
        let url = self.url(&format!("/projects/{}/statuses/{}", project, req.head_sha));
        let body = Self::commit_status_payload("running", None, req.details_url);
        let _ = self
            .http
            .post(&url)
            .headers(self.api_headers())
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        // Upsert-by-sha: nothing to persist for the resolve call to read back.
        Ok(None)
    }

    async fn resolve_check_run(
        &self,
        repo: &RepoRef,
        req: CheckRunResolve<'_>,
    ) -> anyhow::Result<()> {
        let project = Self::project_encoded(repo);
        let url = self.url(&format!("/projects/{}/statuses/{}", project, req.head_sha));
        let body = Self::commit_status_payload(
            Self::commit_status_state(req.conclusion),
            Some(req.summary),
            req.details_url,
        );
        let _ = self
            .http
            .post(&url)
            .headers(self.api_headers())
            .json(&body)
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

    fn project(project_id: i64, token: &str, secret: &str) -> GitlabProjectConfig {
        GitlabProjectConfig {
            project_id,
            installation_id: None,
            api_url: None,
            access_token: token.to_string(),
            webhook_secret: secret.to_string(),
            bot_handle: None,
        }
    }

    #[test]
    fn commit_status_state_maps_every_conclusion_including_the_lossy_ones() {
        // GitLab has no neutral/timed-out state — pin the documented lossy mapping.
        assert_eq!(
            GitlabClient::commit_status_state(CheckConclusion::Success),
            "success"
        );
        assert_eq!(
            GitlabClient::commit_status_state(CheckConclusion::Neutral),
            "success"
        );
        assert_eq!(
            GitlabClient::commit_status_state(CheckConclusion::Failure),
            "failed"
        );
        assert_eq!(
            GitlabClient::commit_status_state(CheckConclusion::TimedOut),
            "failed"
        );
        assert_eq!(
            GitlabClient::commit_status_state(CheckConclusion::Cancelled),
            "canceled"
        );
    }

    /// Same omit-don't-null contract as GitHub's `details_url` (see
    /// `check_run_payloads_omit_details_url_when_absent` in `github.rs`): an absent optional field
    /// must not be serialized as an explicit `null`.
    #[test]
    fn commit_status_payload_omits_absent_optional_fields() {
        let payload = GitlabClient::commit_status_payload("running", None, None);
        assert_eq!(payload["state"], "running");
        assert_eq!(payload["name"], CHECK_RUN_NAME);
        assert!(
            payload.get("target_url").is_none(),
            "target_url must be absent, not null: {payload}"
        );
        assert!(
            payload.get("description").is_none(),
            "description must be absent, not null: {payload}"
        );
    }

    #[test]
    fn commit_status_payload_includes_present_optional_fields() {
        let payload = GitlabClient::commit_status_payload(
            "failed",
            Some("it broke"),
            Some("https://example.test/run/1"),
        );
        assert_eq!(payload["state"], "failed");
        assert_eq!(payload["description"], "it broke");
        assert_eq!(payload["target_url"], "https://example.test/run/1");
    }

    fn check_repo() -> RepoRef {
        RepoRef {
            platform: crate::integrations::platform::Platform::GitLab,
            full_name: "acme/widgets".to_string(),
            platform_repo_id: 0,
            installation_id: 0,
        }
    }

    /// New integration surface (no prior test exercised the actual GitLab HTTP shape for a status
    /// post) — proves the URL, method, and `state` field a real GitLab instance would see.
    #[tokio::test]
    async fn start_check_run_posts_a_running_status_to_the_head_sha() {
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/api/v4/projects/acme%2Fwidgets/statuses/abc123",
            ))
            .and(wiremock::matchers::body_string_contains(
                "\"state\":\"running\"",
            ))
            // Regression: the absent target_url must not reach the wire as `null`.
            .and(|req: &wiremock::Request| {
                !String::from_utf8_lossy(&req.body).contains("target_url")
            })
            .respond_with(wiremock::ResponseTemplate::new(201))
            .mount(&mock)
            .await;

        let client = GitlabClient::new(
            format!("{}/api/v4", mock.uri()),
            "token".to_string(),
            "secret".to_string(),
        )
        .expect("client");
        client
            .start_check_run(
                &check_repo(),
                CheckRunStart {
                    head_sha: "abc123",
                    details_url: None,
                },
            )
            .await
            .expect("start succeeds");
    }

    #[tokio::test]
    async fn resolve_check_run_posts_the_mapped_state_and_summary() {
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/api/v4/projects/acme%2Fwidgets/statuses/abc123",
            ))
            .and(wiremock::matchers::body_string_contains(
                "\"state\":\"failed\"",
            ))
            .and(wiremock::matchers::body_string_contains(
                "\"description\":\"boom\"",
            ))
            .respond_with(wiremock::ResponseTemplate::new(201))
            .mount(&mock)
            .await;

        let client = GitlabClient::new(
            format!("{}/api/v4", mock.uri()),
            "token".to_string(),
            "secret".to_string(),
        )
        .expect("client");
        client
            .resolve_check_run(
                &check_repo(),
                CheckRunResolve {
                    head_sha: "abc123",
                    external_id: None,
                    conclusion: CheckConclusion::Failure,
                    summary: "boom",
                    details_url: None,
                },
            )
            .await
            .expect("resolve succeeds");
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
                project(1001, "token-a", "secret-a"),
                GitlabProjectConfig {
                    api_url: Some("https://gitlab.internal.example/api/v4".to_string()),
                    bot_handle: Some("lb-reviewer".to_string()),
                    ..project(1002, "token-b", "secret-b")
                },
            ],
        };
        let registry = GitlabRegistry::from_config(&section)
            .expect("valid config builds")
            .expect("enabled config produces registry");

        assert!(registry.is_configured());
        assert_eq!(registry.bot_handle(1001), Some("lightbridge-bot"));
        assert_eq!(registry.bot_handle(1002), Some("lb-reviewer"));
        assert_eq!(
            registry.web_base_url_for_project(1001),
            Some("https://gitlab.example.com")
        );
        assert_eq!(
            registry.web_base_url_for_project(1002),
            Some("https://gitlab.internal.example")
        );
        assert_eq!(
            registry
                .project_web_base_urls()
                .get("1002")
                .map(String::as_str),
            Some("https://gitlab.internal.example")
        );
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
        assert!(clone_url.contains("@gitlab.internal.example/"));
        assert!(clone_url.ends_with("/group/service-b.git"));
    }
}
