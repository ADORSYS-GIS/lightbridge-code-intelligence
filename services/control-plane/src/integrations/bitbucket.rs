//! Bitbucket Cloud integration — `CodePlatform` implementation for Bitbucket.
//!
//! Bitbucket, like GitLab, is naturally multi-tenant (per-workspace/per-repo credentials, no
//! single App-wide secret), so this file mirrors `gitlab.rs`'s file-config + registry + router
//! pattern rather than GitHub's single-tenant App model. Three ways Bitbucket Cloud's API model
//! differs from both GitHub's and GitLab's, which shape this file:
//!
//! 1. **Auth is a per-repo App Password** (HTTP Basic: `username` + `app_password`), configured in
//!    `control-plane.json` like GitLab's per-project token — no `BITBUCKET_*` env fallback.
//! 2. **No numeric project id.** GitLab has `project.id`; Bitbucket has only the
//!    `workspace/repo_slug` pair. `RepoRef.installation_id` stays `i64` (shared with GitHub/GitLab,
//!    not changed for this platform), so a stable `i64` identity is derived from
//!    `workspace/repo_slug` via [`platform::stable_id_from_key`] (SHA-256-based, not
//!    `Hash`-trait-based, since this value is persisted and must survive a Rust toolchain upgrade
//!    unchanged).
//! 3. **No "review" object and no reaction/label APIs.** Like GitLab, there is no aggregate review —
//!    `post_review` posts each inline comment individually, then the body as a general PR comment.
//!    Unlike GitHub/GitLab, Bitbucket Cloud's REST API v2.0 has no comment-reaction (award-emoji)
//!    endpoint and no native PR-label feature, so `add_reaction` and `add_labels` are documented
//!    no-ops and `list_comment_reactions` always returns an empty list — the 👍/👎 feedback loop
//!    (ADR-0035) simply never has anything to find on Bitbucket. This is a real capability gap, not
//!    an oversight; ADR-0108 explicitly allows a documented simplification here.
//!
//! Webhook verification: Bitbucket Cloud's webhook request-signing feature signs the raw request
//! body with the configured secret and sends `X-Hub-Signature: sha256=<hex>` — the same
//! HMAC-SHA256 + `sha256=` format GitHub uses (mirrored exactly, including the constant-time
//! compare). This is new integration surface with no prior reference in this codebase to lift
//! byte-for-byte; confirm the header name/format against a real configured Bitbucket webhook before
//! relying on it in production.
//!
//! `list_changed_files` uses Bitbucket's `/pullrequests/{id}/diff` endpoint, which returns one raw
//! unified diff for the whole PR (there is no per-file JSON diff endpoint the way GitLab's
//! `/merge_requests/{iid}/changes` gives one) — the diff text is split into per-file chunks on
//! `"diff --git a/... b/..."` boundaries. This is a reasonable simplification: it reconstructs the
//! same `path`/`patch` shape `ChangedFile` requires, from the one endpoint Bitbucket actually offers.

use async_trait::async_trait;
use hmac::KeyInit;
use reqwest::Client;

use crate::config::BitbucketSection;
use crate::integrations::platform::*;

/// Bitbucket Cloud API client for one configured repo's credentials — no token minting (a static
/// App Password, like GitLab's static access token).
#[derive(Clone)]
pub struct BitbucketClient {
    /// Base API URL, e.g. `https://api.bitbucket.org/2.0` (no trailing slash).
    api_url: String,
    workspace: String,
    repo_slug: String,
    /// HTTP Basic-auth username.
    username: String,
    /// HTTP Basic-auth password (the Bitbucket App Password). Also used for the HTTPS clone URL.
    app_password: String,
    /// Per-repo webhook signing secret used to verify `X-Hub-Signature`.
    webhook_secret: String,
    http: Client,
}

impl BitbucketClient {
    /// Construct from already-validated file configuration.
    pub fn new(
        api_url: String,
        workspace: String,
        repo_slug: String,
        username: String,
        app_password: String,
        webhook_secret: String,
    ) -> anyhow::Result<Self> {
        let http = Client::builder()
            .user_agent("lightbridge-code-intelligence")
            .build()?;
        Ok(Self {
            api_url: api_url.trim_end_matches('/').to_string(),
            workspace,
            repo_slug,
            username,
            app_password,
            webhook_secret,
            http,
        })
    }

    /// `"workspace/repo_slug"` — Bitbucket's equivalent of GitHub's `owner/repo` full name.
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.workspace, self.repo_slug)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.api_url, path)
    }

    /// Base path for this repo's API calls: `/repositories/{workspace}/{repo_slug}`.
    fn repo_base(&self) -> String {
        format!("/repositories/{}/{}", self.workspace, self.repo_slug)
    }

    /// Build the base API path for a PR or issue (`noteable_type`, mirroring `gitlab.rs`'s
    /// `noteable_base`: the task's `target_type` — `"pull_request"` → PR, anything else → issue).
    /// Defaults to PR when `noteable_type` is `None` (legacy rows), same default as GitLab.
    fn noteable_base(&self, id: i64, noteable_type: Option<&str>) -> String {
        let is_issue = noteable_type.is_some_and(|t| t != "pull_request");
        if is_issue {
            format!("{}/issues/{}", self.repo_base(), id)
        } else {
            format!("{}/pullrequests/{}", self.repo_base(), id)
        }
    }

    /// Split a raw unified diff (as returned by `/pullrequests/{id}/diff`) into per-file chunks on
    /// `"diff --git a/... b/..."` boundaries. Bitbucket Cloud has no per-file JSON diff endpoint —
    /// this reconstructs the `ChangedFile { path, patch }` shape from the one text endpoint available.
    fn split_unified_diff(diff: &str) -> Vec<ChangedFile> {
        let mut files = Vec::new();
        let mut current_path: Option<String> = None;
        let mut current_body = String::new();

        for line in diff.lines() {
            if let Some(path) = line
                .strip_prefix("diff --git ")
                .and_then(Self::diff_git_new_path)
            {
                if let Some(prev_path) = current_path.take() {
                    files.push(ChangedFile {
                        path: prev_path,
                        patch: Some(std::mem::take(&mut current_body)),
                    });
                }
                current_path = Some(path);
            }
            if current_path.is_some() {
                current_body.push_str(line);
                current_body.push('\n');
            }
        }
        if let Some(path) = current_path {
            files.push(ChangedFile {
                path,
                patch: Some(current_body),
            });
        }
        files
    }

    /// Parse the new-side path out of a `"a/<path> b/<path>"` diff header tail (the part after
    /// `"diff --git "`). Uses the `" b/"` side (the post-change path), matching GitLab's
    /// `new_path`/GitHub's head-side convention.
    fn diff_git_new_path(header_tail: &str) -> Option<String> {
        let idx = header_tail.rfind(" b/")?;
        Some(header_tail[idx + 3..].trim_end().to_string())
    }

    async fn fetch_pr(&self, pr_number: i64) -> anyhow::Result<serde_json::Value> {
        let url = self.url(&format!("{}/pullrequests/{}", self.repo_base(), pr_number));
        let resp = self
            .http
            .get(&url)
            .basic_auth(&self.username, Some(&self.app_password))
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json().await?)
    }
}

/// One configured Bitbucket repo and its private API/webhook client.
#[derive(Clone)]
pub struct BitbucketProject {
    pub id: i64,
    pub bot_handle: String,
    pub client: BitbucketClient,
}

/// File-configured Bitbucket repo registry keyed by [`platform::stable_id_from_key`] of
/// `workspace/repo_slug` (Bitbucket's substitute for GitLab's numeric `project.id`).
#[derive(Clone, Default)]
pub struct BitbucketRegistry {
    by_id: std::collections::HashMap<i64, BitbucketProject>,
}

impl BitbucketRegistry {
    pub fn from_config(section: &BitbucketSection) -> anyhow::Result<Option<Self>> {
        section.validate()?;
        if !section.enabled {
            return Ok(None);
        }

        let mut by_id = std::collections::HashMap::new();
        for project in &section.projects {
            let api_url = section.resolved_api_url(project).to_string();
            let bot_handle = section.resolved_bot_handle(project).to_string();
            let id = project.stable_id();
            let client = BitbucketClient::new(
                api_url,
                project.workspace.clone(),
                project.repo_slug.clone(),
                project.username.clone(),
                project.app_password.clone(),
                project.webhook_secret.clone(),
            )?;
            by_id.insert(
                id,
                BitbucketProject {
                    id,
                    bot_handle,
                    client,
                },
            );
        }

        tracing::info!(
            projects = by_id.len(),
            "configured Bitbucket projects from control-plane file config"
        );
        Ok(Some(Self { by_id }))
    }

    pub fn get(&self, id: i64) -> Option<&BitbucketProject> {
        self.by_id.get(&id)
    }

    pub fn client_for_project(&self, id: i64) -> Option<&BitbucketClient> {
        self.get(id).map(|project| &project.client)
    }

    pub fn client_for_repo(&self, repo: &RepoRef) -> Option<&BitbucketClient> {
        self.client_for_project(repo.installation_id)
    }

    pub fn bot_handle(&self, id: i64) -> Option<&str> {
        self.get(id).map(|project| project.bot_handle.as_str())
    }

    pub fn is_configured(&self) -> bool {
        !self.by_id.is_empty()
    }
}

/// `CodePlatform` adapter that preserves platform-level dispatch while selecting the concrete
/// Bitbucket client by `RepoRef.installation_id` (the derived stable id for Bitbucket).
#[derive(Clone)]
pub struct BitbucketPlatformRouter {
    registry: BitbucketRegistry,
}

impl BitbucketPlatformRouter {
    pub fn new(registry: BitbucketRegistry) -> Self {
        Self { registry }
    }

    fn client<'a>(&'a self, repo: &RepoRef) -> anyhow::Result<&'a BitbucketClient> {
        self.registry.client_for_repo(repo).ok_or_else(|| {
            anyhow::anyhow!("Bitbucket project {} is not configured", repo.installation_id)
        })
    }
}

#[async_trait]
impl CodePlatform for BitbucketPlatformRouter {
    fn name(&self) -> &'static str {
        "bitbucket"
    }

    fn verify_webhook(&self, _headers: &axum::http::HeaderMap, _body: &[u8]) -> bool {
        // Project-specific Bitbucket webhook verification needs the JSON payload's repository
        // identity to choose the right secret, so `http::webhook` calls the resolved project
        // client directly (mirroring GitlabPlatformRouter::verify_webhook).
        false
    }

    fn delivery_id(&self, headers: &axum::http::HeaderMap) -> Option<String> {
        headers
            .get("x-request-uuid")?
            .to_str()
            .ok()
            .map(|s| s.to_string())
    }

    fn event_type(&self, headers: &axum::http::HeaderMap) -> Option<String> {
        headers
            .get("x-event-key")?
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
        match self.registry.client_for_repo(repo) {
            Some(client) => client.clone_url(repo),
            None => {
                tracing::warn!(
                    project_id = repo.installation_id,
                    "Bitbucket clone URL requested for unconfigured project"
                );
                String::new()
            }
        }
    }
}

#[async_trait]
impl CodePlatform for BitbucketClient {
    fn name(&self) -> &'static str {
        "bitbucket"
    }

    fn verify_webhook(&self, headers: &axum::http::HeaderMap, body: &[u8]) -> bool {
        // Bitbucket Cloud's webhook request-signing feature: HMAC-SHA256 over the raw body, sent as
        // `X-Hub-Signature: sha256=<hex>` — the same header name/format GitHub uses. This mirrors
        // `GithubApp::verify_webhook` exactly (constant-time compare included); see the module doc
        // comment for the confidence caveat on the exact header name/format.
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        type HmacSha256 = Hmac<Sha256>;

        if self.webhook_secret.is_empty() {
            tracing::warn!("Bitbucket verify_webhook failed: configured webhook_secret is empty");
            return false; // fail-closed
        }
        let sig_header = match headers.get("x-hub-signature").and_then(|v| v.to_str().ok()) {
            Some(s) => s,
            None => {
                tracing::warn!("Bitbucket verify_webhook failed: X-Hub-Signature header is missing");
                return false;
            }
        };
        let expected_hex = match sig_header.strip_prefix("sha256=") {
            Some(hex) => hex,
            None => {
                tracing::warn!(
                    "Bitbucket verify_webhook failed: X-Hub-Signature missing sha256= prefix"
                );
                return false;
            }
        };
        let mut mac = match HmacSha256::new_from_slice(self.webhook_secret.as_bytes()) {
            Ok(m) => m,
            Err(_) => return false,
        };
        mac.update(body);
        let computed_hex = hex::encode(mac.finalize().into_bytes());

        use subtle::ConstantTimeEq;
        let is_valid: bool = computed_hex.as_bytes().ct_eq(expected_hex.as_bytes()).into();
        if !is_valid {
            tracing::warn!("Bitbucket verify_webhook failed: signature mismatch");
        }
        is_valid
    }

    fn delivery_id(&self, headers: &axum::http::HeaderMap) -> Option<String> {
        headers
            .get("x-request-uuid")?
            .to_str()
            .ok()
            .map(|s| s.to_string())
    }

    fn event_type(&self, headers: &axum::http::HeaderMap) -> Option<String> {
        headers
            .get("x-event-key")?
            .to_str()
            .ok()
            .map(|s| s.to_string())
    }

    async fn list_changed_files(
        &self,
        _repo: &RepoRef,
        pr_number: i64,
    ) -> anyhow::Result<Vec<ChangedFile>> {
        // No per-file JSON diff endpoint exists in Bitbucket Cloud's API v2.0; `/diff` returns one
        // raw unified diff for the whole PR, split into per-file chunks below (see module doc).
        let url = self.url(&format!("{}/pullrequests/{}/diff", self.repo_base(), pr_number));
        let resp = self
            .http
            .get(&url)
            .basic_auth(&self.username, Some(&self.app_password))
            .send()
            .await?
            .error_for_status()?;
        let text = resp.text().await?;
        Ok(Self::split_unified_diff(&text))
    }

    async fn default_branch(&self, _repo: &RepoRef) -> anyhow::Result<String> {
        let url = self.url(&self.repo_base());
        let resp = self
            .http
            .get(&url)
            .basic_auth(&self.username, Some(&self.app_password))
            .send()
            .await?
            .error_for_status()?;
        let v: serde_json::Value = resp.json().await?;
        v.get("mainbranch")
            .and_then(|b| b.get("name"))
            .and_then(|n| n.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("Bitbucket repository: missing 'mainbranch.name'"))
    }

    async fn pr_shas(
        &self,
        _repo: &RepoRef,
        pr_number: i64,
    ) -> anyhow::Result<(Option<String>, Option<String>)> {
        let v = self.fetch_pr(pr_number).await?;
        let base_sha = v
            .get("destination")
            .and_then(|d| d.get("commit"))
            .and_then(|c| c.get("hash"))
            .and_then(|h| h.as_str())
            .map(|s| s.to_string());
        let head_sha = v
            .get("source")
            .and_then(|d| d.get("commit"))
            .and_then(|c| c.get("hash"))
            .and_then(|h| h.as_str())
            .map(|s| s.to_string());
        Ok((base_sha, head_sha))
    }

    async fn post_review(
        &self,
        _repo: &RepoRef,
        review: &ReviewPost,
    ) -> anyhow::Result<PostedReview> {
        let pr_number = review.pr_number;

        // Bitbucket Cloud has no "review" aggregate: post each inline comment individually via the
        // PR-comments endpoint with an `inline` object, then the body as a general PR comment.
        for c in &review.comments {
            let body = serde_json::json!({
                "content": { "raw": c.body },
                "inline": { "to": c.line, "path": c.path },
            });
            let url = self.url(&format!("{}/pullrequests/{}/comments", self.repo_base(), pr_number));
            let _ = self
                .http
                .post(&url)
                .basic_auth(&self.username, Some(&self.app_password))
                .json(&body)
                .send()
                .await?
                .error_for_status()?;
        }

        let note_url = self.url(&format!("{}/pullrequests/{}/comments", self.repo_base(), pr_number));
        let note_body = serde_json::json!({ "content": { "raw": review.body } });
        let resp = self
            .http
            .post(&note_url)
            .basic_auth(&self.username, Some(&self.app_password))
            .json(&note_body)
            .send()
            .await?
            .error_for_status()?;
        let v: serde_json::Value = resp.json().await?;
        let id = v.get("id").and_then(|i| i.as_i64());
        let html_url = v
            .get("links")
            .and_then(|l| l.get("html"))
            .and_then(|h| h.get("href"))
            .and_then(|s| s.as_str())
            .map(|s| s.to_string());
        Ok(PostedReview { id, html_url })
    }

    async fn post_comment(
        &self,
        _repo: &RepoRef,
        issue_number: i64,
        body: &str,
        noteable_type: Option<&str>,
    ) -> anyhow::Result<PostedComment> {
        let endpoint = format!("{}/comments", self.noteable_base(issue_number, noteable_type));
        let url = self.url(&endpoint);
        let payload = serde_json::json!({ "content": { "raw": body } });
        let resp = self
            .http
            .post(&url)
            .basic_auth(&self.username, Some(&self.app_password))
            .json(&payload)
            .send()
            .await?
            .error_for_status()?;
        let v: serde_json::Value = resp.json().await?;
        let html_url = v
            .get("links")
            .and_then(|l| l.get("html"))
            .and_then(|h| h.get("href"))
            .and_then(|s| s.as_str())
            .map(|s| s.to_string());
        Ok(PostedComment {
            id: v.get("id").and_then(|i| i.as_i64()),
            html_url,
        })
    }

    async fn add_reaction(
        &self,
        _repo: &RepoRef,
        _target: ReactionTarget,
        _emoji: &str,
        _noteable_type: Option<&str>,
    ) -> anyhow::Result<()> {
        // Documented simplification (ADR-0108): Bitbucket Cloud's REST API v2.0 has no
        // comment-reaction / award-emoji endpoint equivalent to GitHub reactions or GitLab award
        // emoji. There is nothing to call here; a no-op rather than an error since the caller (the
        // lifecycle-reaction path) treats this as best-effort UX, not a required step.
        tracing::debug!(
            "Bitbucket add_reaction is a no-op: the platform has no comment-reaction API"
        );
        Ok(())
    }

    async fn add_labels(
        &self,
        _repo: &RepoRef,
        _issue_number: i64,
        _labels: &[String],
    ) -> anyhow::Result<()> {
        // Documented simplification (ADR-0108): Bitbucket Cloud pull requests have no native label
        // feature (unlike GitHub issues/PRs or GitLab MRs). A no-op rather than an error, since
        // outcome labels are a UX nicety, not required for the review to land.
        tracing::debug!("Bitbucket add_labels is a no-op: the platform has no PR-label API");
        Ok(())
    }

    async fn list_review_comments(
        &self,
        _repo: &RepoRef,
        pr_number: i64,
        _review_id: i64,
    ) -> anyhow::Result<Vec<ReviewCommentRef>> {
        let url = self.url(&format!(
            "{}/pullrequests/{}/comments?pagelen=100",
            self.repo_base(),
            pr_number
        ));
        let resp = self
            .http
            .get(&url)
            .basic_auth(&self.username, Some(&self.app_password))
            .send()
            .await?
            .error_for_status()?;
        let v: serde_json::Value = resp.json().await?;
        let values = v
            .get("values")
            .and_then(|c| c.as_array())
            .ok_or_else(|| anyhow::anyhow!("Bitbucket PR comments: missing 'values' array"))?;
        let mut out = Vec::new();
        for item in values {
            // Only comments carrying an `inline` object are inline review comments.
            let Some(inline) = item.get("inline") else {
                continue;
            };
            let id = item.get("id").and_then(|i| i.as_i64()).unwrap_or(0);
            let path = inline
                .get("path")
                .and_then(|p| p.as_str())
                .map(|s| s.to_string());
            let line = inline.get("to").and_then(|l| l.as_i64());
            out.push(ReviewCommentRef { id, path, line });
        }
        Ok(out)
    }

    async fn list_comment_reactions(
        &self,
        _repo: &RepoRef,
        _comment_id: i64,
        _is_review_comment: bool,
        _iid: Option<i64>,
        _noteable_type: Option<&str>,
    ) -> anyhow::Result<Vec<Reaction>> {
        // Documented simplification (ADR-0108): no reaction API on Bitbucket Cloud (see
        // `add_reaction`) — there is never anything to find, so the 👍/👎 feedback poll
        // (`reconcile_comment_feedback`, ADR-0035) always sees an empty list for this platform and
        // simply never suppresses a finding via feedback here, rather than erroring every cycle.
        Ok(vec![])
    }

    fn clone_url(&self, repo: &RepoRef) -> String {
        // Bitbucket Cloud's git-over-HTTPS host is always `bitbucket.org`, independent of the API
        // host (`api.bitbucket.org` or an on-prem-style override) — unlike GitLab, where the API and
        // clone hosts share the same base. HTTP Basic embedded in the URL (`user:app_password@host`)
        // mirrors GitLab's `oauth2:TOKEN@host` embedding.
        format!(
            "https://{}:{}@bitbucket.org/{}.git",
            self.username, self.app_password, repo.full_name
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BitbucketProjectConfig, BitbucketSection};

    fn project(workspace: &str, repo_slug: &str, app_password: &str, secret: &str) -> BitbucketProjectConfig {
        BitbucketProjectConfig {
            workspace: workspace.to_string(),
            repo_slug: repo_slug.to_string(),
            api_url: None,
            username: "bot".to_string(),
            app_password: app_password.to_string(),
            webhook_secret: secret.to_string(),
            bot_handle: None,
        }
    }

    #[test]
    fn disabled_config_builds_no_registry() {
        let section = BitbucketSection::default();
        let registry =
            BitbucketRegistry::from_config(&section).expect("disabled config is valid");
        assert!(registry.is_none());
    }

    #[test]
    fn registry_resolves_clients_and_handles_by_derived_id() {
        let section = BitbucketSection {
            enabled: true,
            default_api_url: Some("https://api.bitbucket.example.com/2.0".to_string()),
            default_bot_handle: Some("lightbridge-bot".to_string()),
            projects: vec![
                project("myteam", "repo-a", "pw-a", "secret-a"),
                BitbucketProjectConfig {
                    bot_handle: Some("lb-reviewer".to_string()),
                    ..project("myteam", "repo-b", "pw-b", "secret-b")
                },
            ],
        };
        let registry = BitbucketRegistry::from_config(&section)
            .expect("valid config builds")
            .expect("enabled config produces registry");

        assert!(registry.is_configured());
        let id_a = section.projects[0].stable_id();
        let id_b = section.projects[1].stable_id();
        assert_eq!(registry.bot_handle(id_a), Some("lightbridge-bot"));
        assert_eq!(registry.bot_handle(id_b), Some("lb-reviewer"));
        assert!(registry.client_for_project(id_a).is_some());
        assert!(registry.client_for_project(9999).is_none());

        let repo = RepoRef {
            platform: Platform::Bitbucket,
            full_name: "myteam/repo-b".to_string(),
            platform_repo_id: id_b,
            installation_id: id_b,
        };
        let clone_url = registry
            .client_for_repo(&repo)
            .expect("repo resolves through installation_id")
            .clone_url(&repo);
        assert!(clone_url.contains("bot:pw-b@bitbucket.org"));
        assert!(clone_url.ends_with("/myteam/repo-b.git"));
    }

    #[test]
    fn verify_webhook_accepts_a_valid_signature_and_rejects_a_tampered_one() {
        let client = BitbucketClient::new(
            "https://api.bitbucket.example.com/2.0".to_string(),
            "myteam".to_string(),
            "repo-a".to_string(),
            "bot".to_string(),
            "pw".to_string(),
            "whsecret".to_string(),
        )
        .unwrap();

        use hmac::{Hmac, KeyInit, Mac};
        use sha2::Sha256;
        type HmacSha256 = Hmac<Sha256>;

        let body = b"{\"pullrequest\":{}}";
        let mut mac = HmacSha256::new_from_slice(b"whsecret").unwrap();
        mac.update(body);
        let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-hub-signature", sig.parse().unwrap());
        assert!(client.verify_webhook(&headers, body));

        let mut tampered_headers = axum::http::HeaderMap::new();
        tampered_headers.insert("x-hub-signature", "sha256=deadbeef".parse().unwrap());
        assert!(!client.verify_webhook(&tampered_headers, body));

        // Missing header → rejected (fail-closed).
        assert!(!client.verify_webhook(&axum::http::HeaderMap::new(), body));
    }

    #[test]
    fn verify_webhook_rejects_when_secret_is_empty() {
        let client = BitbucketClient::new(
            "https://api.bitbucket.example.com/2.0".to_string(),
            "myteam".to_string(),
            "repo-a".to_string(),
            "bot".to_string(),
            "pw".to_string(),
            String::new(),
        )
        .unwrap();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-hub-signature", "sha256=anything".parse().unwrap());
        assert!(!client.verify_webhook(&headers, b"body"));
    }

    #[test]
    fn split_unified_diff_separates_multiple_files() {
        let diff = "diff --git a/foo.rs b/foo.rs\nindex 111..222 100644\n--- a/foo.rs\n+++ b/foo.rs\n@@ -1 +1 @@\n-old\n+new\ndiff --git a/bar.rs b/bar.rs\nindex 333..444 100644\n--- a/bar.rs\n+++ b/bar.rs\n@@ -1 +1 @@\n-a\n+b\n";
        let files = BitbucketClient::split_unified_diff(diff);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "foo.rs");
        assert!(files[0].patch.as_ref().unwrap().contains("-old"));
        assert_eq!(files[1].path, "bar.rs");
        assert!(files[1].patch.as_ref().unwrap().contains("+b"));
    }

    #[test]
    fn split_unified_diff_handles_empty_input() {
        assert!(BitbucketClient::split_unified_diff("").is_empty());
    }
}
