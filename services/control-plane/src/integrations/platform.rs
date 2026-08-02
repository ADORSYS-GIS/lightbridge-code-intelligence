//! Platform abstraction: a single trait that GitHub and GitLab both implement.
//!
//! The control plane talks to a code-hosting platform (GitHub, GitLab) through this trait. Each
//! implementation encapsulates its own auth model (GitHub App installation tokens vs a static
//! GitLab access token) and API shape, so the webhook handler, outbox, and reconciler stay
//! platform-agnostic.
//!
//! ADR-0072/ADR-0108: fully wired in. `http::webhook` looks up `state.platforms` for GitHub
//! webhook-signature verification and the `@mention` re-review PR-SHA fetch; `db::outbox` and
//! `queue::reconciler` dispatch outbound posts/reactions through it per row; `http::internal`
//! dispatches per-task the same way.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Which code-hosting platform a repository lives on.
///
/// Stored as a `TEXT` column in the database (values: `"github"`, `"gitlab"`, `"bitbucket"`).
/// Existing rows default to `"github"` via the Phase 1 migration. No `CHECK` constraint guards
/// this column (verified against `services/control-plane/migrations/`), so adding `Bitbucket` here
/// needs no migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
pub enum Platform {
    GitHub,
    GitLab,
    Bitbucket,
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Platform::GitHub => write!(f, "github"),
            Platform::GitLab => write!(f, "gitlab"),
            Platform::Bitbucket => write!(f, "bitbucket"),
        }
    }
}

impl std::str::FromStr for Platform {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "github" => Ok(Platform::GitHub),
            "gitlab" => Ok(Platform::GitLab),
            "bitbucket" => Ok(Platform::Bitbucket),
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
/// `full_name` is `"owner/repo"` on GitHub, `"namespace/path"` on GitLab, or `"workspace/repo_slug"`
/// on Bitbucket — the same `"/"`-split shape. `installation_id` is the GitHub App installation ID on
/// GitHub, the GitLab project ID on GitLab (GitLab has no installation concept; the project ID
/// serves as the scope for API calls), or a [`stable_id_from_key`]-derived identity on Bitbucket
/// (Bitbucket has no native numeric project id like GitLab's `project.id`).
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

/// Derive a stable, deterministic `i64` identity from an opaque string key (e.g. a Bitbucket
/// `"workspace/repo_slug"` pair), for platforms with no native numeric project id like GitLab's
/// `project.id`. Used as `RepoRef.installation_id` / `tasks.installation_id` for Bitbucket, so this
/// must stay stable forever once a repo is configured — hence SHA-256 rather than
/// `std::collections::hash_map::DefaultHasher`, whose algorithm the standard library explicitly does
/// NOT guarantee to stay the same across Rust versions (a toolchain upgrade could silently reshuffle
/// every configured Bitbucket repo's identity).
pub fn stable_id_from_key(key: &str) -> i64 {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(key.as_bytes());
    i64::from_be_bytes(
        digest[0..8]
            .try_into()
            .expect("sha256 digest is >= 8 bytes"),
    )
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
    Comment { comment_id: i64, iid: Option<i64> },
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

/// A fixed, stable name for the check/status a review run posts — the SAME value used at start and
/// resolve, so GitHub updates one check run (by id) and GitLab/Bitbucket's upsert-by-(sha, name/key)
/// semantics land on the same slot rather than creating a new one each time.
pub const CHECK_RUN_NAME: &str = "Lightbridge Review";

/// The resolved outcome of a review run, mapped onto each platform's status vocabulary. Lives here
/// (not in `db` or `outbox`) because it's the platform-facing contract, exactly like `ReviewPost`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckConclusion {
    Success,
    Failure,
    Neutral,
    Cancelled,
    TimedOut,
}

impl CheckConclusion {
    /// GitHub's `conclusion` vocabulary — a 1:1 mapping (GitHub is the only platform with all five
    /// values natively).
    pub fn github_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Neutral => "neutral",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
        }
    }
}

/// Parameters to open an in-progress check/status on a commit.
#[derive(Debug, Clone)]
pub struct CheckRunStart<'a> {
    pub head_sha: &'a str,
    pub details_url: Option<&'a str>,
}

/// Parameters to resolve a previously-opened check/status.
#[derive(Debug, Clone)]
pub struct CheckRunResolve<'a> {
    pub head_sha: &'a str,
    /// GitHub's check-run id from the `start_check_run` call, `None` when no id was ever recorded (the
    /// start intent hasn't posted yet, dead-lettered on a 403, or predates this feature). GitLab and
    /// Bitbucket implementations ignore this — their status APIs upsert by sha + a fixed name/key, no
    /// id needed — GitHub uses it to choose PATCH-by-id vs. the self-healing create-already-completed
    /// fallback.
    pub external_id: Option<i64>,
    pub conclusion: CheckConclusion,
    /// One-line headline (e.g. `"3 findings"`). GitHub shows it above the summary; GitLab's
    /// commit-status `description` and Bitbucket's build-status `description` are short single-line
    /// fields and take THIS rather than [`Self::summary`].
    pub title: &'a str,
    /// Markdown body describing the outcome. GitHub renders it; the other platforms have nowhere to
    /// put it (see [`Self::title`]).
    pub summary: &'a str,
    /// Permalink to the posted review, when there is one — surfaced as the check's "Details" link.
    pub details_url: Option<&'a str>,
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

    /// Fetch a single file's raw text content at `ref_`, or `None` when the file doesn't exist at that
    /// ref (a 404-equivalent — not an error). Used to resolve `.lightbridge-code-review.jsonc`
    /// (ADR-0030/ADR-0103's `preset`) at webhook/task-creation time, before any clone exists — a single
    /// small file fetch, not a full checkout.
    async fn get_repo_file(
        &self,
        repo: &RepoRef,
        ref_: &str,
        path: &str,
    ) -> anyhow::Result<Option<String>>;

    /// Create or update a single file on the repository's **default branch** (ADR-0109) — a direct
    /// commit, not a PR. Takes a `mutate` closure, not the final content: the implementation fetches
    /// the file's current content **and** its concurrency token (GitHub's `sha`) as ONE read, calls
    /// `mutate` on that exact snapshot, then writes the result back guarded by that same token. This
    /// is load-bearing, not stylistic — a caller-side read-modify-write (fetch via
    /// [`Self::get_repo_file`], compute new content, call this with the final string) opens a TOCTOU
    /// gap: a concurrent edit landing between the caller's read and this method's own internal
    /// conflict check is silently discarded rather than surfaced, because the two reads observe
    /// different snapshots even though the write's conflict check only compares against the second.
    /// `Box<dyn FnOnce>` (not `impl FnOnce`) is required for dyn-safety (this trait is used as
    /// `dyn CodePlatform`). The only caller in this codebase passes `path =
    /// ".lightbridge-code-review.jsonc"` (story #500's preset-selector endpoint) — this is a
    /// single-purpose escape hatch for that one file, not a general write tool; widening what path a
    /// caller may pass is a decision that needs its own review, per ADR-0109's own Consequences
    /// section.
    async fn update_repo_file(
        &self,
        repo: &RepoRef,
        path: &str,
        mutate: Box<dyn FnOnce(Option<String>) -> String + Send>,
        message: &str,
    ) -> anyhow::Result<()>;

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

    /// Open an in-progress check/status on a commit (a PR/MR's head SHA). Returns the platform's id
    /// for the created check run when the platform has one to correlate later (GitHub); `None` when
    /// the platform's status API is upsert-by-sha with no id (GitLab, Bitbucket) — the caller persists
    /// a `Some` onto `tasks.check_run_external_id` for `resolve_check_run` to read back.
    async fn start_check_run(
        &self,
        repo: &RepoRef,
        req: CheckRunStart<'_>,
    ) -> anyhow::Result<Option<i64>>;

    /// Resolve a previously-started check/status to its final outcome.
    async fn resolve_check_run(
        &self,
        repo: &RepoRef,
        req: CheckRunResolve<'_>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_display_and_parse_roundtrip() {
        for (platform, text) in [
            (Platform::GitHub, "github"),
            (Platform::GitLab, "gitlab"),
            (Platform::Bitbucket, "bitbucket"),
        ] {
            assert_eq!(platform.to_string(), text);
            assert_eq!(Platform::parse(text), Some(platform));
            assert_eq!(Platform::parse(&text.to_ascii_uppercase()), Some(platform));
        }
        assert_eq!(Platform::parse("unknown"), None);
    }

    #[test]
    fn check_conclusion_github_str_covers_every_variant() {
        for (conclusion, text) in [
            (CheckConclusion::Success, "success"),
            (CheckConclusion::Failure, "failure"),
            (CheckConclusion::Neutral, "neutral"),
            (CheckConclusion::Cancelled, "cancelled"),
            (CheckConclusion::TimedOut, "timed_out"),
        ] {
            assert_eq!(conclusion.github_str(), text);
        }
    }

    #[test]
    fn stable_id_from_key_is_deterministic_and_distinguishes_keys() {
        let a = stable_id_from_key("myteam/my-repo");
        let b = stable_id_from_key("myteam/my-repo");
        let c = stable_id_from_key("myteam/other-repo");
        assert_eq!(a, b, "same key must always derive the same id");
        assert_ne!(
            a, c,
            "different keys must (in practice) derive different ids"
        );
    }
}
