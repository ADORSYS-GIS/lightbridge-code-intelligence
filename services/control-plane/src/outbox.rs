//! ADR-0059 — the GitHub-egress outbox, producer side. Every outbound GitHub *content* write is shaped
//! here and handed to the queue via an `enqueue_*` helper; the reconciler ([`crate::queue::reconciler`])
//! is the sole consumer that actually posts. Payloads are **fully shaped at produce time** — the diff
//! fetch + validation + rendering happen in the producer and are baked into the row — so the reconciler
//! never parses a diff, it just ships bytes. Every enqueue is idempotent on its `dedup_key`.

use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::integrations::platform::Platform;

/// Who to post as and where — shared by every intent.
pub struct Target<'a> {
    /// `Some` for review/reply/failure_notice (the posted ids are recorded back against the task for the
    /// ADR-0035 feedback join); may be `None` for a bare reaction.
    pub task_id: Option<Uuid>,
    pub platform: Platform,
    pub installation_id: i64,
    pub owner: &'a str,
    pub repo: &'a str,
}

impl Target<'_> {
    /// Stable per-task prefix for `dedup_key`s; falls back to the repo+issue when there's no task.
    fn key_prefix(&self, issue: i64) -> String {
        match self.task_id {
            Some(id) => id.to_string(),
            None => format!("{}/{}#{issue}", self.owner, self.repo),
        }
    }
}

/// A fully-rendered inline comment in a `review` intent (owned mirror of `github::ReviewComment`).
#[derive(Debug, Serialize, Deserialize)]
pub struct ReviewCommentPayload {
    pub path: String,
    pub line: u32,
    /// First line of a validated range (ADR-0071), carried through the outbox row so the reconciler can
    /// post it as `start_line`/`start_side` alongside `line`/`side`. `default` so an outbox row enqueued
    /// before this ADR shipped (in flight across a deploy) still deserializes as a single-line comment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    pub body: String,
}

/// The `review` intent: everything the reconciler needs to post the grouped review **and** its success
/// side-effects (persist the copy, fetch inline ids, apply outcome labels) without re-shaping anything.
/// The verdict reaction (👎, ADR-0068) is enqueued as a separate `reaction` intent at finalize, not here
/// — a review intent is only ever produced when there ARE findings.
#[derive(Debug, Serialize, Deserialize)]
pub struct ReviewPayload {
    pub pr: i64,
    pub body: String,
    pub summary: String,
    pub comments: Vec<ReviewCommentPayload>,
    pub inline_n: i32,
    pub deferred_n: i32,
    pub out_of_scope_n: i32,
    pub findings_json: serde_json::Value,
    /// Outcome-label flags computed at produce time; the reconciler maps them to the configured label
    /// names (so `add_review_labels` rides the outbox, not a second serve-side writer — #218 review).
    pub label_findings: bool,
    pub label_error: bool,
}

/// Enqueue the grouped PR review — one per task (`<task>:review`). Propagates a serialization failure
/// instead of enqueuing a `Null` payload that would silently dead-letter (#219 review) — the caller
/// returns 500 and the runner re-finalizes (idempotent on the dedup_key).
pub async fn enqueue_review(
    pool: &PgPool,
    t: &Target<'_>,
    payload: &ReviewPayload,
) -> anyhow::Result<bool> {
    let key = format!("{}:review", t.key_prefix(payload.pr));
    let value = serde_json::to_value(payload)?;
    let inserted = crate::db::enqueue_outbox_post(
        pool,
        t.platform,
        t.task_id,
        t.installation_id,
        t.owner,
        t.repo,
        "review",
        &value,
        &key,
    )
    .await?;
    Ok(inserted)
}

/// Enqueue a consolidated reply / `ask` answer (issue comment) — one per task (`<task>:reply`).
pub async fn enqueue_reply(
    pool: &PgPool,
    t: &Target<'_>,
    issue: i64,
    body: &str,
    target_type: &str,
) -> anyhow::Result<bool> {
    let key = format!("{}:reply", t.key_prefix(issue));
    let value = json!({ "issue": issue, "body": body, "target_type": target_type });
    let inserted = crate::db::enqueue_outbox_post(
        pool,
        t.platform,
        t.task_id,
        t.installation_id,
        t.owner,
        t.repo,
        "reply",
        &value,
        &key,
    )
    .await?;
    Ok(inserted)
}

/// Enqueue a lifecycle reaction (👀 `eyes` / 😕 `confused`, ADR-0068) — keyed by content so the distinct
/// lifecycle reactions don't collide (`<task>:reaction:<content>`). When `comment_id` is `Some`, the
/// reconciler reacts on that ISSUE COMMENT (the `@mention` that triggered the task) rather than the
/// PR/issue body — so an @mention review's acknowledgment lands on the request. The 👍/👎 verdict pair
/// goes through [`enqueue_verdict_reaction`] instead — a content-scoped key would let a verdict flip
/// across finalize attempts stack BOTH reactions.
pub async fn enqueue_reaction(
    pool: &PgPool,
    t: &Target<'_>,
    issue: i64,
    content: &str,
    comment_id: Option<i64>,
    target_type: &str,
) -> anyhow::Result<bool> {
    let key = format!("{}:reaction:{content}", t.key_prefix(issue));
    let value = reaction_payload(issue, content, comment_id, target_type);
    let inserted = crate::db::enqueue_outbox_post(
        pool,
        t.platform,
        t.task_id,
        t.installation_id,
        t.owner,
        t.repo,
        "reaction",
        &value,
        &key,
    )
    .await?;
    Ok(inserted)
}

/// Enqueue the ADR-0068 **verdict** reaction (👍 `+1` clean / 👎 `-1` findings) under ONE shared dedup
/// key — `<task>:reaction:verdict` — with the content only in the payload. A task gets exactly one
/// verdict: if a re-finalize (crash-then-requeue, or a stray retry against a now-empty buffer) computes a
/// *different* verdict, the `ON CONFLICT DO NOTHING` makes the first one win instead of leaving both 👍
/// and 👎 on the trigger.
pub async fn enqueue_verdict_reaction(
    pool: &PgPool,
    t: &Target<'_>,
    issue: i64,
    content: &str,
    comment_id: Option<i64>,
    target_type: &str,
) -> anyhow::Result<bool> {
    let key = format!("{}:reaction:verdict", t.key_prefix(issue));
    let value = reaction_payload(issue, content, comment_id, target_type);
    let inserted = crate::db::enqueue_outbox_post(
        pool,
        t.platform,
        t.task_id,
        t.installation_id,
        t.owner,
        t.repo,
        "reaction",
        &value,
        &key,
    )
    .await?;
    Ok(inserted)
}

/// The `reaction` intent payload (ADR-0068). `comment_id` is included **only when `Some`**, so the
/// reconciler routes on its presence: present → react on that issue comment (the `@mention` trigger);
/// absent → react on the PR/issue body. Pure, so the shape is unit-tested without a DB.
fn reaction_payload(
    issue: i64,
    content: &str,
    comment_id: Option<i64>,
    target_type: &str,
) -> serde_json::Value {
    match comment_id {
        Some(cid) => {
            json!({ "issue": issue, "content": content, "comment_id": cid, "target_type": target_type })
        }
        None => json!({ "issue": issue, "content": content, "target_type": target_type }),
    }
}

/// Enqueue the ADR-0056 failure notice — one per task (`<task>:failure_notice`). The reconciler re-checks
/// `has_posted_to_github` before posting, so a finalize-then-fail never double-posts.
pub async fn enqueue_failure_notice(
    pool: &PgPool,
    t: &Target<'_>,
    issue: i64,
    target_type: &str,
) -> anyhow::Result<bool> {
    let key = format!("{}:failure_notice", t.key_prefix(issue));
    let value = json!({ "issue": issue, "body": crate::review::render_failure_notice(), "target_type": target_type });
    let inserted = crate::db::enqueue_outbox_post(
        pool,
        t.platform,
        t.task_id,
        t.installation_id,
        t.owner,
        t.repo,
        "failure_notice",
        &value,
        &key,
    )
    .await?;
    Ok(inserted)
}

/// The `check_run_start` intent: open an in-progress check/status on a PR/MR's head SHA (new feature —
/// see the module doc). One per task (`<task>:check_run:start`).
#[derive(Debug, Serialize, Deserialize)]
pub struct CheckRunStartPayload {
    pub pr: i64,
    pub head_sha: String,
}

/// Enqueue the "check in progress" signal.
pub async fn enqueue_check_run_start(
    pool: &PgPool,
    t: &Target<'_>,
    pr: i64,
    head_sha: &str,
) -> anyhow::Result<bool> {
    let key = format!("{}:check_run:start", t.key_prefix(pr));
    let value = serde_json::to_value(CheckRunStartPayload {
        pr,
        head_sha: head_sha.to_string(),
    })?;
    let inserted = crate::db::enqueue_outbox_post(
        pool,
        t.platform,
        t.task_id,
        t.installation_id,
        t.owner,
        t.repo,
        "check_run_start",
        &value,
        &key,
    )
    .await?;
    Ok(inserted)
}

/// The `check_run_resolve` intent: resolve a previously-opened check/status to its final outcome. One
/// per task (`<task>:check_run:resolve`) — `ON CONFLICT DO NOTHING` on the shared dedup key means the
/// FIRST resolve to reach the outbox wins if two terminal paths ever somehow raced for the same task.
#[derive(Debug, Serialize, Deserialize)]
pub struct CheckRunResolvePayload {
    pub pr: i64,
    pub head_sha: String,
    pub conclusion: crate::integrations::platform::CheckConclusion,
    /// One-line headline (e.g. `"3 findings"`). `default` so a row enqueued before titles existed
    /// (in flight across a deploy) still deserializes — the platform impls fall back to the check
    /// name for an empty title.
    #[serde(default)]
    pub title: String,
    pub summary: String,
}

/// Enqueue the check resolution.
pub async fn enqueue_check_run_resolve(
    pool: &PgPool,
    t: &Target<'_>,
    pr: i64,
    head_sha: &str,
    conclusion: crate::integrations::platform::CheckConclusion,
    title: &str,
    summary: &str,
) -> anyhow::Result<bool> {
    let key = format!("{}:check_run:resolve", t.key_prefix(pr));
    let value = serde_json::to_value(CheckRunResolvePayload {
        pr,
        head_sha: head_sha.to_string(),
        conclusion,
        title: title.to_string(),
        summary: summary.to_string(),
    })?;
    let inserted = crate::db::enqueue_outbox_post(
        pool,
        t.platform,
        t.task_id,
        t.installation_id,
        t.owner,
        t.repo,
        "check_run_resolve",
        &value,
        &key,
    )
    .await?;
    Ok(inserted)
}

/// The `pr_open` intent (ADR-0088): everything the egress plane needs to push a branch + open a PR,
/// with the branch itself **offloaded** — `content_hash` points at the `pr_open_blob` row holding the
/// `git format-patch` bytes (the offload rule; the intent on the wire carries the key + hash, not the
/// bytes). The egress plane rehydrates by hash, verifies, pushes the branch, and opens the PR. It never
/// auto-merges — this proposes.
#[derive(Debug, Serialize, Deserialize)]
pub struct PrOpenPayload {
    /// The local branch name the sandbox committed to; the egress plane pushes it under this name.
    pub branch: String,
    /// The base ref the PR targets; `None` → the repo default branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    pub title: String,
    pub body: String,
    /// The content hash keying the offloaded branch patch in `pr_open_blob`.
    pub content_hash: String,
}

/// The `pr_open` dedup key — `(task_id, run_epoch)` (ADR-0088 O5). Pure, so the idempotency contract is
/// unit-tested without a DB: a replay of the terminal `propose_pr` step recomputes the SAME key, so the
/// outbox `ON CONFLICT DO NOTHING` opens exactly one PR. `run_epoch` is the ADR-0076 run-identity
/// discriminator, resolved control-plane-side (the agent never knows it — trust boundary).
#[must_use]
pub fn pr_open_dedup_key(task_id: Uuid, run_epoch: i32) -> String {
    format!("{task_id}:{run_epoch}:pr_open")
}

/// Enqueue the open-mode PR-open intent — dedup-keyed by `(task_id, run_epoch)` so a replayed/at-least-
/// once `propose_pr` never opens a duplicate PR (ADR-0088 O5). Returns whether a NEW row was inserted
/// (`false` = an intent with this key already existed → the existing PR proposal stands). Mirrors
/// [`enqueue_review`]: propagates a serialization failure rather than enqueuing a `Null` payload.
pub async fn enqueue_pr_open(
    pool: &PgPool,
    t: &Target<'_>,
    task_id: Uuid,
    run_epoch: i32,
    payload: &PrOpenPayload,
) -> anyhow::Result<bool> {
    let key = pr_open_dedup_key(task_id, run_epoch);
    let value = serde_json::to_value(payload)?;
    let inserted = crate::db::enqueue_outbox_post(
        pool,
        t.platform,
        Some(task_id),
        t.installation_id,
        t.owner,
        t.repo,
        "pr_open",
        &value,
        &key,
    )
    .await?;
    Ok(inserted)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ADR-0088 O5: the pr_open dedup key is a pure function of `(task_id, run_epoch)`, so a replayed
    // propose_pr recomputes the identical key and the outbox `ON CONFLICT` opens exactly one PR.
    #[test]
    fn pr_open_dedup_key_is_stable_per_task_and_run_epoch() {
        let task = Uuid::from_u128(1);
        assert_eq!(pr_open_dedup_key(task, 0), pr_open_dedup_key(task, 0));
        assert_ne!(pr_open_dedup_key(task, 0), pr_open_dedup_key(task, 1));
        assert!(pr_open_dedup_key(task, 3).ends_with(":3:pr_open"));
    }

    // The check-run start/resolve dedup keys are stable per task and distinct from each other and from
    // every other intent kind, so a re-dispatch/re-finalize never double-enqueues either signal.
    #[test]
    fn check_run_dedup_keys_are_stable_per_task_and_distinct() {
        let t = Target {
            task_id: Some(Uuid::from_u128(1)),
            platform: Platform::GitHub,
            installation_id: 1,
            owner: "octo",
            repo: "repo",
        };
        assert_eq!(t.key_prefix(7), t.key_prefix(7), "stable for the same task");
        assert!(format!("{}:check_run:start", t.key_prefix(7)).ends_with(":check_run:start"));
        assert!(format!("{}:check_run:resolve", t.key_prefix(7)).ends_with(":check_run:resolve"));
        assert_ne!(
            format!("{}:check_run:start", t.key_prefix(7)),
            format!("{}:check_run:resolve", t.key_prefix(7)),
            "start and resolve must not share a dedup key"
        );
    }

    // ADR-0068: the reaction payload carries `comment_id` ONLY when the task was @mention-triggered, so
    // the reconciler can route on its presence (comment vs PR/issue body). This is the round-trip the
    // reconciler's `deliver` reads back.
    #[test]
    fn reaction_payload_includes_comment_id_only_when_present() {
        // Mention-triggered: comment_id present → the reconciler reacts on the comment.
        let with = reaction_payload(7, "eyes", Some(4242), "pull_request");
        assert_eq!(with["issue"], 7);
        assert_eq!(with["content"], "eyes");
        assert_eq!(with["comment_id"], 4242);
        assert_eq!(with["target_type"], "pull_request");

        // Auto review: no trigger comment → the key is absent (not null), so `get("comment_id")` → None
        // and the reconciler falls back to the PR/issue body.
        let without = reaction_payload(7, "+1", None, "issue");
        assert_eq!(without["issue"], 7);
        assert_eq!(without["content"], "+1");
        assert_eq!(without["target_type"], "issue");
        assert!(
            without.get("comment_id").is_none(),
            "comment_id must be absent, not null, so the reconciler routes to the issue body"
        );
    }
}
