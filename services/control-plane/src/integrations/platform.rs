//! Platform abstraction: a single trait that GitHub and GitLab both implement.
//!
//! The control plane talks to a code-hosting platform (GitHub today, GitLab tomorrow) through this
//! trait. Each implementation encapsulates its own auth model (GitHub App installation tokens vs
//! a static GitLab access token) and API shape, so the webhook handler, outbox, and reconciler
//! stay platform-agnostic.
//!
//! Phase 0 (this file + `impl CodePlatform for GithubApp`): the trait is introduced and GitHub is
//! refactored behind it. No behavior changes — GitHub works exactly as before.
//!
//! The types and trait here are not yet wired into the webhook handler, outbox, or reconciler —
//! that happens in Phases 2–3. Until then, `#[allow(dead_code)]` suppresses the "never used"
//! warnings that `-D warnings` would otherwise turn into errors.

#![allow(dead_code)]

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Which code-hosting platform a repository lives on.
///
/// Stored as a `TEXT` column in the database (values: `"github"`, `"gitlab"`). Existing rows default
/// to `"github"` via the Phase 1 migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
pub enum Platform {
    GitHub,
    GitLab,
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Platform::GitHub => write!(f, "github"),
            Platform::GitLab => write!(f, "gitlab"),
        }
    }
}

impl std::str::FromStr for Platform {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "github" => Ok(Platform::GitHub),
            "gitlab" => Ok(Platform::GitLab),
            other => Err(format!("unknown platform: {other}")),
        }
    }
}

impl Platform {
    /// Parse from a string, tolerating case. Returns `None` on unknown values (rather than `Err`)
    /// so callers can use it in `filter_map` / `Option` chains without a `Result`.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

/// A reference to a repository, platform-agnostic.
///
/// `full_name` is `"owner/repo"` on GitHub or `"namespace/path"` on GitLab — the same `"/"`-split
/// shape. `installation_id` is the GitHub App installation ID on GitHub, or the GitLab project ID
/// on GitLab (GitLab has no installation concept; the project ID serves as the scope for API calls).
#[derive(Debug, Clone)]
pub struct RepoRef {
    pub platform: Platform,
    pub full_name: String,
    pub platform_repo_id: i64,
    pub installation_id: i64,
}

impl RepoRef {
    /// Split `full_name` into `(owner, repo)` / `(namespace, path)`. Uses
    /// `rsplit_once` so the repo name is always the last segment — correct for
    /// GitLab nested subgroups (`group/sub/repo` → `("group/sub", "repo")`).
    pub fn owner_repo(&self) -> (&str, &str) {
        match self.full_name.rsplit_once('/') {
            Some((o, r)) => (o, r),
            None => (&self.full_name, ""),
        }
    }
}

/// A changed file in a PR/MR diff. `patch` is the unified-diff text (absent for binary/huge files).
#[derive(Debug, Clone)]
pub struct ChangedFile {
    pub path: String,
    pub patch: Option<String>,
}

/// An inline review comment to post.
#[derive(Debug, Clone)]
pub struct InlineComment {
    pub path: String,
    pub line: u32,
    /// Which side of the diff the comment anchors to (`"RIGHT"` for the PR head, `"LEFT"` for the
    /// base). GitHub uses this in its review API; GitLab infers it from the position object.
    pub side: &'static str,
    /// First line of a validated multi-line range (ADR-0071), or `None` for a single-line comment.
    pub start_line: Option<u32>,
    /// Side for the start of the range (`"RIGHT"` when `start_line` is set). GitHub requires this
    /// alongside `start_line`; GitLab uses `line_range` in the position object instead.
    pub start_side: Option<&'static str>,
    pub body: String,
}

/// A review to post (body + inline comments + optional labels).
#[derive(Debug, Clone)]
pub struct ReviewPost {
    pub pr_number: i64,
    pub body: String,
    pub comments: Vec<InlineComment>,
    pub labels: Vec<String>,
}

/// Where a reaction/award emoji is attached.
#[derive(Debug, Clone)]
pub enum ReactionTarget {
    /// React to the PR/MR/issue body (the "description").
    Issue { number: i64 },
    /// React to a specific comment/note.
    ///
    /// `iid` is the parent MR/issue iid — required by GitLab, whose notes are scoped to their
    /// parent (there is no global `/projects/{id}/notes/{note_id}` endpoint). GitHub ignores it
    /// (GitHub comment IDs are globally addressable). `None` for legacy outbox rows that predate
    /// this field; GitLab will fail with a clear error in that case.
    Comment {
        comment_id: i64,
        iid: Option<i64>,
    },
}

/// A reaction found on a comment (for feedback polling).
#[derive(Debug, Clone)]
pub struct Reaction {
    pub content: String,
    pub user_login: String,
}

/// The result of posting a review: the platform's review/comment ID (for correlation) and an optional
/// HTML permalink.
#[derive(Debug, Default)]
pub struct PostedReview {
    pub id: Option<i64>,
    pub html_url: Option<String>,
}

/// The result of posting a comment: the platform's comment ID (for feedback polling) and an optional
/// HTML permalink.
#[derive(Debug, Default)]
pub struct PostedComment {
    pub id: Option<i64>,
    pub html_url: Option<String>,
}

/// An inline review comment the platform created, fetched after posting so the feedback poller
/// knows its id. `path`/`line` correlate it back to the finding.
#[derive(Debug, Clone)]
pub struct ReviewCommentRef {
    pub id: i64,
    pub path: Option<String>,
    pub line: Option<i64>,
}

/// The platform trait. GitHub (`GithubApp`) and GitLab (`GitlabClient`, future) both implement this.
///
/// The trait encapsulates auth: GitHub mints an installation token internally using
/// `RepoRef.installation_id`; GitLab uses a static access token. Callers never handle tokens.
#[async_trait]
pub trait CodePlatform: Send + Sync {
    /// Human-readable platform name for logs (e.g. `"github"`, `"gitlab"`).
    fn name(&self) -> &'static str;

    // --- Webhook verification ---

    /// Verify a webhook signature. Returns `true` if valid.
    /// GitHub: HMAC-SHA256 over the body (`X-Hub-Signature-256`).
    /// GitLab: plain token comparison (`X-Gitlab-Token`).
    fn verify_webhook(&self, headers: &axum::http::HeaderMap, body: &[u8]) -> bool;

    /// Extract the delivery (dedup) ID from webhook headers.
    /// GitHub: `X-GitHub-Delivery`. GitLab: `X-Gitlab-Event-UUID`.
    fn delivery_id(&self, headers: &axum::http::HeaderMap) -> Option<String>;

    /// Extract the event type string from webhook headers.
    /// GitHub: `X-GitHub-Event`. GitLab: `X-Gitlab-Event`.
    fn event_type(&self, headers: &axum::http::HeaderMap) -> Option<String>;

    // --- PR/MR reads ---

    /// List changed files in a PR/MR with their unified-diff patches.
    async fn list_changed_files(
        &self,
        repo: &RepoRef,
        pr_number: i64,
    ) -> anyhow::Result<Vec<ChangedFile>>;

    /// Get the default branch of a repository.
    async fn default_branch(&self, repo: &RepoRef) -> anyhow::Result<String>;

    /// Get the `(base_sha, head_sha)` of a PR/MR.
    async fn pr_shas(
        &self,
        repo: &RepoRef,
        pr_number: i64,
    ) -> anyhow::Result<(Option<String>, Option<String>)>;

    // --- Posting ---

    /// Post a review (inline comments + body). Returns the platform review ID + optional HTML URL.
    async fn post_review(
        &self,
        repo: &RepoRef,
        review: &ReviewPost,
    ) -> anyhow::Result<PostedReview>;

    /// Post a comment on an issue/PR/MR. Returns the platform comment ID + optional HTML URL.
    /// `noteable_type` carries the task's `target_type` (`"pull_request"` or `"issue"`) so GitLab
    /// can route to MR notes vs issue notes without probing (MRs and issues share iid sequences).
    async fn post_comment(
        &self,
        repo: &RepoRef,
        issue_number: i64,
        body: &str,
        noteable_type: Option<&str>,
        iid: Option<i64>,
    ) -> anyhow::Result<PostedComment>;

    /// Add a reaction (emoji) to an issue/PR/MR body or to a comment.
    /// `noteable_type` carries the task's `target_type` for GitLab routing (see `post_comment`).
    async fn add_reaction(
        &self,
        repo: &RepoRef,
        target: ReactionTarget,
        emoji: &str,
        noteable_type: Option<&str>,
    ) -> anyhow::Result<()>;

    /// Add labels to an issue/PR/MR.
    async fn add_labels(
        &self,
        repo: &RepoRef,
        issue_number: i64,
        labels: &[String],
    ) -> anyhow::Result<()>;

    // --- Feedback polling ---

    /// List the inline comments of a posted review (for feedback correlation).
    async fn list_review_comments(
        &self,
        repo: &RepoRef,
        pr_number: i64,
        review_id: i64,
    ) -> anyhow::Result<Vec<ReviewCommentRef>>;

    /// List reactions on a comment (for 👍/👎 feedback polling, ADR-0035).
    /// `is_review_comment` distinguishes inline PR review comments from plain issue comments
    /// (GitHub uses different endpoints; GitLab uses award emoji on notes).
    /// `iid` is the parent MR/issue iid — required by GitLab (notes are scoped to their parent;
    /// there is no global `/projects/{id}/notes/{note_id}` endpoint). GitHub ignores it.
    /// `noteable_type` carries the task's `target_type` (`"pull_request"` or `"issue"`) for GitLab
    /// routing (MR notes vs issue notes), matching `post_comment` / `add_reaction`.
    async fn list_comment_reactions(
        &self,
        repo: &RepoRef,
        comment_id: i64,
        is_review_comment: bool,
        iid: Option<i64>,
        noteable_type: Option<&str>,
    ) -> anyhow::Result<Vec<Reaction>>;

    // --- Clone ---

    /// Build the clone URL with credentials for the agent-runner.
    fn clone_url(&self, repo: &RepoRef) -> String;
}
