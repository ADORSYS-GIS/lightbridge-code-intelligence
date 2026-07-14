//! ADR-0037 pending review actions: the agent's mediated write tools (`add_review_comment` /
//! `add_comment` / `set_summary`) accumulate here during a run; the control plane flushes them as one
//! grouped review on clean completion. Split out of the former monolithic `db.rs` (ADR-0086
//! follow-up) — pure move, no behavior change.

use sqlx::PgPool;
use uuid::Uuid;

// ── ADR-0037 pending review actions ────────────────────────────────────────────────────────────
// The agent's mediated write tools (add_review_comment / add_comment / set_summary) accumulate here
// during a run; the control plane flushes them as one grouped review on clean completion.

/// One buffered inline finding (the `add_review_comment` payload), read back at flush time.
#[derive(Debug, sqlx::FromRow)]
pub struct PendingInline {
    pub file: String,
    pub line: i32,
    pub title: Option<String>,
    pub priority: Option<String>,
    pub category: Option<String>,
    pub suggestion: Option<String>,
    pub body: String,
}

/// The accumulated buffer for a task: inline findings (deduped by file+line), plain comment bodies
/// (in call order), and the latest summary. Drives the single flush (ADR-0037).
#[derive(Debug, Default)]
pub struct PendingReview {
    pub inline: Vec<PendingInline>,
    pub comments: Vec<String>,
    pub summary: Option<String>,
}

impl PendingReview {
    /// True when the agent called no write tool at all — the empty-run case that still gets a default
    /// "no issues found" review so an `@mention` is never a silent hang (ADR-0037).
    pub fn is_empty(&self) -> bool {
        self.inline.is_empty() && self.comments.is_empty() && self.summary.is_none()
    }
}

/// Delete one buffered inline finding by `(task_id, file, line)` — the refute pass retracting a P0/P1
/// that didn't survive verification before it is ever posted (Phase 2, ADR-0043). Idempotent: deleting
/// a finding that isn't there is a no-op (the agent may retract speculatively).
pub async fn delete_pending_inline(
    pool: &PgPool,
    task_id: Uuid,
    file: &str,
    line: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM pending_review_actions \
         WHERE task_id = $1 AND action = 'inline' AND file = $2 AND line = $3",
    )
    .bind(task_id)
    .bind(file)
    .bind(line)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Clear a task's accumulation buffer. Called when a runner (re)starts the task so a retry begins from
/// empty rather than appending to a partial buffer (ADR-0037 idempotency), and after a flush.
pub async fn clear_pending_review(pool: &PgPool, task_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM pending_review_actions WHERE task_id = $1")
        .bind(task_id)
        .execute(pool)
        .await
        .map(|_| ())
}

/// Clear one class of buffered action for a task (`inline` | `comment` | `summary`). Used by the
/// flush to drop each part **as it posts**, so a second finalize (e.g. a retried delivery) re-posts
/// only the parts that previously failed — never a duplicate of one that already succeeded (ADR-0037).
pub async fn clear_pending_action(
    pool: &PgPool,
    task_id: Uuid,
    action: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM pending_review_actions WHERE task_id = $1 AND action = $2")
        .bind(task_id)
        .bind(action)
        .execute(pool)
        .await
        .map(|_| ())
}

/// Buffer (or overwrite) one inline finding. Last write wins per `(task, file, line)` — a
/// re-emitted finding refines rather than duplicates (ADR-0037; content hashes would let a reworded
/// re-run slip through).
#[allow(clippy::too_many_arguments)]
pub async fn upsert_pending_inline(
    pool: &PgPool,
    task_id: Uuid,
    file: &str,
    line: i32,
    title: Option<&str>,
    priority: Option<&str>,
    category: Option<&str>,
    suggestion: Option<&str>,
    body: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO pending_review_actions \
         (task_id, action, file, line, title, priority, category, suggestion, body) \
         VALUES ($1, 'inline', $2, $3, $4, $5, $6, $7, $8) \
         ON CONFLICT (task_id, file, line) WHERE action = 'inline' DO UPDATE SET \
           title = EXCLUDED.title, priority = EXCLUDED.priority, category = EXCLUDED.category, \
           suggestion = EXCLUDED.suggestion, body = EXCLUDED.body",
    )
    .bind(task_id)
    .bind(file)
    .bind(line)
    .bind(title)
    .bind(priority)
    .bind(category)
    .bind(suggestion)
    .bind(body)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Buffer one plain thread comment (the `add_comment` payload). Append-only; the bodies are
/// consolidated into a single reply at flush so multiple calls don't fan out into notifications.
///
/// Replay dedup (ADR-0087 C2): under `CheckpointRuntime` a crash in the persist window can re-execute
/// this write step on resume and double-append. When the caller threads the tool `call_id` (and the
/// run's `run_epoch`, resolved server-side) the insert is idempotent on the partial unique index
/// `(task_id, run_epoch, call_id) WHERE action = 'comment' AND call_id IS NOT NULL` via
/// `ON CONFLICT DO NOTHING` — a replayed reply is a no-op. Legacy callers pass `call_id = None`: the
/// partial index ignores NULL `call_id`, so those rows always append exactly as before (prod-neutral).
pub async fn add_pending_comment(
    pool: &PgPool,
    task_id: Uuid,
    run_epoch: Option<i32>,
    call_id: Option<&str>,
    body: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO pending_review_actions (task_id, action, run_epoch, call_id, body) \
         VALUES ($1, 'comment', $2, $3, $4) \
         ON CONFLICT (task_id, run_epoch, call_id) WHERE action = 'comment' AND call_id IS NOT NULL \
         DO NOTHING",
    )
    .bind(task_id)
    .bind(run_epoch)
    .bind(call_id)
    .bind(body)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Set (or replace) the run's summary/verdict (the `set_summary` payload). One per task.
pub async fn upsert_pending_summary(
    pool: &PgPool,
    task_id: Uuid,
    body: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO pending_review_actions (task_id, action, body) VALUES ($1, 'summary', $2) \
         ON CONFLICT (task_id) WHERE action = 'summary' DO UPDATE SET body = EXCLUDED.body",
    )
    .bind(task_id)
    .bind(body)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Record the task's **emergent** run kind (ADR-0037), derived at flush from which write tools fired
/// (`review` / `ask` / `mixed`). Best-effort observability — it doesn't gate behaviour.
pub async fn set_task_kind(pool: &PgPool, task_id: Uuid, kind: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE tasks SET kind = $2 WHERE id = $1")
        .bind(task_id)
        .bind(kind)
        .execute(pool)
        .await
        .map(|_| ())
}

/// Load a task's accumulated buffer for the flush: inline findings (call order), comment bodies (call
/// order), and the summary if set.
pub async fn load_pending_review(
    pool: &PgPool,
    task_id: Uuid,
) -> Result<PendingReview, sqlx::Error> {
    let inline = sqlx::query_as::<_, PendingInline>(
        "SELECT file, line, title, priority, category, suggestion, body \
         FROM pending_review_actions WHERE task_id = $1 AND action = 'inline' ORDER BY id",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await?;
    let comments =
        sqlx::query_scalar::<_, String>("SELECT body FROM pending_review_actions WHERE task_id = $1 AND action = 'comment' ORDER BY id")
            .bind(task_id)
            .fetch_all(pool)
            .await?;
    let summary = sqlx::query_scalar::<_, String>(
        "SELECT body FROM pending_review_actions WHERE task_id = $1 AND action = 'summary'",
    )
    .bind(task_id)
    .fetch_optional(pool)
    .await?;
    Ok(PendingReview {
        inline,
        comments,
        summary,
    })
}
