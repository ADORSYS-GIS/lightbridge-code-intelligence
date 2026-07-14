//! Review persistence: posted reviews (Epic #75), agent run transcripts (ADR-0034), review-run
//! telemetry, and the review-comment refs the feedback poller reads (ADR-0035 wiring). Split out of
//! the former monolithic `db.rs` (ADR-0086 follow-up) — pure move, no behavior change.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

/// A persisted review (Epic #75, Milestone C) — what the agent posted for a task, mirrored from the
/// GitHub PR review so the admin console can show it. Serialized straight to `GET /tasks/{id}/review`.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ReviewRow {
    pub task_id: Uuid,
    pub summary: String,
    pub body: String,
    pub inline_count: i32,
    pub deferred_count: i32,
    pub out_of_scope_count: i32,
    pub findings: Value,
    /// Permalink to the posted review on the PR (epic #89); `None` for older rows / if GitHub omitted it.
    pub review_url: Option<String>,
    /// The GitHub review id we created (ADR-0035) — kept so a feedback signal (👍/👎) can correlate
    /// back to this run. `None` for older rows / non-PR runs.
    pub platform_review_id: Option<i64>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Persist (or replace, on a re-post) the review posted for a task. Best-effort: the review is already
/// on GitHub by the time this is called, so a failure here is logged by the caller, not fatal.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_review(
    pool: &PgPool,
    task_id: Uuid,
    summary: &str,
    body: &str,
    inline_count: i32,
    deferred_count: i32,
    out_of_scope_count: i32,
    findings: &Value,
    review_url: Option<&str>,
    platform_review_id: Option<i64>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO reviews \
         (task_id, summary, body, inline_count, deferred_count, out_of_scope_count, findings, review_url, platform_review_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
         ON CONFLICT (task_id) DO UPDATE SET \
           summary = EXCLUDED.summary, body = EXCLUDED.body, \
           inline_count = EXCLUDED.inline_count, deferred_count = EXCLUDED.deferred_count, \
           out_of_scope_count = EXCLUDED.out_of_scope_count, findings = EXCLUDED.findings, \
           review_url = EXCLUDED.review_url, platform_review_id = EXCLUDED.platform_review_id, \
           created_at = now()",
    )
    .bind(task_id)
    .bind(summary)
    .bind(body)
    .bind(inline_count)
    .bind(deferred_count)
    .bind(out_of_scope_count)
    .bind(findings)
    .bind(review_url)
    .bind(platform_review_id)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Persist the silent-clean review copy (ADR-0068) **without clobbering** — `ON CONFLICT DO NOTHING`.
/// The clean path must never overwrite a row the reconciler wrote for a *posted* review (that would null
/// `platform_review_id` and break the ADR-0035 feedback join); a re-run of the clean path itself is a
/// no-op (same content). Contrast [`upsert_review`], which the reconciler uses at drain time where
/// replacing on a re-post is the point.
#[allow(clippy::too_many_arguments)]
pub async fn insert_review_if_absent(
    pool: &PgPool,
    task_id: Uuid,
    summary: &str,
    body: &str,
    inline_count: i32,
    deferred_count: i32,
    out_of_scope_count: i32,
    findings: &Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO reviews \
         (task_id, summary, body, inline_count, deferred_count, out_of_scope_count, findings) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (task_id) DO NOTHING",
    )
    .bind(task_id)
    .bind(summary)
    .bind(body)
    .bind(inline_count)
    .bind(deferred_count)
    .bind(out_of_scope_count)
    .bind(findings)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Whether this task already has a review going to (or on) GitHub: a `review` intent in the egress
/// outbox (ANY status — even a dead-lettered one means the run had findings, so a later re-finalize
/// against the cleared buffer must not re-read as "clean") or a persisted review that was actually
/// posted (`platform_review_id` set by the reconciler). The ADR-0068 silent-clean branch gates on this so
/// a re-finalize can't clobber a real review with a 👍-and-nothing.
pub async fn has_review_intent_or_posted_review(
    pool: &PgPool,
    task_id: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM outbox WHERE task_id = $1 AND kind = 'review') \
              OR EXISTS (SELECT 1 FROM reviews WHERE task_id = $1 AND platform_review_id IS NOT NULL)",
    )
    .bind(task_id)
    .fetch_one(pool)
    .await
}

/// The persisted review for a task, or `None` if none was recorded (e.g. an older run, an index task,
/// or a review that failed to post).
pub async fn get_review(pool: &PgPool, task_id: Uuid) -> Result<Option<ReviewRow>, sqlx::Error> {
    sqlx::query_as::<_, ReviewRow>("SELECT * FROM reviews WHERE task_id = $1")
        .bind(task_id)
        .fetch_optional(pool)
        .await
}

/// How many prior reviews of a target to carry into a re-review's context (ADR-0040 + ADR-0065). The
/// latest is rendered in detail and the rest compressed; this bounds the DB read and the block. A PR
/// re-reviewed dozens of times keeps only the most recent slice — older passes are the least relevant.
const PRIOR_REVIEWS_CAP: i64 = 20;

/// **All** prior reviews of the same target (ADR-0040 + ADR-0065), newest first, as
/// `(ordinal, summary, findings)` — used to feed a re-review its own past output so it
/// re-derives-then-reconciles instead of anchoring on a single prior verdict. `ordinal` is the review's
/// **true 1-based chronological position** (1 = the first review ever posted on this target), computed
/// with a window function over the FULL prior set *before* the `LIMIT` — so "review #1" stays the first
/// review even once a PR accumulates more than [`PRIOR_REVIEWS_CAP`] priors, and the labels never shift
/// between runs. `ORDER BY created_at DESC, task_id DESC` carries a unique tie-breaker so the "latest"
/// pick is deterministic even on equal timestamps. Joins `reviews` to `tasks` to match the same
/// `(repository_id, target_type, target_id)` as the current task, excluding the current task itself.
/// Returns an empty vec when this target has no earlier posted review (e.g. the first review on a freshly
/// opened PR). Best-effort context: the caller treats a query error as "no prior reviews" so a DB hiccup
/// degrades to the old blind re-review rather than failing the task. Capped at [`PRIOR_REVIEWS_CAP`].
pub async fn all_prior_reviews_for_target(
    pool: &PgPool,
    repository_id: i64,
    target_type: &str,
    target_id: i64,
    current_task_id: Uuid,
) -> Result<Vec<(i64, String, Value)>, sqlx::Error> {
    sqlx::query_as::<_, (i64, String, Value)>(
        "SELECT ordinal, summary, findings FROM ( \
             SELECT r.summary, r.findings, r.created_at, r.task_id, \
                    ROW_NUMBER() OVER (ORDER BY r.created_at ASC, r.task_id ASC) AS ordinal \
             FROM reviews r JOIN tasks t ON t.id = r.task_id \
             WHERE t.repository_id = $1 AND t.target_type = $2 AND t.target_id = $3 \
               AND r.task_id <> $4 \
         ) prior \
         ORDER BY created_at DESC, task_id DESC \
         LIMIT $5",
    )
    .bind(repository_id)
    .bind(target_type)
    .bind(target_id)
    .bind(current_task_id)
    .bind(PRIOR_REVIEWS_CAP)
    .fetch_all(pool)
    .await
}

/// Whether ANY prior review of this target (any commit, excluding the current task) carried at least one
/// finding — the ADR-0065 × ADR-0068 composition gate for the silent-clean path: full silence (👍 only,
/// no post) is only honest when there is nothing to reconcile. When prior findings exist and the current
/// run re-derived none, the verdict must POST so the retractions (which the prompt contract routes into
/// the verdict text) are visible on the PR. Type-guarded (`jsonb_typeof = 'array'`) so a legacy malformed
/// findings blob reads as "no findings" instead of erroring the whole gate.
pub async fn target_has_prior_findings(
    pool: &PgPool,
    repository_id: i64,
    target_type: &str,
    target_id: i64,
    current_task_id: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS ( \
             SELECT 1 FROM reviews r JOIN tasks t ON t.id = r.task_id \
             WHERE t.repository_id = $1 AND t.target_type = $2 AND t.target_id = $3 \
               AND r.task_id <> $4 \
               AND jsonb_typeof(r.findings) = 'array' AND jsonb_array_length(r.findings) > 0 \
         )",
    )
    .bind(repository_id)
    .bind(target_type)
    .bind(target_id)
    .bind(current_task_id)
    .fetch_one(pool)
    .await
}

/// The `findings` JSON arrays already posted — or **queued for posting** — by prior Lightbridge reviews
/// on the **same head_sha** as the current run (ADR-0065, Option B — finalize dedup). We match on
/// head_sha, not just the target, because line numbers drift across commits: a `(file, line, title)`
/// dedup key is only safe within one commit.
///
/// Two sources, unioned:
/// - `reviews` — reviews the reconciler already delivered (persisted at post time, ADR-0035);
/// - `outbox` `review`-kind rows still `pending` — a review **enqueued but not yet posted**
///   (reconciler backoff, or two rapid re-reviews racing finalize). Without these, the second finalize
///   wouldn't see the first run's findings and would double-post; their findings ride in the payload
///   (`payload->'findings_json'`, baked at produce time per ADR-0059). `posted` rows are skipped — the
///   reconciler persists them into `reviews` on delivery, so the first arm already covers them.
///
/// Excludes the current task in both arms (a re-finalize must not dedup against its own in-flight
/// review). Returns one `Value` (a findings array) per prior review; the caller flattens them into a set
/// of normalized keys. Best-effort: a query error is treated by the caller as "nothing posted yet" (no
/// dedup), never fatal.
pub async fn posted_findings_for_head(
    pool: &PgPool,
    repository_id: i64,
    target_type: &str,
    target_id: i64,
    head_sha: &str,
    current_task_id: Uuid,
) -> Result<Vec<Value>, sqlx::Error> {
    sqlx::query_scalar::<_, Value>(
        "SELECT r.findings \
         FROM reviews r JOIN tasks t ON t.id = r.task_id \
         WHERE t.repository_id = $1 AND t.target_type = $2 AND t.target_id = $3 \
           AND t.head_sha = $4 AND r.task_id <> $5 \
         UNION ALL \
         SELECT o.payload->'findings_json' \
         FROM outbox o JOIN tasks t ON t.id = o.task_id \
         WHERE o.kind = 'review' AND o.status = 'pending' \
           AND t.repository_id = $1 AND t.target_type = $2 AND t.target_id = $3 \
           AND t.head_sha = $4 AND o.task_id <> $5",
    )
    .bind(repository_id)
    .bind(target_type)
    .bind(target_id)
    .bind(head_sha)
    .bind(current_task_id)
    .fetch_all(pool)
    .await
}

// ── ADR-0034 agent run transcript ──────────────────────────────────────────────────────────────

/// One transcript entry submitted by the runner (the ingest shape; mirrors
/// `lci-agent-clients::TranscriptEntry`).
#[derive(Debug, Deserialize)]
pub struct TranscriptInput {
    pub role: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Value>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub prompt_tokens: Option<i64>,
    #[serde(default)]
    pub completion_tokens: Option<i64>,
    #[serde(default)]
    pub reasoning_tokens: Option<i64>,
    #[serde(default)]
    pub model: Option<String>,
}

/// One stored transcript entry, serialized to `GET /tasks/{id}/transcript` (ADR-0034) for the
/// dashboard timeline.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct TranscriptRow {
    pub seq: i32,
    pub role: String,
    pub content: Option<String>,
    pub tool_calls: Option<Value>,
    pub tool_name: Option<String>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Replace a task's transcript with `entries` (ordered). The runner submits the whole transcript once
/// at run end; a re-submit (task retry) replaces the prior rows, so the row set always reflects the
/// latest run. Done in one transaction so a reader never sees a half-written transcript.
pub async fn replace_transcript(
    pool: &PgPool,
    task_id: Uuid,
    entries: &[TranscriptInput],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM agent_transcript WHERE task_id = $1")
        .bind(task_id)
        .execute(&mut *tx)
        .await?;
    for (seq, e) in entries.iter().enumerate() {
        sqlx::query(
            "INSERT INTO agent_transcript \
             (id, task_id, seq, role, content, tool_calls, tool_name, prompt_tokens, completion_tokens, \
              reasoning_tokens, model) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(Uuid::new_v4())
        .bind(task_id)
        .bind(seq as i32)
        .bind(&e.role)
        .bind(&e.content)
        .bind(&e.tool_calls)
        .bind(&e.tool_name)
        .bind(e.prompt_tokens)
        .bind(e.completion_tokens)
        .bind(e.reasoning_tokens)
        .bind(&e.model)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// Load a task's transcript in order (ADR-0034), or an empty vec if none was recorded.
pub async fn get_transcript(
    pool: &PgPool,
    task_id: Uuid,
) -> Result<Vec<TranscriptRow>, sqlx::Error> {
    sqlx::query_as::<_, TranscriptRow>(
        "SELECT seq, role, content, tool_calls, tool_name, prompt_tokens, completion_tokens, created_at \
         FROM agent_transcript WHERE task_id = $1 ORDER BY seq",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await
}

/// Record a review run's telemetry (extends ADR-0034/0017/0060) on the task row: the tool set OFFERED
/// to the model this run (`run_tools`, a `[{name, source}]` array) and the resolved, **already-redacted +
/// base64-encoded** `ReviewConfig` (`run_config_b64`). The runner submits both at run START, so a
/// crashed/aborted run still has its config recorded. One task = one run, so this is a plain UPDATE in
/// place (latest-run-replace semantics, matching how the transcript is replaced per run). Indexing runs
/// never call this, so their columns stay NULL. Returns whether a row was updated — `false` means the
/// task id is unknown, so the caller can 404 without a separate existence SELECT (gemini review on #270).
pub async fn record_review_run_telemetry(
    pool: &PgPool,
    task_id: Uuid,
    run_tools: &Value,
    run_config_b64: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query("UPDATE tasks SET run_tools = $2, run_config_b64 = $3 WHERE id = $1")
        .bind(task_id)
        .bind(run_tools)
        .bind(run_config_b64)
        .execute(pool)
        .await
        .map(|r| r.rows_affected() > 0)
}

// ── ADR-0035 review feedback (poll reactions on our comments) ───────────────────────────────────

/// A comment we created at write-back, recorded so the poller knows what to poll. `kind` selects the
/// reactions endpoint (`inline` → pulls/comments, `reply` → issues/comments); `file`/`line` correlate
/// an inline comment to its finding.
#[derive(Debug, Clone)]
pub struct ReviewCommentRef {
    pub platform_comment_id: i64,
    pub kind: String,
    pub file: Option<String>,
    pub line: Option<i32>,
}

/// Record the comment ids created for a task (idempotent — a re-post DO NOTHINGs on the existing id).
pub async fn store_review_comments(
    pool: &PgPool,
    task_id: Uuid,
    comments: &[ReviewCommentRef],
) -> Result<(), sqlx::Error> {
    for c in comments {
        sqlx::query(
            "INSERT INTO review_comments (id, task_id, platform_comment_id, kind, file, line) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (kind, platform_comment_id) DO NOTHING",
        )
        .bind(Uuid::new_v4())
        .bind(task_id)
        .bind(c.platform_comment_id)
        .bind(&c.kind)
        .bind(&c.file)
        .bind(c.line)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Whether the task has already responded **or is about to** — anything posted to GitHub (a `reviews`
/// row, or any recorded `review_comments`: an inline finding, a reply, or a prior failure notice) OR a
/// `review`/`reply` intent still in flight (`pending`/`posted`) in the egress outbox. This is the gate
/// the reconciler's failure-notice handler uses (ADR-0056/0059): it must never post a notice on top of a
/// real review, and — because the `reviews` row is now written at *drain* time — a review intent that's
/// enqueued-but-not-yet-delivered (e.g. transiently backing off) would otherwise read as "nothing posted"
/// and let a misleading failure notice race ahead of it (#219 review). A `failed` (dead-lettered) review
/// intent is deliberately excluded — then the review truly won't post, so the notice *should* fire.
/// Idempotent across retries because the notice itself is recorded as a `review_comments` row.
pub async fn has_responded_or_pending_content(
    pool: &PgPool,
    task_id: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM reviews WHERE task_id = $1) \
              OR EXISTS (SELECT 1 FROM review_comments WHERE task_id = $1) \
              OR EXISTS (SELECT 1 FROM outbox \
                         WHERE task_id = $1 AND kind IN ('review', 'reply') \
                           AND status IN ('pending', 'posted'))",
    )
    .bind(task_id)
    .fetch_one(pool)
    .await
}
