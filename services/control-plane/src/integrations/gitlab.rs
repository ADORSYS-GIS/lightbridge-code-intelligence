//! GitLab integration — `CodePlatform` implementation for GitLab.
//!
//! GitLab's API model differs from GitHub's in three ways that shape this file:
//!
//! 1. **Auth is a single static token** (`PRIVATE-TOKEN` header), not per-installation tokens.
//!    `GITLAB_API_TOKEN` (project/group/personal access token) is all we need; there is no
//!    installation/App-JWT dance.
//! 2. **No "review" object** — inline comments are "discussion threads" with a `position` object
//!    (base/head/start SHA + path + line), and the review body is a plain MR note. `post_review`
//!    fetches the MR's `diff_refs` first, posts each inline as a discussion, then the body as a note.
//! 3. **Webhook auth is a plain token** (`X-Gitlab-Token` header), not HMAC. `verify_webhook`
//!    does a constant-time comparison against `GITLAB_WEBHOOK_SECRET`.
//!
//! Known limitations (Phase 4):
//! - `list_comment_reactions` returns an empty Vec — feedback polling (👍/👎) requires the MR/issue
//!   `iid` which we don't have from just a note ID. Phase 7 can store the iid alongside the note.
//! - `add_reaction` on a `Comment` is a no-op (same reason). Issue/MR body reactions work.
//! - `post_comment` tries MR notes first, then issue notes (the caller passes a single `issue_number`
//!   which is the MR `iid` for PRs or the issue `iid` for issues — we don't know which, so we probe).

#![allow(dead_code)]

use async_trait::async_trait;
use reqwest::Client;

use crate::integrations::platform::*;

/// GitLab API client. One static token, one base URL — no token minting.
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
    /// Construct from env. Returns `None` when `GITLAB_API_TOKEN` is unset/empty (GitLab not
    /// configured). `GITLAB_API_URL` defaults to `https://gitlab.com/api/v4` (GitLab.com SaaS);
    /// self-hosted instances set it to `https://<host>/api/v4`.
    pub fn from_env() -> Option<Self> {
        let token = std::env::var("GITLAB_API_TOKEN").unwrap_or_default();
        if token.is_empty() {
            return None;
        }
        // Fail fast if the token contains bytes that can't be a valid header value —
        // otherwise every API call would silently go out unauthenticated (ADR-0072).
        if reqwest::header::HeaderValue::from_str(&token).is_err() {
            tracing::error!("GITLAB_API_TOKEN contains invalid header bytes — GitLab disabled");
            return None;
        }
        let api_url = std::env::var("GITLAB_API_URL")
            .unwrap_or_else(|_| "https://gitlab.com/api/v4".to_string())
            .trim_end_matches('/')
            .to_string();
        let webhook_secret = std::env::var("GITLAB_WEBHOOK_SECRET").unwrap_or_default();
        let http = Client::builder()
            .user_agent("lightbridge-code-intelligence")
            .build()
            .ok()?;
        Some(Self {
            api_url,
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
        let payload = serde_json::json!({ "body": body });

        // Phase A (ADR-0072): use `noteable_type` to route directly — no probe.
        // GitLab MRs and issues share iid sequences (both start at 1), so a probe would
        // succeed on the wrong noteable. `target_type` is `"pull_request"` or `"issue"`.
        let is_mr = noteable_type.map(|t| t == "pull_request").unwrap_or(true); // default to MR (the common case for reviews)

        let endpoint = if is_mr {
            format!(
                "/projects/{}/merge_requests/{}/notes",
                project, issue_number
            )
        } else {
            format!("/projects/{}/issues/{}/notes", project, issue_number)
        };
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
                // Phase A (ADR-0072): use `noteable_type` to route directly — no probe.
                let is_mr = noteable_type.map(|t| t == "pull_request").unwrap_or(true); // default to MR (the common case for reviews)

                let endpoint = if is_mr {
                    format!(
                        "/projects/{}/merge_requests/{}/award_emoji",
                        project, number
                    )
                } else {
                    format!("/projects/{}/issues/{}/award_emoji", project, number)
                };
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
            ReactionTarget::Comment { comment_id: _ } => {
                // Known limitation (Phase 4): awarding emoji on a note requires the MR/issue iid,
                // which we don't have from just the note ID. Logged, skipped.
                tracing::debug!(
                    "gitlab add_reaction on comment skipped (iid lookup not implemented in Phase 4)"
                );
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
        _repo: &RepoRef,
        _comment_id: i64,
        _is_review_comment: bool,
    ) -> anyhow::Result<Vec<Reaction>> {
        // Known limitation (Phase 4): listing award emoji on a note requires the MR/issue iid,
        // which we don't have from just the note ID. Feedback polling (👍/👎) is a no-op for GitLab
        // until Phase 7 stores the iid alongside the note.
        Ok(Vec::new())
    }

    fn clone_url(&self, repo: &RepoRef) -> String {
        // Embed the token for HTTPS clone (oauth2:TOKEN@host form).
        // Strip the `/api/v4` suffix to get the base host URL, then strip the
        // protocol prefix to avoid a doubled scheme in the final URL.
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
