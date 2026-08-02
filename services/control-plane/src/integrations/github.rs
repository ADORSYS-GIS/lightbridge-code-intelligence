//! GitHub App authentication.
//!
//! Mints a short-lived **App JWT** (RS256, signed with the App private key) and exchanges it for an
//! **installation access token** — the credential used to call the GitHub API as an installation
//! (read repo contents, post review comments). Config: `GITHUB_APP_ID` + `GITHUB_APP_PRIVATE_KEY`
//! (PEM). Absent either, [`GithubApp::from_env`] returns `None`; webhook handling and task creation
//! still work — only authenticated GitHub API calls require a token.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::KeyInit;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::{Deserialize, Serialize};

/// A cached installation token with its expiry epoch-second. GitHub installation
/// tokens expire after 1 hour; we cache with a 50-minute TTL (10-minute safety
/// margin) so repeated trait method calls within the same reconciler drain batch
/// reuse one token instead of minting per-method (ADR-0072).
struct CachedToken {
    token: String,
    expires_at: u64,
}

/// 50 minutes — 10 minutes before GitHub's 1-hour installation token expiry.
const TOKEN_CACHE_TTL_SECS: u64 = 50 * 60;

#[derive(Clone)]
pub struct GithubApp {
    app_id: String,
    key: EncodingKey,
    http: reqwest::Client,
    token_cache: Arc<Mutex<HashMap<i64, CachedToken>>>,
}

#[derive(Debug, Serialize)]
struct AppClaims {
    iat: u64,
    exp: u64,
    iss: String,
}

impl GithubApp {
    /// Build from env. `None` when `GITHUB_APP_ID` / `GITHUB_APP_PRIVATE_KEY` are unset or the key
    /// is not valid RSA PEM (logged, non-fatal — the App features stay disabled).
    pub fn from_env() -> Option<Self> {
        let app_id = std::env::var("GITHUB_APP_ID").ok()?;
        let pem = std::env::var("GITHUB_APP_PRIVATE_KEY").ok()?;
        match EncodingKey::from_rsa_pem(pem.as_bytes()) {
            Ok(key) => Some(Self {
                app_id,
                key,
                http: reqwest::Client::new(),
                token_cache: Arc::new(Mutex::new(HashMap::new())),
            }),
            Err(error) => {
                tracing::error!(%error, "GITHUB_APP_PRIVATE_KEY is not valid RSA PEM");
                None
            }
        }
    }

    /// Mint a short-lived App JWT (~9 min, backdated 60s for clock skew), per GitHub's App-auth
    /// spec: `iss` = App ID, signed RS256 with the App private key.
    fn app_jwt(&self) -> Result<String, jsonwebtoken::errors::Error> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let claims = AppClaims {
            iat: now - 60,
            exp: now + 9 * 60,
            iss: self.app_id.clone(),
        };
        encode(&Header::new(Algorithm::RS256), &claims, &self.key)
    }

    /// Exchange the App JWT for an installation access token. Uses an in-process
    /// TTL cache (50 min) so repeated calls within the same reconciler drain batch
    /// reuse one token instead of minting per-method (ADR-0072, review-3 P1 #2).
    pub async fn installation_token(&self, installation_id: i64) -> anyhow::Result<String> {
        use anyhow::Context;
        // Fast path: return a cached token if it's still within its TTL.
        if let Some(cached) = self.cached_token(installation_id) {
            return Ok(cached);
        }
        #[derive(Deserialize)]
        struct TokenResponse {
            token: String,
        }
        let jwt = self.app_jwt().context("minting app jwt")?;
        let token = self
            .http
            .post(format!(
                "https://api.github.com/app/installations/{installation_id}/access_tokens"
            ))
            .header("Authorization", format!("Bearer {jwt}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "lightbridge-code-intelligence")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .context("requesting installation token")?
            .error_for_status()
            .context("github rejected the installation token request")?
            .json::<TokenResponse>()
            .await
            .context("parsing installation token response")?
            .token;
        // Cache the freshly minted token with a 50-minute TTL.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if let Ok(mut cache) = self.token_cache.lock() {
            cache.insert(
                installation_id,
                CachedToken {
                    token: token.clone(),
                    expires_at: now + TOKEN_CACHE_TTL_SECS,
                },
            );
        }
        Ok(token)
    }

    /// Return a cached token if one exists and is still within its TTL, else `None`.
    fn cached_token(&self, installation_id: i64) -> Option<String> {
        let cache = self.token_cache.lock().ok()?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        cache
            .get(&installation_id)
            .filter(|c| c.expires_at > now)
            .map(|c| c.token.clone())
    }

    /// Fetch a PR's changed files with their unified-diff patches (first page, up to 100 files —
    /// enough for typical PRs; pagination is a follow-up). Used to validate which finding lines are
    /// commentable (see `review::diff_lines`).
    pub async fn list_pr_files(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        pr: i64,
    ) -> anyhow::Result<Vec<PrFile>> {
        use anyhow::Context;
        let files = self
            .http
            .get(format!(
                "https://api.github.com/repos/{owner}/{repo}/pulls/{pr}/files?per_page=100"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "lightbridge-code-intelligence")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .context("requesting PR files")?
            .error_for_status()
            .context("github rejected the PR-files request")?
            .json::<Vec<PrFile>>()
            .await
            .context("parsing PR files")?;
        Ok(files)
    }

    /// Post a PR review (`event: COMMENT`) with a body and optional inline comments. GitHub rejects
    /// the whole review if any comment's line isn't in the diff, so the caller must pre-validate.
    /// Post the review; returns its `html_url` (the permalink to the review on the PR) when GitHub
    /// includes it, so the console can link to what was posted.
    pub async fn create_pr_review(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        pr: i64,
        body: &str,
        comments: &[ReviewComment],
    ) -> anyhow::Result<PostedReview> {
        use anyhow::Context;
        let payload = serde_json::json!({
            "body": body,
            "event": "COMMENT",
            "comments": comments,
        });
        // The create-review response is a single review object — `id` + `html_url`. It does NOT carry
        // the per-inline-comment ids (those need a follow-up GET .../reviews/{id}/comments); we keep
        // the review id now so feedback (ADR-0035) can correlate back to this run.
        #[derive(Deserialize)]
        struct ReviewResponse {
            id: Option<i64>,
            html_url: Option<String>,
        }
        let review: ReviewResponse = self
            .http
            .post(format!(
                "https://api.github.com/repos/{owner}/{repo}/pulls/{pr}/reviews"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "lightbridge-code-intelligence")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&payload)
            .send()
            .await
            .context("posting PR review")?
            .error_for_status()
            .context("github rejected the PR review")?
            .json()
            .await
            .context("parsing PR review response")?;
        Ok(PostedReview {
            id: review.id,
            html_url: review.html_url,
        })
    }

    /// Attach `details_url` to a check-run payload **only when it is present**.
    ///
    /// Load-bearing, not stylistic: GitHub rejects an explicit JSON `null` on this field with
    /// `422 Invalid request. For 'properties/details_url', nil is not a string.` — reproduced live
    /// against the real API on 2026-08-02, which is what made every check-run intent dead-letter
    /// after the feature first shipped. `serde_json::json!` serializes an `Option::None` as `null`,
    /// so the key must be inserted conditionally rather than interpolated.
    ///
    /// This is the SAME omit-don't-null contract ADR-0071 pinned for `start_line`/`start_side` on
    /// the review endpoint (see `review_comment_without_start_line_serializes_unchanged`) — different
    /// field, identical GitHub behavior, so it gets the same treatment and the same regression test.
    fn attach_details_url(payload: &mut serde_json::Value, details_url: Option<&str>) {
        if let Some(url) = details_url {
            payload["details_url"] = serde_json::Value::String(url.to_string());
        }
    }

    /// The `in_progress` check-run creation body. Pure, so the omit-don't-null contract is
    /// unit-tested without HTTP.
    fn check_run_start_payload(head_sha: &str, details_url: Option<&str>) -> serde_json::Value {
        let mut payload = serde_json::json!({
            "name": CHECK_RUN_NAME,
            "head_sha": head_sha,
            "status": "in_progress",
        });
        Self::attach_details_url(&mut payload, details_url);
        payload
    }

    /// The `completed` check-run body for a PATCH (no `head_sha`/`name` — the run already exists).
    fn check_run_update_payload(
        conclusion: &str,
        summary: &str,
        details_url: Option<&str>,
    ) -> serde_json::Value {
        let mut payload = serde_json::json!({
            "status": "completed",
            "conclusion": conclusion,
            "output": { "title": CHECK_RUN_NAME, "summary": summary },
        });
        Self::attach_details_url(&mut payload, details_url);
        payload
    }

    /// The already-`completed` check-run creation body (the self-healing fallback).
    fn check_run_completed_payload(
        head_sha: &str,
        conclusion: &str,
        summary: &str,
        details_url: Option<&str>,
    ) -> serde_json::Value {
        let mut payload = serde_json::json!({
            "name": CHECK_RUN_NAME,
            "head_sha": head_sha,
            "status": "completed",
            "conclusion": conclusion,
            "output": { "title": CHECK_RUN_NAME, "summary": summary },
        });
        Self::attach_details_url(&mut payload, details_url);
        payload
    }

    /// Fail with GitHub's OWN response body attached.
    ///
    /// `error_for_status()` discards the body, so a rejected check-run write surfaced only as
    /// "github rejected the check-run creation" with no reason — the 422 above was invisible in
    /// production logs and took a live API reproduction to identify. Anything that reaches an
    /// operator's logs from these endpoints should say what GitHub actually complained about.
    async fn ensure_success(
        response: reqwest::Response,
        context: &str,
    ) -> anyhow::Result<reqwest::Response> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("{context}: HTTP {status}: {body}")
    }

    /// Open a Check Run in `in_progress` status (`POST repos/{owner}/{repo}/check-runs`). Returns its
    /// id, kept so `update_check_run_completed` can address the SAME check run later rather than
    /// opening a second one.
    pub async fn create_check_run_in_progress(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        head_sha: &str,
        details_url: Option<&str>,
    ) -> anyhow::Result<i64> {
        use anyhow::Context;
        #[derive(Deserialize)]
        struct CheckRunResponse {
            id: i64,
        }
        let response = self
            .http
            .post(format!(
                "https://api.github.com/repos/{owner}/{repo}/check-runs"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "lightbridge-code-intelligence")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&Self::check_run_start_payload(head_sha, details_url))
            .send()
            .await
            .context("opening check run")?;
        let created: CheckRunResponse =
            Self::ensure_success(response, "github rejected the check-run creation")
                .await?
                .json()
                .await
                .context("parsing check-run creation response")?;
        Ok(created.id)
    }

    /// Resolve a previously-opened Check Run (`PATCH repos/{owner}/{repo}/check-runs/{id}`).
    #[allow(clippy::too_many_arguments)] // mirrors db::outbox::enqueue_outbox_post's precedent
    pub async fn update_check_run_completed(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        check_run_id: i64,
        conclusion: &str,
        summary: &str,
        details_url: Option<&str>,
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        let response = self
            .http
            .patch(format!(
                "https://api.github.com/repos/{owner}/{repo}/check-runs/{check_run_id}"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "lightbridge-code-intelligence")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&Self::check_run_update_payload(
                conclusion,
                summary,
                details_url,
            ))
            .send()
            .await
            .context("resolving check run")?;
        Self::ensure_success(response, "github rejected the check-run update").await?;
        Ok(())
    }

    /// Self-healing fallback: create a Check Run that is ALREADY `completed`, in one call — used when
    /// no id was ever recorded for the start (e.g. the start intent dead-lettered on a permission-
    /// missing installation). Still gives the PR SOME check on this SHA rather than none.
    #[allow(clippy::too_many_arguments)] // mirrors db::outbox::enqueue_outbox_post's precedent
    pub async fn create_check_run_completed(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        head_sha: &str,
        conclusion: &str,
        summary: &str,
        details_url: Option<&str>,
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        let response = self
            .http
            .post(format!(
                "https://api.github.com/repos/{owner}/{repo}/check-runs"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "lightbridge-code-intelligence")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&Self::check_run_completed_payload(
                head_sha,
                conclusion,
                summary,
                details_url,
            ))
            .send()
            .await
            .context("opening a completed check run")?;
        Self::ensure_success(response, "github rejected the completed check-run creation").await?;
        Ok(())
    }

    /// Post a plain comment on an issue or PR thread (`POST issues/{n}/comments`). Used for the `ask`
    /// run kind (ADR-0033): a conversational answer, not a diff-scoped review. PRs share the issues
    /// comment endpoint, so this works for either target. Returns the comment's `id` (kept so the
    /// feedback poller can read its reactions, ADR-0035) + `html_url`.
    pub async fn create_issue_comment(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        issue: i64,
        body: &str,
    ) -> anyhow::Result<PostedComment> {
        use anyhow::Context;
        #[derive(Deserialize)]
        struct CommentResponse {
            id: Option<i64>,
            html_url: Option<String>,
        }
        let comment: CommentResponse = self
            .http
            .post(format!(
                "https://api.github.com/repos/{owner}/{repo}/issues/{issue}/comments"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "lightbridge-code-intelligence")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&serde_json::json!({ "body": body }))
            .send()
            .await
            .context("posting issue comment")?
            .error_for_status()
            .context("github rejected the issue comment")?
            .json()
            .await
            .context("parsing issue comment response")?;
        Ok(PostedComment {
            id: comment.id,
            html_url: comment.html_url,
        })
    }

    /// List the inline comments of a posted review (`GET pulls/{pr}/reviews/{review_id}/comments`).
    /// The create-review response omits per-comment ids; we fetch them here so the feedback poller can
    /// read each comment's reactions (ADR-0035). Returns `(comment_id, path, line)` for each.
    pub async fn list_review_comments(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        pr: i64,
        review_id: i64,
    ) -> anyhow::Result<Vec<ReviewCommentRef>> {
        use anyhow::Context;
        #[derive(Deserialize)]
        struct RawComment {
            id: i64,
            #[serde(default)]
            path: Option<String>,
            #[serde(default)]
            line: Option<i64>,
        }
        let raw: Vec<RawComment> = self
            .http
            .get(format!(
                "https://api.github.com/repos/{owner}/{repo}/pulls/{pr}/reviews/{review_id}/comments?per_page=100"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "lightbridge-code-intelligence")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .context("requesting review comments")?
            .error_for_status()
            .context("github rejected the review-comments request")?
            .json()
            .await
            .context("parsing review comments")?;
        Ok(raw
            .into_iter()
            .map(|c| ReviewCommentRef {
                id: c.id,
                path: c.path,
                line: c.line,
            })
            .collect())
    }

    /// Read the reactions on a comment (ADR-0035). The endpoint differs by comment kind: an inline PR
    /// review comment uses `pulls/comments/{id}/reactions`, a plain issue/PR comment uses
    /// `issues/comments/{id}/reactions`. Returns `(reactor_login, reaction_content)` pairs.
    pub async fn list_comment_reactions(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        comment_id: i64,
        is_review_comment: bool,
    ) -> anyhow::Result<Vec<(String, String)>> {
        use anyhow::Context;
        let kind = if is_review_comment { "pulls" } else { "issues" };
        #[derive(Deserialize)]
        struct RawReaction {
            content: String,
            user: Option<RawUser>,
        }
        #[derive(Deserialize)]
        struct RawUser {
            login: String,
        }
        let raw: Vec<RawReaction> = self
            .http
            .get(format!(
                "https://api.github.com/repos/{owner}/{repo}/{kind}/comments/{comment_id}/reactions?per_page=100"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "lightbridge-code-intelligence")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .context("requesting comment reactions")?
            .error_for_status()
            .context("github rejected the reactions request")?
            .json()
            .await
            .context("parsing reactions")?;
        Ok(raw
            .into_iter()
            .filter_map(|r| r.user.map(|u| (u.login, r.content)))
            .collect())
    }

    /// Fetch a repository's default branch. Used by index-on-approve (Epic #75): a repo registered
    /// via an installation webhook has no `default_branch` (that payload omits it), so we resolve it
    /// before indexing.
    pub async fn repository_default_branch(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
    ) -> anyhow::Result<String> {
        use anyhow::Context;
        let value: serde_json::Value = self
            .http
            .get(format!("https://api.github.com/repos/{owner}/{repo}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "lightbridge-code-intelligence")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .context("fetching repository")?
            .error_for_status()
            .context("github rejected the repository fetch")?
            .json()
            .await
            .context("parsing repository")?;
        value["default_branch"]
            .as_str()
            .map(str::to_string)
            .context("repository response missing default_branch")
    }

    /// Fetch a single file's raw text content at `ref_` via the Contents API, or `None` when it
    /// doesn't exist there (404) — used to resolve `.lightbridge-code-review.jsonc` (ADR-0030) before
    /// any clone exists. GitHub base64-encodes the content with embedded newlines (RFC 2045 style); we
    /// strip them before decoding.
    pub async fn repository_file_content(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        ref_: &str,
        path: &str,
    ) -> anyhow::Result<Option<String>> {
        use anyhow::Context;
        use base64::Engine;
        let response = self
            .http
            .get(format!(
                "https://api.github.com/repos/{owner}/{repo}/contents/{path}?ref={ref_}"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "lightbridge-code-intelligence")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .context("fetching repository file content")?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let value: serde_json::Value = response
            .error_for_status()
            .context("github rejected the file-content request")?
            .json()
            .await
            .context("parsing file-content response")?;
        let Some(encoded) = value["content"].as_str() else {
            // A directory (not a file) response has no `content` — treat as absent.
            return Ok(None);
        };
        let stripped: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(stripped)
            .context("decoding base64 file content")?;
        Ok(Some(String::from_utf8_lossy(&decoded).into_owned()))
    }

    /// Create or update a file's content on the repository's default branch via the Contents API
    /// (ADR-0109) — a direct commit, not a PR. Reads the file's current content **and** `sha` as ONE
    /// GET, calls `mutate` on that exact snapshot, then PUTs the result guarded by that same `sha`
    /// (required by GitHub for an update, omitted for a create) — a concurrent edit since that read
    /// surfaces as a 409 from the PUT (`error_for_status` below), never a silent overwrite. Doing the
    /// read and the mutation as one call is load-bearing: a caller that reads separately and passes
    /// final content would race a concurrent editor without ever seeing the 409 (see the trait doc).
    pub async fn update_repository_file_content(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        path: &str,
        mutate: Box<dyn FnOnce(Option<String>) -> String + Send>,
        message: &str,
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        use base64::Engine;
        let contents_url = format!("https://api.github.com/repos/{owner}/{repo}/contents/{path}");
        let existing = self
            .http
            .get(&contents_url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "lightbridge-code-intelligence")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .context("looking up the existing file content")?;
        let (current_content, sha) = if existing.status() == reqwest::StatusCode::NOT_FOUND {
            (None, None)
        } else {
            let value: serde_json::Value = existing
                .error_for_status()
                .context("github rejected the file lookup")?
                .json()
                .await
                .context("parsing file lookup response")?;
            let content = value["content"].as_str().map(|encoded| {
                let stripped: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(stripped)
                    .unwrap_or_default();
                String::from_utf8_lossy(&decoded).into_owned()
            });
            let sha = value["sha"].as_str().map(str::to_string);
            (content, sha)
        };
        let new_content = mutate(current_content);
        let mut body = serde_json::json!({
            "message": message,
            "content": base64::engine::general_purpose::STANDARD.encode(&new_content),
        });
        if let Some(sha) = sha {
            body["sha"] = serde_json::Value::String(sha);
        }
        self.http
            .put(&contents_url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "lightbridge-code-intelligence")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&body)
            .send()
            .await
            .context("committing the file update")?
            .error_for_status()
            .context("github rejected the file commit (e.g. a stale sha ⇒ 409 conflict)")?;
        Ok(())
    }

    /// Fetch a PR's base + head SHAs. Used by the `@mention` re-review path, where the
    /// `issue_comment` payload has no SHAs (unlike the `pull_request` event).
    pub async fn pull_request_shas(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        pr: i64,
    ) -> anyhow::Result<(Option<String>, Option<String>)> {
        use anyhow::Context;
        let value: serde_json::Value = self
            .http
            .get(format!(
                "https://api.github.com/repos/{owner}/{repo}/pulls/{pr}"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "lightbridge-code-intelligence")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .context("fetching pull request")?
            .error_for_status()
            .context("github rejected the pull request fetch")?
            .json()
            .await
            .context("parsing pull request")?;
        let base = value["base"]["sha"].as_str().map(str::to_string);
        let head = value["head"]["sha"].as_str().map(str::to_string);
        Ok((base, head))
    }

    /// React to a PR/issue body (the "description") with one of GitHub's 8 reaction contents
    /// (`eyes`, `hooray`, `confused`, …). Used as lightweight review-lifecycle feedback. Adding the
    /// same reaction twice is a no-op on GitHub's side, so this is safe to retry.
    pub async fn add_reaction(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        issue: i64,
        content: &str,
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        self.http
            .post(format!(
                "https://api.github.com/repos/{owner}/{repo}/issues/{issue}/reactions"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "lightbridge-code-intelligence")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&serde_json::json!({ "content": content }))
            .send()
            .await
            .context("posting reaction")?
            .error_for_status()
            .context("github rejected the reaction")?;
        Ok(())
    }

    /// React to an **issue comment** (an `@mention` that triggered a task, ADR-0068) with one of
    /// GitHub's 8 reaction contents. Distinct from [`add_reaction`] (which targets the PR/issue body)
    /// because the comment-reactions endpoint is a different path
    /// (`/issues/comments/{comment_id}/reactions`). Adding the same reaction twice is a GitHub-side
    /// no-op, so it's safe to retry.
    pub async fn add_comment_reaction(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        comment_id: i64,
        content: &str,
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        self.http
            .post(format!(
                "https://api.github.com/repos/{owner}/{repo}/issues/comments/{comment_id}/reactions"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "lightbridge-code-intelligence")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&serde_json::json!({ "content": content }))
            .send()
            .await
            .context("posting comment reaction")?
            .error_for_status()
            .context("github rejected the comment reaction")?;
        Ok(())
    }

    /// Add labels to a PR/issue. GitHub creates any label that doesn't exist yet (default colour),
    /// and adding an already-present label is idempotent.
    pub async fn add_labels(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        issue: i64,
        labels: &[String],
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        if labels.is_empty() {
            return Ok(());
        }
        self.http
            .post(format!(
                "https://api.github.com/repos/{owner}/{repo}/issues/{issue}/labels"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "lightbridge-code-intelligence")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&serde_json::json!({ "labels": labels }))
            .send()
            .await
            .context("adding labels")?
            .error_for_status()
            .context("github rejected the labels")?;
        Ok(())
    }
}

/// A changed file in a PR, as returned by the PR-files API. `patch` is absent for binary/huge files.
#[derive(Debug, Deserialize)]
pub struct PrFile {
    pub filename: String,
    #[serde(default)]
    pub patch: Option<String>,
}

/// An inline comment in the GitHub "create review" payload (RIGHT = the new file side).
///
/// `start_line`/`start_side` are the ADR-0071 range fields: additive and optional, so a single-line
/// comment (`start_line: None`) serializes to byte-for-byte the same JSON as before that ADR — both are
/// `#[serde(skip_serializing_if = "Option::is_none")]` so they're omitted entirely (not sent as
/// `null`), since GitHub may reject an inline comment payload with an unexpected null field.
#[derive(Debug, Serialize)]
pub struct ReviewComment {
    pub path: String,
    /// Maps directly to GitHub's `line`, which GitHub itself treats as the range's **LAST** line
    /// whenever `start_line` is also present — so this is the range's *end* (and the sole anchor
    /// everything downstream keys on), not a co-equal endpoint. For a single-line comment it's just that
    /// one line.
    pub line: u32,
    pub side: &'static str,
    /// First line of a validated range (ADR-0071), or `None` for a single-line comment — the complement
    /// to `line` (the range's last line); GitHub renders the span from `start_line` to `line`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    /// Always `Some("RIGHT")` when `start_line` is `Some` — this repo doesn't support LEFT-side
    /// (deleted-line) ranges, matching ADR-0022's existing single-line limitation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_side: Option<&'static str>,
    pub body: String,
}

// PostedReview, PostedComment, and ReviewCommentRef now live in `platform.rs` and are re-exported
// here via the `use crate::integrations::platform::*` below.

// ---------------------------------------------------------------------------
// CodePlatform trait implementation (Phase 0 — platform abstraction).
//
// The trait encapsulates auth so callers never handle tokens. GitHub mints an
// installation access token internally using `RepoRef.installation_id`, then
// delegates to the existing API methods. No behavior changes — GitHub works
// exactly as before.
// ---------------------------------------------------------------------------

use crate::integrations::platform::*;

impl GithubApp {
    // These helpers are used by the `impl CodePlatform` block below. Until the trait is wired
    // into the webhook/outbox/reconciler (Phases 2–3), `#[allow(dead_code)]` keeps clippy quiet.
    #![allow(dead_code)]
    /// The webhook secret from env, read once and cached by the caller (AppState).
    /// Used by `verify_webhook` for HMAC-SHA256 verification.
    fn webhook_secret() -> String {
        std::env::var("GITHUB_WEBHOOK_SECRET").unwrap_or_default()
    }

    /// Mint an installation token for the repo's installation_id. Internal helper for the
    /// trait impl so every method doesn't repeat the token-mint dance.
    async fn token_for(&self, repo: &RepoRef) -> anyhow::Result<String> {
        self.installation_token(repo.installation_id).await
    }
}

#[async_trait::async_trait]
impl CodePlatform for GithubApp {
    fn name(&self) -> &'static str {
        "github"
    }

    fn verify_webhook(&self, headers: &axum::http::HeaderMap, body: &[u8]) -> bool {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        type HmacSha256 = Hmac<Sha256>;

        let secret = Self::webhook_secret();
        if secret.is_empty() {
            return false; // fail-closed
        }

        let sig_header = match headers
            .get("x-hub-signature-256")
            .and_then(|v| v.to_str().ok())
        {
            Some(s) => s,
            None => return false,
        };
        let expected = match sig_header.strip_prefix("sha256=") {
            Some(hex) => hex,
            None => return false,
        };
        let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
            Ok(m) => m,
            Err(_) => return false,
        };
        mac.update(body);
        let computed = hex::encode(mac.finalize().into_bytes());

        // Constant-time comparison to avoid a timing oracle on the digest.
        use subtle::ConstantTimeEq;
        computed.as_bytes().ct_eq(expected.as_bytes()).into()
    }

    fn delivery_id(&self, headers: &axum::http::HeaderMap) -> Option<String> {
        headers
            .get("x-github-delivery")?
            .to_str()
            .ok()
            .map(|s| s.to_string())
    }

    fn event_type(&self, headers: &axum::http::HeaderMap) -> Option<String> {
        headers
            .get("x-github-event")?
            .to_str()
            .ok()
            .map(|s| s.to_string())
    }

    async fn list_changed_files(
        &self,
        repo: &RepoRef,
        pr_number: i64,
    ) -> anyhow::Result<Vec<ChangedFile>> {
        let (owner, name) = repo.owner_repo();
        let token = self.token_for(repo).await?;
        let files = self.list_pr_files(&token, owner, name, pr_number).await?;
        Ok(files
            .into_iter()
            .map(|f| ChangedFile {
                path: f.filename,
                patch: f.patch,
            })
            .collect())
    }

    async fn default_branch(&self, repo: &RepoRef) -> anyhow::Result<String> {
        let (owner, name) = repo.owner_repo();
        let token = self.token_for(repo).await?;
        self.repository_default_branch(&token, owner, name).await
    }

    async fn pr_shas(
        &self,
        repo: &RepoRef,
        pr_number: i64,
    ) -> anyhow::Result<(Option<String>, Option<String>)> {
        let (owner, name) = repo.owner_repo();
        let token = self.token_for(repo).await?;
        self.pull_request_shas(&token, owner, name, pr_number).await
    }

    async fn get_repo_file(
        &self,
        repo: &RepoRef,
        ref_: &str,
        path: &str,
    ) -> anyhow::Result<Option<String>> {
        let (owner, name) = repo.owner_repo();
        let token = self.token_for(repo).await?;
        self.repository_file_content(&token, owner, name, ref_, path)
            .await
    }

    async fn update_repo_file(
        &self,
        repo: &RepoRef,
        path: &str,
        mutate: Box<dyn FnOnce(Option<String>) -> String + Send>,
        message: &str,
    ) -> anyhow::Result<()> {
        let (owner, name) = repo.owner_repo();
        let token = self.token_for(repo).await?;
        self.update_repository_file_content(&token, owner, name, path, mutate, message)
            .await
    }

    async fn post_review(
        &self,
        repo: &RepoRef,
        review: &ReviewPost,
    ) -> anyhow::Result<PostedReview> {
        let (owner, name) = repo.owner_repo();
        let token = self.token_for(repo).await?;
        let comments: Vec<ReviewComment> = review
            .comments
            .iter()
            .map(|c| ReviewComment {
                path: c.path.clone(),
                line: c.line,
                side: c.side,
                start_line: c.start_line,
                start_side: c.start_side,
                body: c.body.clone(),
            })
            .collect();
        let posted = self
            .create_pr_review(
                &token,
                owner,
                name,
                review.pr_number,
                &review.body,
                &comments,
            )
            .await?;
        Ok(PostedReview {
            id: posted.id,
            html_url: posted.html_url,
        })
    }

    async fn post_comment(
        &self,
        repo: &RepoRef,
        issue_number: i64,
        body: &str,
        _noteable_type: Option<&str>,
    ) -> anyhow::Result<PostedComment> {
        let (owner, name) = repo.owner_repo();
        let token = self.token_for(repo).await?;
        let posted = self
            .create_issue_comment(&token, owner, name, issue_number, body)
            .await?;
        Ok(PostedComment {
            id: posted.id,
            html_url: posted.html_url,
        })
    }

    async fn add_reaction(
        &self,
        repo: &RepoRef,
        target: ReactionTarget,
        emoji: &str,
        _noteable_type: Option<&str>,
    ) -> anyhow::Result<()> {
        let (owner, name) = repo.owner_repo();
        let token = self.token_for(repo).await?;
        match target {
            ReactionTarget::Issue { number } => {
                self.add_reaction(&token, owner, name, number, emoji).await
            }
            ReactionTarget::Comment { comment_id, .. } => {
                self.add_comment_reaction(&token, owner, name, comment_id, emoji)
                    .await
            }
        }
    }

    async fn add_labels(
        &self,
        repo: &RepoRef,
        issue_number: i64,
        labels: &[String],
    ) -> anyhow::Result<()> {
        let (owner, name) = repo.owner_repo();
        let token = self.token_for(repo).await?;
        self.add_labels(&token, owner, name, issue_number, labels)
            .await
    }

    async fn start_check_run(
        &self,
        repo: &RepoRef,
        req: CheckRunStart<'_>,
    ) -> anyhow::Result<Option<i64>> {
        let (owner, name) = repo.owner_repo();
        let token = self.token_for(repo).await?;
        let id = self
            .create_check_run_in_progress(&token, owner, name, req.head_sha, req.details_url)
            .await?;
        Ok(Some(id))
    }

    async fn resolve_check_run(
        &self,
        repo: &RepoRef,
        req: CheckRunResolve<'_>,
    ) -> anyhow::Result<()> {
        let (owner, name) = repo.owner_repo();
        let token = self.token_for(repo).await?;
        let conclusion = req.conclusion.github_str();
        match req.external_id {
            Some(id) => {
                self.update_check_run_completed(
                    &token,
                    owner,
                    name,
                    id,
                    conclusion,
                    req.summary,
                    req.details_url,
                )
                .await
            }
            None => {
                self.create_check_run_completed(
                    &token,
                    owner,
                    name,
                    req.head_sha,
                    conclusion,
                    req.summary,
                    req.details_url,
                )
                .await
            }
        }
    }

    async fn list_review_comments(
        &self,
        repo: &RepoRef,
        pr_number: i64,
        review_id: i64,
    ) -> anyhow::Result<Vec<ReviewCommentRef>> {
        let (owner, name) = repo.owner_repo();
        let token = self.token_for(repo).await?;
        self.list_review_comments(&token, owner, name, pr_number, review_id)
            .await
    }

    async fn list_comment_reactions(
        &self,
        repo: &RepoRef,
        comment_id: i64,
        is_review_comment: bool,
        _iid: Option<i64>,
        _noteable_type: Option<&str>,
    ) -> anyhow::Result<Vec<Reaction>> {
        let (owner, name) = repo.owner_repo();
        let token = self.token_for(repo).await?;
        let raw = self
            .list_comment_reactions(&token, owner, name, comment_id, is_review_comment)
            .await?;
        Ok(raw
            .into_iter()
            .map(|(user_login, content)| Reaction {
                content,
                user_login,
            })
            .collect())
    }

    fn clone_url(&self, repo: &RepoRef) -> String {
        // Use the installation token as a bearer for HTTPS clone.
        // The token is minted on demand by the caller (the control plane provides it
        // to the agent-runner via the internal API). Here we just build the URL shape.
        format!("https://github.com/{}.git", repo.full_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use rsa::pkcs8::EncodePrivateKey as _;

    fn test_app(app_id: &str) -> GithubApp {
        let private = rsa::RsaPrivateKey::new(&mut rand_08::rngs::OsRng, 2048).expect("gen rsa");
        let pem = private
            .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
            .expect("pkcs8 pem");
        GithubApp {
            app_id: app_id.to_string(),
            key: EncodingKey::from_rsa_pem(pem.as_bytes()).expect("encoding key"),
            http: reqwest::Client::new(),
            token_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[test]
    fn app_jwt_carries_issuer_and_future_expiry() {
        let token = test_app("123456").app_jwt().expect("mint jwt");
        // header.payload.signature — decode the payload (no verification needed here).
        let payload_b64 = token.split('.').nth(1).expect("payload segment");
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload_b64)
            .expect("base64url payload");
        let claims: serde_json::Value = serde_json::from_slice(&payload).expect("json claims");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(claims["iss"], "123456");
        assert!(
            claims["exp"].as_u64().unwrap() > now,
            "exp must be in the future"
        );
        assert!(
            claims["iat"].as_u64().unwrap() <= now,
            "iat must be backdated"
        );
    }

    /// Regression (ADR-0071): a single-line `ReviewComment` (`start_line: None`, the only shape ever
    /// produced before this ADR) must serialize to byte-for-byte the same JSON as today — the range
    /// fields must be OMITTED entirely, not present as `null` (GitHub may reject an unexpected null
    /// field on this endpoint).
    #[test]
    fn review_comment_without_start_line_serializes_unchanged() {
        let comment = ReviewComment {
            path: "src/main.rs".to_string(),
            line: 42,
            side: "RIGHT",
            start_line: None,
            start_side: None,
            body: "a finding".to_string(),
        };
        let value = serde_json::to_value(&comment).expect("serializes");
        assert_eq!(
            value,
            serde_json::json!({
                "path": "src/main.rs",
                "line": 42,
                "side": "RIGHT",
                "body": "a finding",
            }),
            "no start_line/start_side keys at all when the finding is single-line: {value}"
        );
        assert!(
            value.get("start_line").is_none(),
            "start_line key must be absent, not null"
        );
        assert!(
            value.get("start_side").is_none(),
            "start_side key must be absent, not null"
        );
    }

    /// REGRESSION (live 422, 2026-08-02): every check-run payload must OMIT `details_url` entirely
    /// when there is no URL — never send it as `null`. GitHub answers an explicit null with
    /// `422 Invalid request. For 'properties/details_url', nil is not a string.`, which is what made
    /// every check-run intent dead-letter when this feature first shipped. Same contract, and same
    /// style of test, as `review_comment_without_start_line_serializes_unchanged` above.
    #[test]
    fn check_run_payloads_omit_details_url_when_absent() {
        for (label, payload) in [
            ("start", GithubApp::check_run_start_payload("abc123", None)),
            (
                "update",
                GithubApp::check_run_update_payload("success", "all good", None),
            ),
            (
                "create-completed",
                GithubApp::check_run_completed_payload("abc123", "success", "all good", None),
            ),
        ] {
            assert!(
                payload.get("details_url").is_none(),
                "{label} payload must omit details_url entirely, not send null: {payload}"
            );
        }
    }

    /// The counterpart: when a URL IS supplied it rides as a plain string.
    #[test]
    fn check_run_payloads_include_details_url_when_present() {
        let url = "https://example.test/run/1";
        for (label, payload) in [
            (
                "start",
                GithubApp::check_run_start_payload("abc123", Some(url)),
            ),
            (
                "update",
                GithubApp::check_run_update_payload("success", "all good", Some(url)),
            ),
            (
                "create-completed",
                GithubApp::check_run_completed_payload("abc123", "success", "all good", Some(url)),
            ),
        ] {
            assert_eq!(
                payload.get("details_url").and_then(|v| v.as_str()),
                Some(url),
                "{label} payload must carry details_url as a string"
            );
        }
    }

    /// The rest of each payload's shape, pinned so a future edit can't silently drop a required
    /// field while satisfying the details_url assertions above.
    #[test]
    fn check_run_payloads_carry_their_required_fields() {
        let start = GithubApp::check_run_start_payload("abc123", None);
        assert_eq!(start["name"], CHECK_RUN_NAME);
        assert_eq!(start["head_sha"], "abc123");
        assert_eq!(start["status"], "in_progress");

        let update = GithubApp::check_run_update_payload("neutral", "found 2 things", None);
        assert_eq!(update["status"], "completed");
        assert_eq!(update["conclusion"], "neutral");
        assert_eq!(update["output"]["summary"], "found 2 things");
        assert!(
            update.get("head_sha").is_none(),
            "a PATCH addresses an existing run by id; head_sha would be meaningless"
        );

        let completed =
            GithubApp::check_run_completed_payload("abc123", "failure", "it broke", None);
        assert_eq!(completed["name"], CHECK_RUN_NAME);
        assert_eq!(completed["head_sha"], "abc123");
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["conclusion"], "failure");
        assert_eq!(completed["output"]["summary"], "it broke");
    }

    /// A ranged `ReviewComment` (ADR-0071) posts `start_line` + `start_side: RIGHT` alongside the
    /// existing `line` + `side: RIGHT`.
    #[test]
    fn review_comment_with_start_line_serializes_range_fields() {
        let comment = ReviewComment {
            path: "src/main.rs".to_string(),
            line: 42,
            side: "RIGHT",
            start_line: Some(40),
            start_side: Some("RIGHT"),
            body: "a ranged finding".to_string(),
        };
        let value = serde_json::to_value(&comment).expect("serializes");
        assert_eq!(
            value,
            serde_json::json!({
                "path": "src/main.rs",
                "line": 42,
                "side": "RIGHT",
                "start_line": 40,
                "start_side": "RIGHT",
                "body": "a ranged finding",
            })
        );
    }
}
