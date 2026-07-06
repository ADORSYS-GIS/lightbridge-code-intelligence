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
    crate::db::enqueue_outbox_post(
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
    .await
    .map_err(Into::into)
}

/// Enqueue a consolidated reply / `ask` answer (issue comment) — one per task (`<task>:reply`).
pub async fn enqueue_reply(
    pool: &PgPool,
    t: &Target<'_>,
    issue: i64,
    body: &str,
) -> Result<bool, sqlx::Error> {
    let key = format!("{}:reply", t.key_prefix(issue));
    let value = json!({ "issue": issue, "body": body });
    crate::db::enqueue_outbox_post(
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
    .await
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
) -> Result<bool, sqlx::Error> {
    let key = format!("{}:reaction:{content}", t.key_prefix(issue));
    let value = reaction_payload(issue, content, comment_id);
    crate::db::enqueue_outbox_post(
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
    .await
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
) -> Result<bool, sqlx::Error> {
    let key = format!("{}:reaction:verdict", t.key_prefix(issue));
    let value = reaction_payload(issue, content, comment_id);
    crate::db::enqueue_outbox_post(
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
    .await
}

/// The `reaction` intent payload (ADR-0068). `comment_id` is included **only when `Some`**, so the
/// reconciler routes on its presence: present → react on that issue comment (the `@mention` trigger);
/// absent → react on the PR/issue body. Pure, so the shape is unit-tested without a DB.
fn reaction_payload(issue: i64, content: &str, comment_id: Option<i64>) -> serde_json::Value {
    match comment_id {
        Some(cid) => json!({ "issue": issue, "content": content, "comment_id": cid }),
        None => json!({ "issue": issue, "content": content }),
    }
}

/// Enqueue the ADR-0056 failure notice — one per task (`<task>:failure_notice`). The reconciler re-checks
/// `has_posted_to_github` before posting, so a finalize-then-fail never double-posts.
pub async fn enqueue_failure_notice(
    pool: &PgPool,
    t: &Target<'_>,
    issue: i64,
) -> Result<bool, sqlx::Error> {
    let key = format!("{}:failure_notice", t.key_prefix(issue));
    let value = json!({ "issue": issue, "body": crate::review::render_failure_notice() });
    crate::db::enqueue_outbox_post(
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
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    // ADR-0068: the reaction payload carries `comment_id` ONLY when the task was @mention-triggered, so
    // the reconciler can route on its presence (comment vs PR/issue body). This is the round-trip the
    // reconciler's `deliver` reads back.
    #[test]
    fn reaction_payload_includes_comment_id_only_when_present() {
        // Mention-triggered: comment_id present → the reconciler reacts on the comment.
        let with = reaction_payload(7, "eyes", Some(4242));
        assert_eq!(with["issue"], 7);
        assert_eq!(with["content"], "eyes");
        assert_eq!(with["comment_id"], 4242);

        // Auto review: no trigger comment → the key is absent (not null), so `get("comment_id")` → None
        // and the reconciler falls back to the PR/issue body.
        let without = reaction_payload(7, "+1", None);
        assert_eq!(without["issue"], 7);
        assert_eq!(without["content"], "+1");
        assert!(
            without.get("comment_id").is_none(),
            "comment_id must be absent, not null, so the reconciler routes to the issue body"
        );
    }
}
