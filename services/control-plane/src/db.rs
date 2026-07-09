//! Postgres persistence (hand-written SQLx; cratestack codegen deferred — ADR-0005).
//!
//! Runtime queries only (no compile-time `query!`), so the crate builds without a database. The
//! pool is optional: absent `DATABASE_URL` the control plane runs in a degraded, in-memory mode
//! (dev) and readiness reports it.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::integrations::platform::Platform;

/// Postgres `LISTEN`/`NOTIFY` channel the dispatcher waits on; `create_task` notifies it on enqueue
/// so a dispatcher reacts immediately instead of waiting for its poll fallback.
pub const TASK_QUEUED_CHANNEL: &str = "task_queued";

/// Connect to `DATABASE_URL` and run migrations. Returns `Ok(None)` when the URL is unset (dev).
/// **Fails fast** (`Err`) when the URL is set but the database is unreachable or migrations fail —
/// the process should exit so the orchestrator restarts it and retries, rather than running
/// permanently unready with no recovery path.
pub async fn connect_from_env() -> anyhow::Result<Option<PgPool>> {
    use anyhow::Context;
    let url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(error) => {
            return Err(anyhow::Error::from(error).context("failed to read DATABASE_URL"));
        }
    };
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .context("failed to connect to DATABASE_URL")?;
    sqlx::migrate!()
        .run(&pool)
        .await
        .context("database migrations failed")?;
    tracing::info!("database connected and migrations applied");
    Ok(Some(pool))
}

/// Reconcile the `code_chunks.embedding` column width to the configured `dimension` (ADR-0018). The
/// pgvector column is a fixed-width `vector(N)`, so changing the embedding model's dimension is
/// **destructive** — every stored vector is the wrong width. No-op when the column already matches
/// (or isn't present / has no fixed dim). On a mismatch: if `allow_clear`, **TRUNCATE `code_chunks`
/// and ALTER the column** to the new width; else return `Err` (fail loud) so a config typo can't
/// silently wipe the semantic index. Idempotent + safe to run from each role at startup.
pub async fn reconcile_embedding_dimension(
    pool: &PgPool,
    dimension: i64,
    allow_clear: bool,
) -> anyhow::Result<()> {
    use anyhow::bail;
    // pgvector stores the dimension in the column's `atttypmod` (== N for `vector(N)`, -1 if none).
    // `to_regclass` resolves the table via the active search_path and yields NULL (→ no row) when it
    // doesn't exist, so this is schema-safe and a no-op before the table is created.
    let current: Option<i32> = sqlx::query_scalar(
        "SELECT atttypmod FROM pg_attribute \
         WHERE attrelid = to_regclass('code_chunks') AND attname = 'embedding' AND NOT attisdropped",
    )
    .fetch_optional(pool)
    .await?;
    let Some(current) = current.filter(|&m| m > 0).map(i64::from) else {
        return Ok(()); // no code_chunks/embedding column or no fixed dimension yet — nothing to do
    };
    if current == dimension {
        return Ok(());
    }
    if !allow_clear {
        bail!(
            "embedding dimension changed ({current} → {dimension}) but \
             embeddings.allow_reindex_on_dim_change is false; refusing to wipe code_chunks. \
             Set the flag to reindex from scratch, or revert the dimension."
        );
    }
    tracing::warn!(
        from = current,
        to = dimension,
        "embedding dimension changed; TRUNCATE code_chunks + ALTER column (reindex from scratch)"
    );
    // Atomic: TRUNCATE + ALTER in one transaction so a failed ALTER can't leave the table truncated
    // but still at the old width (an inconsistent state the next startup wouldn't detect).
    let mut tx = pool.begin().await?;
    sqlx::query("TRUNCATE TABLE code_chunks")
        .execute(&mut *tx)
        .await?;
    // `dimension` is an i64 from typed config (not user free-text), so formatting it into the DDL is
    // safe; the vector type width can't be a bind parameter.
    sqlx::query(&format!(
        "ALTER TABLE code_chunks ALTER COLUMN embedding TYPE vector({dimension})"
    ))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

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
/// `agent-runner::bootstrap::client::TranscriptEntry`).
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

// ── GitHub egress outbox (ADR-0059) ────────────────────────────────────────────────────────────────
// Every outbound GitHub *content* write is an intent row here; the reconciler is the sole consumer that
// posts it. Producers only enqueue.

/// `LISTEN`/`NOTIFY` channel the reconciler's outbox drain waits on; producers notify it on enqueue
/// (the timer fallback in the reconciler covers a missed notify, exactly like the dispatcher on
/// [`TASK_QUEUED_CHANNEL`]).
pub const OUTBOX_CHANNEL: &str = "outbox";

/// Max delivery attempts before an outbox row is parked `failed` (dead-letter). A courtesy post isn't
/// worth retrying forever; the row stays for inspection.
pub const OUTBOX_MAX_ATTEMPTS: i32 = 6;

/// A claimed outbox row, with the coordinates the reconciler needs to post it (no join required).
#[derive(Debug, sqlx::FromRow)]
pub struct OutboxRow {
    pub id: i64,
    pub task_id: Option<Uuid>,
    pub installation_id: i64,
    pub owner: String,
    pub repo: String,
    pub kind: String,
    pub payload: Value,
    pub attempts: i32,
    pub platform: Platform,
}

/// Enqueue one GitHub-egress intent. Idempotent on `dedup_key` (`ON CONFLICT DO NOTHING`), so a
/// re-finalize or a retry never double-enqueues, and wakes the reconciler via `NOTIFY`. Returns whether
/// a new row was inserted (`false` = a row with this `dedup_key` already existed).
#[allow(clippy::too_many_arguments)]
pub async fn enqueue_outbox_post(
    pool: &PgPool,
    platform: Platform,
    task_id: Option<Uuid>,
    installation_id: i64,
    owner: &str,
    repo: &str,
    kind: &str,
    payload: &Value,
    dedup_key: &str,
) -> Result<bool, sqlx::Error> {
    let inserted = sqlx::query(
        "INSERT INTO outbox (platform, task_id, installation_id, owner, repo, kind, payload, dedup_key) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (dedup_key) DO NOTHING",
    )
    .bind(platform)
    .bind(task_id)
    .bind(installation_id)
    .bind(owner)
    .bind(repo)
    .bind(kind)
    .bind(payload)
    .bind(dedup_key)
    .execute(pool)
    .await?
    .rows_affected()
        > 0;
    if inserted {
        let _ = sqlx::query("SELECT pg_notify($1, $2)")
            .bind(OUTBOX_CHANNEL)
            .bind(dedup_key)
            .execute(pool)
            .await;
    }
    Ok(inserted)
}

/// Claim up to `limit` due outbox rows in `(created_at, id)` order — `id` breaks the `created_at` ties
/// that one-transaction enqueues share (`now()` is transaction-stable). The caller posts each, then
/// [`mark_outbox_posted`] / [`mark_outbox_failed`].
///
/// `FOR UPDATE SKIP LOCKED` only holds the row lock for the duration of *this* statement (no enclosing
/// transaction), so it does **not** protect the claim→post→mark gap — two replicas could each claim a
/// distinct disjoint set, then both deliver. Correctness here rests on the **single-replica** invariant
/// (ADR-0058/0059), not the lock; the `SKIP LOCKED` just avoids a self-collision if a claim ever overlaps
/// an in-flight one.
pub async fn claim_outbox_batch(pool: &PgPool, limit: i64) -> Result<Vec<OutboxRow>, sqlx::Error> {
    sqlx::query_as::<_, OutboxRow>(
        "SELECT id, task_id, installation_id, owner, repo, kind, payload, attempts, platform \
         FROM outbox \
         WHERE status = 'pending' AND next_attempt_at <= now() \
         ORDER BY created_at, id \
         LIMIT $1 \
         FOR UPDATE SKIP LOCKED",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Mark an outbox row delivered, recording the posted GitHub id (review/comment) for correlation.
pub async fn mark_outbox_posted(
    pool: &PgPool,
    id: i64,
    platform_ref_id: Option<i64>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE outbox SET status = 'posted', posted_at = now(), platform_ref_id = $2 WHERE id = $1",
    )
    .bind(id)
    .bind(platform_ref_id)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Record a failed delivery: bump `attempts`, stash the error, and either back off (`pending`, retried
/// after `attempts²` minutes) or park as `failed` once `OUTBOX_MAX_ATTEMPTS` is reached.
pub async fn mark_outbox_failed(pool: &PgPool, id: i64, error: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE outbox SET \
             attempts = attempts + 1, \
             last_error = $2, \
             status = CASE WHEN attempts + 1 >= $3 THEN 'failed' ELSE 'pending' END, \
             next_attempt_at = now() + make_interval(mins => (attempts + 1) * (attempts + 1)) \
         WHERE id = $1",
    )
    .bind(id)
    .bind(error)
    .bind(OUTBOX_MAX_ATTEMPTS)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Load one outbox row **iff it is still `pending`**, by id — the status-guarded read the
/// `PlatformEgress` virtual object runs inside `ctx.run` before posting (RFC-0005 / ADR-0074). Returns
/// `None` when the row is absent (pruned) **or** already terminal (`posted`/`failed`) — either way the
/// handler skips, which is what makes redelivery idempotent under the Restate path: a row a prior
/// invocation (or a mode-flip drain) already settled is never re-posted. Mirrors the `status = 'pending'`
/// filter the drain's [`claim_outbox_batch`] applies.
pub async fn load_pending_outbox_row(
    pool: &PgPool,
    id: i64,
) -> Result<Option<OutboxRow>, sqlx::Error> {
    sqlx::query_as::<_, OutboxRow>(
        "SELECT id, task_id, installation_id, owner, repo, kind, payload, attempts, platform \
         FROM outbox \
         WHERE id = $1 AND status = 'pending'",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// Resolve the `outbox.id` for a `dedup_key` — the producer's bridge between the idempotent intent row
/// ([`enqueue_outbox_post`], which returns only whether it inserted) and the `PlatformEgress::post`
/// invocation, which is keyed on the row id (RFC-0005 / ADR-0074). Read only on the `restate` egress
/// path; the `drain` default never calls it, so the enqueue path is byte-for-byte unchanged by the pilot.
///
/// `dedup_key` is `UNIQUE` (the `ON CONFLICT (dedup_key)` in [`enqueue_outbox_post`]), so this already
/// matches at most one row. The explicit `LIMIT 1` is defensive: it keeps the "one id per key" contract
/// stated at the query if a future migration ever relaxes that constraint.
pub async fn outbox_id_by_dedup_key(
    pool: &PgPool,
    dedup_key: &str,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, i64>("SELECT id FROM outbox WHERE dedup_key = $1 LIMIT 1")
        .bind(dedup_key)
        .fetch_optional(pool)
        .await
}

/// Dead-letter an outbox row: park it `failed` **unconditionally** with the terminal error, recording
/// the attempt. This is the `PlatformEgress` handler's `TerminalError` branch (RFC-0005 / ADR-0074) —
/// the engine's retry policy has already exhausted its ceiling, so unlike [`mark_outbox_failed`] (which
/// re-derives `pending`-vs-`failed` from `attempts²`) this makes the row terminal in one step. Mirrors
/// `mark_outbox_failed`'s dead-letter destination so both egress paths settle a give-up the same way.
///
/// The `AND status = 'pending'` guard is defensive: it makes the terminal write a no-op if another
/// consumer already settled the row (`posted`/`failed`), so a give-up can never clobber a delivered row.
/// Not reachable in steady state (the drain is off in `restate` mode and the virtual object is
/// single-writer per key), but it costs nothing and keeps the settle-once invariant local to the query.
pub async fn mark_outbox_dead_letter(
    pool: &PgPool,
    id: i64,
    error: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE outbox SET \
             attempts = attempts + 1, \
             last_error = $2, \
             status = 'failed' \
         WHERE id = $1 AND status = 'pending'",
    )
    .bind(id)
    .bind(error)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Prune terminal outbox rows past their retention window (ADR-0059 GC). `outbox` is
/// append-mostly — every delivered intent settles to `posted` (a 👀 reaction alone leaves a permanent
/// row per PR) and every dead-lettered one to `failed`, and nothing ever deletes them — so the table,
/// and the `ON CONFLICT (dedup_key)` probe every enqueue pays against it, grow without bound.
///
/// `posted` rows are deleted `posted_retention_days` after delivery (the feedback-join id was recorded
/// at post time, so the row has served its purpose); `failed` rows are kept `failed_retention_days` —
/// longer, for post-mortem inspection — then deleted, keyed off `created_at` (a dead-lettered row has
/// no `posted_at`; the few hours of retries between enqueue and dead-letter are negligible against the
/// multi-week window). `pending` rows are in-flight and never touched, whatever their age. Returns
/// `(posted_deleted, failed_deleted)`.
///
/// A non-positive window is treated as **skip that prune**, never "delete everything": `now() -
/// make_interval(days => 0)` is `now()`, so a `0` would make `posted_at < now()` match every delivered
/// row. The dispatcher already floors its config to a positive default; this second guard keeps the
/// public helper safe for any direct/test caller. The day count binds as `int8` and is narrowed in SQL
/// via `$1::int` (what `make_interval(days …)` — an `int` arg — wants), so an out-of-range value errors
/// loudly (`integer out of range`) instead of silently wrapping like a Rust `as i32` would.
pub async fn prune_outbox(
    pool: &PgPool,
    posted_retention_days: i64,
    failed_retention_days: i64,
) -> Result<(u64, u64), sqlx::Error> {
    let posted = if posted_retention_days > 0 {
        sqlx::query(
            "DELETE FROM outbox \
             WHERE status = 'posted' AND posted_at < now() - make_interval(days => $1::int)",
        )
        .bind(posted_retention_days)
        .execute(pool)
        .await?
        .rows_affected()
    } else {
        0
    };
    let failed = if failed_retention_days > 0 {
        sqlx::query(
            "DELETE FROM outbox \
             WHERE status = 'failed' AND created_at < now() - make_interval(days => $1::int)",
        )
        .bind(failed_retention_days)
        .execute(pool)
        .await?
        .rows_affected()
    } else {
        0
    };
    Ok((posted, failed))
}

/// A comment the poller should check for reactions, with the repo coordinates + installation needed to
/// mint a token and hit the reactions API.
#[derive(Debug, sqlx::FromRow)]
pub struct PollableComment {
    pub task_id: Uuid,
    pub platform_comment_id: i64,
    pub kind: String,
    pub owner: String,
    pub name: String,
    pub installation_id: i64,
    pub platform: Platform,
}

/// Comments to poll this cycle (ADR-0035), within `within_days`, **tiered by age** so API usage stays
/// flat regardless of repo activity instead of re-reading every comment every cycle: fresh comments
/// (< 1 day) every cycle, 1–3 days old ~1-in-12 cycles, 3+ days old ~1-in-72 cycles. The tier filter
/// is a deterministic `comment_id % N == current_cycle % N` (the cycle index is wall-clock /
/// `interval_secs`), so it needs no per-comment state and spreads load evenly across cycles. Joined to
/// the repo for owner/name/installation.
pub async fn list_pollable_comments(
    pool: &PgPool,
    within_days: i32,
    interval_secs: i64,
) -> Result<Vec<PollableComment>, sqlx::Error> {
    // Filter to GitHub only to avoid wasting cycles on no-op GitLab API calls (GitLab feedback polling
    // requires MR/issue iid which is stored in the outbox payload).
    sqlx::query_as::<_, PollableComment>(
        "SELECT rc.task_id, rc.platform_comment_id, rc.kind, r.owner, r.name, t.installation_id, r.platform \
         FROM review_comments rc \
         JOIN tasks t ON t.id = rc.task_id \
         JOIN repositories r ON r.id = t.repository_id \
         WHERE r.platform = 'github' \
           AND t.created_at > now() - ($1 * interval '1 day') \
           AND ( \
             rc.created_at > now() - interval '1 day' \
             OR (rc.created_at BETWEEN now() - interval '3 days' AND now() - interval '1 day' \
                 AND (rc.platform_comment_id % 12) = (extract(epoch from now())::bigint / $2) % 12) \
             OR (rc.created_at < now() - interval '3 days' \
                 AND (rc.platform_comment_id % 72) = (extract(epoch from now())::bigint / $2) % 72) \
           ) \
         ORDER BY rc.created_at",
    )
    .bind(within_days)
    .bind(interval_secs.max(1))
    .fetch_all(pool)
    .await
}

/// Reconcile the reactions on one comment with GitHub (ADR-0035): insert any new `(reactor, reaction)`
/// and delete any that have disappeared (the un-react case — no webhook needed). `reactions` is the
/// current full set from the API; an empty set removes all stored feedback for the comment.
pub async fn reconcile_comment_feedback(
    pool: &PgPool,
    task_id: Uuid,
    platform_comment_id: i64,
    kind: &str,
    reactions: &[(String, String)],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    for (reactor, reaction) in reactions {
        sqlx::query(
            "INSERT INTO review_feedback \
             (id, task_id, platform_comment_id, comment_kind, reactor, reaction) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (platform_comment_id, comment_kind, reactor, reaction) DO NOTHING",
        )
        .bind(Uuid::new_v4())
        .bind(task_id)
        .bind(platform_comment_id)
        .bind(kind)
        .bind(reactor)
        .bind(reaction)
        .execute(&mut *tx)
        .await?;
    }
    // Drop feedback no longer present on GitHub. `reactor` and `reaction` are constrained values (a
    // GitHub login and a fixed reaction vocabulary), so `|` is a safe key separator. An empty
    // `present` set makes `<> ALL('{}')` true for every row → all are deleted.
    let present: Vec<String> = reactions.iter().map(|(u, c)| format!("{u}|{c}")).collect();
    sqlx::query(
        "DELETE FROM review_feedback \
         WHERE platform_comment_id = $1 AND comment_kind = $2 \
           AND (reactor || '|' || reaction) <> ALL($3)",
    )
    .bind(platform_comment_id)
    .bind(kind)
    .bind(&present)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// One stored reaction, serialized to `GET /tasks/{id}/feedback` (ADR-0035). `file`/`line` are joined
/// from the comment so the dashboard can show feedback per finding.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct FeedbackRow {
    pub platform_comment_id: i64,
    pub comment_kind: String,
    pub reactor: String,
    pub reaction: String,
    pub file: Option<String>,
    pub line: Option<i32>,
}

/// Findings in this repo that a human **rejected** with a 👎 (`-1`) reaction (M1 feedback memory,
/// ADR-0044): joins the reaction → the inline comment's `(file, line)` → the matching finding in that
/// run's `reviews.findings` to recover its title. Fed back into future reviews as "previously rejected
/// here — don't repeat" so the agent stops re-raising the same false positives. Bounded; best-effort
/// (a path-normalization mismatch just misses a row). Returns `(file, line, title)`.
pub async fn rejected_findings_for_repo(
    pool: &PgPool,
    repository_id: i64,
    limit: i64,
) -> Result<Vec<(String, i32, String)>, sqlx::Error> {
    sqlx::query_as::<_, (String, i32, String)>(
        "SELECT DISTINCT rc.file, rc.line, finding->>'title' AS title \
         FROM review_feedback f \
         JOIN review_comments rc \
           ON rc.platform_comment_id = f.platform_comment_id AND rc.kind = f.comment_kind \
         JOIN tasks t ON t.id = f.task_id \
         JOIN reviews r ON r.task_id = f.task_id \
         JOIN LATERAL jsonb_array_elements(r.findings) finding \
           ON finding->>'file' = rc.file AND (finding->>'line')::int = rc.line \
         WHERE t.repository_id = $1 AND f.reaction = '-1' AND rc.kind = 'inline' \
           AND finding->>'title' IS NOT NULL \
         ORDER BY rc.file, rc.line \
         LIMIT $2",
    )
    .bind(repository_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// All feedback recorded for a task (ADR-0035), for the dashboard.
pub async fn get_feedback(pool: &PgPool, task_id: Uuid) -> Result<Vec<FeedbackRow>, sqlx::Error> {
    sqlx::query_as::<_, FeedbackRow>(
        "SELECT f.platform_comment_id, f.comment_kind, f.reactor, f.reaction, rc.file, rc.line \
         FROM review_feedback f \
         LEFT JOIN review_comments rc \
           ON rc.platform_comment_id = f.platform_comment_id AND rc.kind = f.comment_kind \
         WHERE f.task_id = $1 ORDER BY f.created_at",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await
}

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
pub async fn add_pending_comment(
    pool: &PgPool,
    task_id: Uuid,
    body: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO pending_review_actions (task_id, action, body) VALUES ($1, 'comment', $2)",
    )
    .bind(task_id)
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

/// Persist a GitHub delivery, using its `delivery_id` PRIMARY KEY for exactly-once handling.
/// Returns `true` if the delivery is new (inserted), `false` if it was already seen (duplicate).
pub async fn record_delivery(
    pool: &PgPool,
    platform: Platform,
    delivery_id: &str,
    event_name: &str,
    payload: &Value,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO webhook_deliveries (platform, delivery_id, event_name, payload_json) \
         VALUES ($1, $2, $3, $4) ON CONFLICT (delivery_id) DO NOTHING",
    )
    .bind(platform)
    .bind(delivery_id)
    .bind(event_name)
    .bind(payload)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Liveness of the connection pool (used by readiness).
pub async fn ping(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT 1").execute(pool).await.map(|_| ())
}

/// A task row as stored — one task run for the dashboard (ADR-0016). Serialized directly to the
/// `/tasks` API (timestamps as RFC 3339). The `repo_*` fields are joined from `repositories` so the
/// dashboard can show a human repo name + branch without a second round-trip (LEFT JOIN, so they're
/// `None` for the rare orphaned row).
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct TaskRow {
    pub id: Uuid,
    pub repository_id: i64,
    pub installation_id: i64,
    /// `None` for admin-initiated tasks (e.g. index-on-approve) that have no originating webhook
    /// delivery; `Some` for webhook-created tasks. (Column is nullable since migration 0008.)
    pub webhook_delivery_id: Option<String>,
    pub target_type: String,
    pub target_id: i64,
    pub command_text: String,
    /// Run kind (ADR-0033): `review` (diff-scoped findings) or `ask` (a conversational answer).
    /// Defaults to `review` for rows created before migration 0011.
    pub kind: String,
    pub base_sha: Option<String>,
    pub head_sha: Option<String>,
    pub status: String,
    pub priority: i32,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub started_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub completed_at: Option<OffsetDateTime>,
    pub repo_owner: Option<String>,
    pub repo_name: Option<String>,
    pub repo_default_branch: Option<String>,
    /// The Kubernetes Job name (set once dispatched), so the console can stream the run's logs. `None`
    /// before dispatch or after the Job is reaped/TTL'd. Already selected by `TASK_SELECT` (`t.*`).
    pub job_name: Option<String>,
    /// The runner's free-text status `detail` (#137), persisted on the last status report that carried
    /// one. `None` for a genuine clean success / runs that predate migration 0016; `Some` records why
    /// a run did not post a review (e.g. a failure reason, or a "posted nothing" no-op), so the console
    /// can tell a silent no-op apart from a real clean review. Selected by `TASK_SELECT` (`t.*`).
    pub error_detail: Option<String>,
}

/// `SELECT` projection shared by the list and detail queries: every `tasks` column plus the joined
/// repository identity, aliased to the `repo_*` fields of [`TaskRow`].
const TASK_SELECT: &str = "SELECT t.*, r.owner AS repo_owner, r.name AS repo_name, \
     r.default_branch AS repo_default_branch \
     FROM tasks t LEFT JOIN repositories r ON r.id = t.repository_id";

/// Fields needed to create a task from a webhook event.
pub struct NewTask {
    pub repository_id: i64,
    pub installation_id: i64,
    pub webhook_delivery_id: String,
    pub target_type: String,
    pub target_id: i64,
    pub command_text: String,
    pub base_sha: Option<String>,
    pub head_sha: Option<String>,
    /// Re-run discriminator (RFC-0001), used by the content-idempotent [`create_task`] path (the
    /// automatic first review uses `0`). The explicit `@mention` path goes through
    /// [`create_explicit_task`], which computes the epoch inside the INSERT and ignores this field.
    pub run_epoch: i32,
    /// Review tier (ADR-0062): `"fast"` (automatic `pull_request opened` — SAST + one diff-only LLM
    /// turn, no retrieval) or `"deep"` (`@mention` — full retrieval, multi-turn). The runner reads it
    /// from the task context. Index tasks don't set it (the column defaults to `deep`, ignored).
    pub tier: String,
    /// GitHub id of the `@mention` comment that triggered this task (ADR-0068), so the lifecycle
    /// reactions target the triggering comment. `None` for the automatic `pull_request opened` review
    /// (no trigger comment → the reactions land on the PR body) and for index tasks.
    pub trigger_comment_id: Option<i64>,
}

/// A task claimed by the dispatcher for execution (the subset needed to launch its Job).
#[derive(Debug, sqlx::FromRow)]
pub struct ClaimedTask {
    pub id: Uuid,
    pub repository_id: i64,
    pub installation_id: i64,
    pub target_type: String,
    pub target_id: i64,
    pub command_text: String,
    pub base_sha: Option<String>,
    pub head_sha: Option<String>,
    pub attempts: i32,
}

/// Insert or update a repository by its GitHub id; returns the local `repositories.id`.
/// `installation_id` is recorded when known (`Some`) and preserved otherwise (`COALESCE`), so the
/// index-on-approve path can mint a token for it. Status is never touched here (the approval gate
/// owns it).
pub async fn upsert_repository(
    pool: &PgPool,
    platform: Platform,
    platform_repo_id: i64,
    owner: &str,
    name: &str,
    default_branch: &str,
    installation_id: Option<i64>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO repositories (platform, platform_repo_id, owner, name, default_branch, installation_id) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (platform, platform_repo_id) DO UPDATE \
           SET owner = EXCLUDED.owner, name = EXCLUDED.name, \
               default_branch = EXCLUDED.default_branch, \
               installation_id = COALESCE(EXCLUDED.installation_id, repositories.installation_id) \
         RETURNING id",
    )
    .bind(platform)
    .bind(platform_repo_id)
    .bind(owner)
    .bind(name)
    .bind(default_branch)
    .bind(installation_id)
    .fetch_one(pool)
    .await
}

/// Initial-status SQL for a newly inserted task (ADR-0055 index-readiness gate). A **non-`index`**
/// task (review / ask — anything that reads the index) starts `waiting_for_index` when an `index` task
/// is **in flight** for the repo, so it isn't claimed against a half-written snapshot; otherwise it
/// starts `queued`. Index tasks themselves never wait. Spliced where the bound params are
/// `$2 = repository_id` and `$7 = command_text` (true for both [`create_task`] and
/// [`create_explicit_task`]); a `set`-time `EXISTS` is fine — the dispatcher and the
/// release in [`set_task_status`] handle the (repo, index) lifecycle, not this insert.
const INITIAL_TASK_STATUS_SQL: &str = "CASE WHEN $7 <> 'index' AND EXISTS ( \
        SELECT 1 FROM tasks ix WHERE ix.repository_id = $2 AND ix.command_text = 'index' \
          AND ix.status IN ('queued', 'running', 'posting_result', 'waiting_for_index')) \
     THEN 'waiting_for_index' ELSE 'queued' END";

/// After inserting a task: wake a listening dispatcher when it's claimable (`queued`), or log that it
/// was parked behind an in-flight index (`waiting_for_index`, ADR-0055). A waiting task is woken later
/// by [`set_task_status`] when the repo's index task completes — notifying now would be a no-op (the
/// claim query only selects `queued`).
async fn notify_or_log_initial_status(pool: &PgPool, id: Uuid, repository_id: i64, status: &str) {
    if status == "waiting_for_index" {
        tracing::info!(
            task_id = %id, repository_id,
            "review gated: WaitingForIndex — an index task is in flight; will run once it completes (ADR-0055)"
        );
        return;
    }
    // Wake a listening dispatcher; harmless if none is connected (the dispatcher also polls).
    let _ = sqlx::query("SELECT pg_notify($1, $2)")
        .bind(TASK_QUEUED_CHANNEL)
        .bind(id.to_string())
        .execute(pool)
        .await;
}

/// Enqueue a task idempotently. Returns the new task id, or `None` when an equivalent task already
/// exists — GitHub can deliver several events for one PR head (e.g. `opened` then `synchronize`),
/// and the `tasks_idempotency_idx` unique index collapses those to a single task. On a real insert,
/// the task starts `queued` (→ notifies [`TASK_QUEUED_CHANNEL`]) or `waiting_for_index` when the repo
/// is mid-index (ADR-0055).
pub async fn create_task(pool: &PgPool, task: &NewTask) -> Result<Option<Uuid>, sqlx::Error> {
    let id = Uuid::new_v4();
    let inserted: Option<(Uuid, String)> = sqlx::query_as(&format!(
        "INSERT INTO tasks (id, repository_id, installation_id, webhook_delivery_id, target_type, \
         target_id, command_text, base_sha, head_sha, run_epoch, tier, trigger_comment_id, status) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, {INITIAL_TASK_STATUS_SQL}) \
         ON CONFLICT (repository_id, target_type, target_id, command_text, head_sha, run_epoch) \
         DO NOTHING \
         RETURNING id, status"
    ))
    .bind(id)
    .bind(task.repository_id)
    .bind(task.installation_id)
    .bind(&task.webhook_delivery_id)
    .bind(&task.target_type)
    .bind(task.target_id)
    .bind(&task.command_text)
    .bind(&task.base_sha)
    .bind(&task.head_sha)
    .bind(task.run_epoch)
    .bind(&task.tier)
    .bind(task.trigger_comment_id)
    .fetch_optional(pool)
    .await?;

    if let Some((new_id, status)) = &inserted {
        notify_or_log_initial_status(pool, *new_id, task.repository_id, status).await;
    }
    Ok(inserted.map(|(id, _)| id))
}

/// Enqueue an **explicit human command** (an `@mention`), which must ALWAYS land a task — never
/// content-deduped. True webhook redeliveries are already collapsed upstream by the
/// `webhook_deliveries` PRIMARY KEY, so content-idempotency on this path adds nothing and only drops
/// legitimate re-requests.
///
/// The `run_epoch` is folded into the INSERT — `COALESCE(MAX(run_epoch), -1) + 1` over the SAME
/// columns as the idempotency index (minus `run_epoch`), using the REAL `command_text` — so it is
/// computed and consumed in a single statement.
///
/// That subquery+insert is NOT atomic under READ COMMITTED: two near-simultaneous deliveries for the
/// same natural key can each read the same `MAX` and collide on `tasks_idempotency_idx` (`23505`).
/// We **retry** on that unique violation (a fresh `MAX` is computed each attempt, so the loser just
/// lands the next epoch) — bounded, so a genuinely persistent conflict can't spin forever. This keeps
/// the "an explicit mention always lands a task" guarantee even under concurrency. On insert, notifies
/// [`TASK_QUEUED_CHANNEL`] like [`create_task`].
pub async fn create_explicit_task(pool: &PgPool, task: &NewTask) -> Result<Uuid, sqlx::Error> {
    const MAX_ATTEMPTS: u32 = 5;
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let id = Uuid::new_v4();
        let result = sqlx::query_as::<_, (Uuid, String)>(&format!(
            "INSERT INTO tasks (id, repository_id, installation_id, webhook_delivery_id, target_type, \
             target_id, command_text, base_sha, head_sha, tier, trigger_comment_id, run_epoch, status) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, \
               (SELECT COALESCE(MAX(run_epoch), -1) + 1 FROM tasks \
                WHERE repository_id = $2 AND target_type = $5 AND target_id = $6 \
                  AND command_text = $7 AND head_sha IS NOT DISTINCT FROM $9), \
               {INITIAL_TASK_STATUS_SQL}) \
             RETURNING id, status"
        ))
        .bind(id)
        .bind(task.repository_id)
        .bind(task.installation_id)
        .bind(&task.webhook_delivery_id)
        .bind(&task.target_type)
        .bind(task.target_id)
        .bind(&task.command_text)
        .bind(&task.base_sha)
        .bind(&task.head_sha)
        .bind(&task.tier)
        .bind(task.trigger_comment_id)
        .fetch_one(pool)
        .await;
        match result {
            Ok((new_id, status)) => {
                notify_or_log_initial_status(pool, new_id, task.repository_id, &status).await;
                return Ok(new_id);
            }
            // Lost the epoch race — recompute MAX and try the next epoch.
            Err(sqlx::Error::Database(db))
                if db.code().as_deref() == Some("23505") && attempt < MAX_ATTEMPTS =>
            {
                tracing::debug!(attempt, "explicit-task epoch race (23505); retrying");
            }
            Err(e) => return Err(e),
        }
    }
}

/// Cancel a PR's active tasks (queued/running/posting_result) — used when the PR is closed so its
/// work stops. Returns the cancelled task ids. The agent Jobs of cancelled tasks are deleted by the
/// reaper (the control plane that serves webhooks has no Kubernetes client — trust boundary).
pub async fn cancel_active_tasks_for_pr(
    pool: &PgPool,
    repository_id: i64,
    pr: i64,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "UPDATE tasks SET status = 'cancelled', completed_at = now(), \
             lease_owner = NULL, lease_expires_at = NULL \
         WHERE repository_id = $1 AND target_type = 'pull_request' AND target_id = $2 \
           AND status IN ('queued', 'running', 'posting_result') \
         RETURNING id",
    )
    .bind(repository_id)
    .bind(pr)
    .fetch_all(pool)
    .await
}

/// Cancel every active (queued/running/posting) task for a repository — used when the repo is removed
/// from the installation or denied, so in-flight Jobs are stopped (the reaper deletes them) and
/// nothing new dispatches. Returns the cancelled task ids.
pub async fn cancel_active_tasks_for_repo(
    pool: &PgPool,
    repository_id: i64,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "UPDATE tasks SET status = 'cancelled', completed_at = now(), \
             lease_owner = NULL, lease_expires_at = NULL \
         WHERE repository_id = $1 AND status IN ('queued', 'running', 'posting_result') \
         RETURNING id",
    )
    .bind(repository_id)
    .fetch_all(pool)
    .await
}

/// Cancel a single task by id, if it's still active. Returns `true` when a row moved to `cancelled`
/// (false if the id is unknown or already terminal). Backs the manual "Cancel run" action; the
/// runner's self-cancel poll / the reaper then stop the Job + pod.
pub async fn cancel_task_by_id(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE tasks SET status = 'cancelled', completed_at = now(), \
             lease_owner = NULL, lease_expires_at = NULL \
         WHERE id = $1 \
           AND status IN ('received', 'waiting_for_index', 'queued', 'running', 'posting_result')",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// The `commit_sha` of the repository's **most recently indexed snapshot**, or `None` if it has never
/// been indexed (ADR-0050). This is the single anchor for review reuse: the runner skips the re-index
/// when this is `Some` and pins all retrieval (`search_code_chunks`, graph) to this same commit — so
/// the skip decision and the search scope always reference a commit that *provably has chunks*.
///
/// Why "latest", not the PR head: indexing is maintained on the default branch by re-index-on-push
/// (#183), and a review's value comes from the *base* repo context plus the PR diff (already in the
/// prompt). Pinning to the PR head (the previous behaviour) meant every new head commit looked
/// "not indexed" and triggered a full re-index per PR (slow + costly), while a repo-level "any rows?"
/// check meant searches at the head returned **zero** hits — a hollow index that starved the agent
/// (run `7c15f9bb`). Anchoring to the latest indexed snapshot fixes both: real hits, no per-PR re-index.
pub async fn latest_indexed_commit(
    pool: &PgPool,
    repository_id: i64,
) -> Result<Option<String>, sqlx::Error> {
    // `id DESC` tie-breaks when two snapshots share a `created_at` (coarse clock, or rows written in one
    // transaction where `now()` is constant) — `id` is BIGSERIAL, so the most-recently-inserted snapshot
    // wins deterministically. Backed by the `(repository_id, created_at DESC, id DESC)` index (migration
    // 0018) so this is an index lookup, not a scan — it runs on every search/graph query via `task_scope`.
    sqlx::query_scalar(
        "SELECT commit_sha FROM code_chunks WHERE repository_id = $1 \
         ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(repository_id)
    .fetch_optional(pool)
    .await
}

/// Delete a repository's semantic index (all `code_chunks` rows) — part of the data purge when a repo
/// is removed/denied (Epic #75, Milestone B). Returns the number of rows deleted.
pub async fn delete_code_chunks_for_repo(
    pool: &PgPool,
    repository_id: i64,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM code_chunks WHERE repository_id = $1")
        .bind(repository_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// Delete a repository's indexing-state rows (`repo_index`) — completes the purge bookkeeping so a
/// later re-add reindexes from scratch.
pub async fn delete_repo_index_rows(pool: &PgPool, repository_id: i64) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM repo_index WHERE repository_id = $1")
        .bind(repository_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

// ── Index snapshot pruning (RFC-0002, ADR-0052) ──────────────────────────────────────────────────
// Every default-branch push writes a full new `(repository_id, commit_sha)` snapshot into
// `code_chunks` (+ Neo4j) and nothing reaps the old ones — reviews only ever read the *latest*
// (`latest_indexed_commit`, ADR-0050). The index sweeper keeps only the in-use snapshots per repo and
// prunes the rest. These helpers are the Postgres half; the Neo4j half is `neo4j::prune_graph`.

/// Repos that currently hold MORE THAN ONE distinct `commit_sha` in `code_chunks` — i.e. the only
/// repos with anything prunable. The sweeper iterates these so a steady-state repo (one snapshot)
/// costs a single grouped count, not a per-repo delete.
pub async fn repos_with_stale_snapshots(pool: &PgPool) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT repository_id FROM code_chunks \
         GROUP BY repository_id HAVING count(DISTINCT commit_sha) > 1",
    )
    .fetch_all(pool)
    .await
}

/// Commits a still-running task pins, so the sweeper never prunes a snapshot out from under an
/// in-flight run. Non-terminal tasks that carry a `head_sha` (e.g. a review pinned to a PR head);
/// terminal statuses are excluded. `head_sha IS NOT NULL` since a null can't match a `commit_sha`.
/// NOTE: an `index` task carries a NULL `head_sha` (see [`create_index_task`]) so it is NOT covered
/// here — that case is handled by [`has_active_index_task`] (the sweeper skips a repo mid-index).
pub async fn in_use_commits(pool: &PgPool, repository_id: i64) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT DISTINCT head_sha FROM tasks \
         WHERE repository_id = $1 AND head_sha IS NOT NULL \
           AND status NOT IN ('succeeded', 'failed', 'timed_out', 'cancelled')",
    )
    .bind(repository_id)
    .fetch_all(pool)
    .await
}

/// Is an `index` task still in flight for this repo? An index task is the only thing that *writes* a
/// new snapshot, and it carries a NULL `head_sha` (see [`create_index_task`]) so [`in_use_commits`]
/// can't protect the commit it is mid-writing — and the Neo4j graph has NO recency grace (unlike
/// `code_chunks`). So the sweeper skips a repo entirely while an index runs and defers the prune one
/// cycle; deferring GC is harmless. Active = any non-terminal status.
pub async fn has_active_index_task(pool: &PgPool, repository_id: i64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS ( \
           SELECT 1 FROM tasks \
           WHERE repository_id = $1 AND command_text = 'index' \
             AND status NOT IN ('succeeded', 'failed', 'timed_out', 'cancelled'))",
    )
    .bind(repository_id)
    .fetch_one(pool)
    .await
}

/// Delete every `code_chunks` row for `repository_id` whose `commit_sha` is NOT in `keep`, except rows
/// indexed within the last 10 minutes (a recency grace: a just-finished index whose task hasn't yet
/// flipped to a terminal status, belt-and-suspenders on top of `in_use_commits`). Returns rows deleted.
/// No-op (returns 0) when `keep` is empty — never blindly delete a repo's whole index here.
pub async fn prune_code_chunks(
    pool: &PgPool,
    repository_id: i64,
    keep: &[String],
) -> Result<u64, sqlx::Error> {
    if keep.is_empty() {
        return Ok(0);
    }
    let result = sqlx::query(
        "DELETE FROM code_chunks \
         WHERE repository_id = $1 \
           AND commit_sha <> ALL($2) \
           AND created_at < now() - interval '10 minutes'",
    )
    .bind(repository_id)
    .bind(keep)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Disabled repositories that still have index data (leftover `code_chunks` or `repo_index` rows) —
/// the purge reconciler re-purges these so a cleanup lost to a control-plane restart still completes.
/// (Neo4j leftovers accompany `code_chunks`, so this also catches graph data to purge.)
pub async fn list_disabled_repos_needing_purge(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT r.id FROM repositories r \
         WHERE r.status = 'disabled' \
           AND (EXISTS (SELECT 1 FROM code_chunks c WHERE c.repository_id = r.id) \
                OR EXISTS (SELECT 1 FROM repo_index ri WHERE ri.repository_id = r.id)) \
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Cancelled tasks that still have a Kubernetes Job to clean up (the reaper deletes the Job, then
/// clears `job_name` so the row isn't returned again).
pub async fn list_cancelled_with_job(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<ReapableTask>, sqlx::Error> {
    sqlx::query_as::<_, ReapableTask>(
        "SELECT id, job_name, attempts FROM tasks \
         WHERE status = 'cancelled' AND job_name IS NOT NULL \
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Clear a task's `job_name` once its Job has been deleted (so the cleanup is idempotent).
pub async fn clear_job_name(pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE tasks SET job_name = NULL WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .map(|_| ())
}

/// Atomically claim the next due `queued` task and take a short dispatch lease. `FOR UPDATE SKIP
/// LOCKED` guarantees that concurrent dispatcher replicas never claim the same row. Returns `None`
/// when nothing is due. (Lease expiry is reaped by the scheduler in RFC-0001 Phase 2.)
pub async fn claim_next_task(
    pool: &PgPool,
    owner: &str,
    lease: Duration,
) -> Result<Option<ClaimedTask>, sqlx::Error> {
    sqlx::query_as::<_, ClaimedTask>(
        "UPDATE tasks \
         SET status = 'running', attempts = attempts + 1, started_at = now(), \
             lease_owner = $1, lease_expires_at = now() + ($2 * interval '1 second') \
         WHERE id = ( \
           SELECT id FROM tasks \
           WHERE status = 'queued' AND run_after <= now() \
           ORDER BY priority DESC, created_at \
           FOR UPDATE SKIP LOCKED \
           LIMIT 1 \
         ) \
         RETURNING id, repository_id, installation_id, target_type, target_id, command_text, \
                   base_sha, head_sha, attempts",
    )
    .bind(owner)
    .bind(lease.as_secs_f64())
    .fetch_optional(pool)
    .await
}

/// Record the Kubernetes Job created for a dispatched task.
pub async fn set_task_job(pool: &PgPool, id: Uuid, job_name: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE tasks SET job_name = $2 WHERE id = $1")
        .bind(id)
        .bind(job_name)
        .execute(pool)
        .await
        .map(|_| ())
}

/// Return a `running` task to the queue with a backoff delay — used both when Job creation fails and
/// when the reaper requeues a stuck task. Clears the lease, `started_at`, and `job_name` so the next
/// claim is clean and the next dispatch creates a fresh Job (the Job name is derived from the task
/// id, so a stale name would otherwise collide). Guarded on the active statuses so two reapers can't
/// both requeue the same task. Returns `true` if a row was actually requeued.
pub async fn release_task(pool: &PgPool, id: Uuid, backoff: Duration) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE tasks \
         SET status = 'queued', lease_owner = NULL, lease_expires_at = NULL, started_at = NULL, \
             job_name = NULL, run_after = now() + ($2 * interval '1 second') \
         WHERE id = $1 AND status IN ('running', 'posting_result')",
    )
    .bind(id)
    .bind(backoff.as_secs_f64())
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// A `running` task whose claim lease has expired — a candidate the reaper reconciles against its
/// Job's real liveness (RFC-0001 Phase 2).
#[derive(Debug, sqlx::FromRow)]
pub struct ReapableTask {
    pub id: Uuid,
    pub job_name: Option<String>,
    pub attempts: i32,
}

/// Tasks stuck in an active status (`running`/`posting_result`) past their lease — the lease is set
/// short at claim and renewed by the reaper only while the Job is live, so an expired lease just
/// means "needs reconciling", not "dead" — the caller decides by checking each Job's liveness.
/// Bounded so one cycle is cheap (backed by the `tasks_reapable_idx` partial index).
pub async fn list_reapable_tasks(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<ReapableTask>, sqlx::Error> {
    sqlx::query_as::<_, ReapableTask>(
        "SELECT id, job_name, attempts FROM tasks \
         WHERE status IN ('running', 'posting_result') AND lease_expires_at < now() \
         ORDER BY started_at NULLS FIRST \
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Extend an active task's lease — the reaper's heartbeat for a Job it confirmed is still live, so a
/// long-running task isn't reclaimed. No-op (returns `false`) if the task is no longer active.
pub async fn renew_lease(pool: &PgPool, id: Uuid, lease: Duration) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE tasks SET lease_expires_at = now() + ($2 * interval '1 second') \
         WHERE id = $1 AND status IN ('running', 'posting_result')",
    )
    .bind(id)
    .bind(lease.as_secs_f64())
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Most recent tasks first (the dashboard run list).
pub async fn list_tasks(pool: &PgPool, limit: i64) -> Result<Vec<TaskRow>, sqlx::Error> {
    let sql = format!("{TASK_SELECT} ORDER BY t.created_at DESC LIMIT $1");
    sqlx::query_as::<_, TaskRow>(&sql)
        .bind(limit)
        .fetch_all(pool)
        .await
}

/// A single task by id.
pub async fn get_task(pool: &PgPool, id: Uuid) -> Result<Option<TaskRow>, sqlx::Error> {
    let sql = format!("{TASK_SELECT} WHERE t.id = $1");
    sqlx::query_as::<_, TaskRow>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// A connected repository for the dashboard's Repositories view (ADR-0016), with a small activity
/// summary (run count + most-recent run) derived from `tasks`. RepoIndex health is not joined yet —
/// the indexer that populates `repo_index` is a later step in the Code product epic.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct RepositoryRow {
    pub id: i64,
    pub platform_repo_id: i64,
    pub platform: Platform,
    pub owner: String,
    pub name: String,
    pub default_branch: String,
    /// Approval gate (Epic #75): `pending` | `approved` | `disabled`. `active` mirrors
    /// `status = 'approved'` for the existing dashboard; `status` is the source of truth.
    pub status: String,
    pub active: bool,
    #[serde(with = "time::serde::rfc3339::option")]
    pub approved_at: Option<OffsetDateTime>,
    pub approved_by: Option<String>,
    pub task_count: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_task_at: Option<OffsetDateTime>,
}

/// Connected repositories, most-recently-active first, optionally filtered by approval `status`
/// (e.g. `Some("pending")` for the admin approval queue). Aggregates each repo's task activity in one
/// query so the list needs no per-row round-trip.
pub async fn list_repositories(
    pool: &PgPool,
    status: Option<&str>,
) -> Result<Vec<RepositoryRow>, sqlx::Error> {
    sqlx::query_as::<_, RepositoryRow>(
        "SELECT r.id, r.platform_repo_id, r.platform, r.owner, r.name, r.default_branch, r.status, \
           (r.status = 'approved') AS active, r.approved_at, r.approved_by, \
           COUNT(t.id) AS task_count, MAX(t.created_at) AS last_task_at \
         FROM repositories r LEFT JOIN tasks t ON t.repository_id = r.id \
         WHERE ($1::text IS NULL OR r.status = $1) \
         GROUP BY r.id \
         ORDER BY last_task_at DESC NULLS LAST, r.owner, r.name",
    )
    .bind(status)
    .fetch_all(pool)
    .await
}

/// Register a repository seen via an `installation` / `installation_repositories` webhook as
/// **pending** approval. New repo → inserted pending. A previously **disabled** repo (uninstalled
/// then re-added) is re-opened to pending so the admin sees it in the queue again; an already
/// `approved`/`pending` repo is left untouched (the `WHERE` guard), preserving its status and real
/// `default_branch`. The installation payload carries no `default_branch`, so a placeholder is fine —
/// the first PR webhook fills it in. Returns `true` when a row was inserted or re-pended.
pub async fn register_pending_repository(
    pool: &PgPool,
    platform: Platform,
    platform_repo_id: i64,
    owner: &str,
    name: &str,
    default_branch: &str,
    installation_id: Option<i64>,
) -> Result<bool, sqlx::Error> {
    let affected = sqlx::query(
        "INSERT INTO repositories (platform, platform_repo_id, owner, name, default_branch, installation_id, status) \
         VALUES ($1, $2, $3, $4, $5, $6, 'pending') \
         ON CONFLICT (platform, platform_repo_id) DO UPDATE \
           SET status = 'pending', owner = EXCLUDED.owner, name = EXCLUDED.name, \
               installation_id = COALESCE(EXCLUDED.installation_id, repositories.installation_id) \
           WHERE repositories.status = 'disabled'",
    )
    .bind(platform)
    .bind(platform_repo_id)
    .bind(owner)
    .bind(name)
    .bind(default_branch)
    .bind(installation_id)
    .execute(pool)
    .await?;
    Ok(affected.rows_affected() > 0)
}

/// A repository's approval status (`pending`/`approved`/`disabled`), or `None` if no such repo.
pub async fn repository_status(
    pool: &PgPool,
    repository_id: i64,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT status FROM repositories WHERE id = $1")
        .bind(repository_id)
        .fetch_optional(pool)
        .await
}

/// Is this repository approved for work? The gate the webhook handlers check before creating any
/// review/index task. A missing repo (shouldn't happen — callers upsert first) reads as not approved.
pub async fn repository_approved(pool: &PgPool, repository_id: i64) -> Result<bool, sqlx::Error> {
    Ok(repository_status(pool, repository_id).await?.as_deref() == Some("approved"))
}

/// Persist a repository's resolved default branch (e.g. fetched at approval time for a repo first
/// seen via an installation webhook, which doesn't carry it).
pub async fn update_repository_default_branch(
    pool: &PgPool,
    repository_id: i64,
    default_branch: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE repositories SET default_branch = $2 WHERE id = $1")
        .bind(repository_id)
        .bind(default_branch)
        .execute(pool)
        .await
        .map(|_| ())
}

/// The repository's GitHub `installation_id` (for minting a clone token), or `None` if not recorded.
pub async fn repository_installation_id(
    pool: &PgPool,
    repository_id: i64,
) -> Result<Option<i64>, sqlx::Error> {
    let id: Option<Option<i64>> =
        sqlx::query_scalar("SELECT installation_id FROM repositories WHERE id = $1")
            .bind(repository_id)
            .fetch_optional(pool)
            .await?;
    Ok(id.flatten())
}

/// A repository resolved by its platform + `owner/name` slug for an A2A review submission
/// (RFC-0006). Carries just what the submission path needs to enforce the approval gate and build
/// the review task **without a forge round-trip** (the `a2a` role holds no forge credentials).
#[derive(Debug, sqlx::FromRow)]
pub struct RepoForReview {
    pub id: i64,
    /// GitHub App installation id (or GitLab project id). `None` for a repo seen but never carried
    /// through a PR/installation webhook that recorded it — the A2A path treats that as not runnable.
    pub installation_id: Option<i64>,
    /// Approval status: `pending` | `approved` | `disabled`. The A2A path rejects anything but
    /// `approved` (the same gate as the webhook path, ADR-0063).
    pub status: String,
}

/// Resolve an approved-or-not repository by platform + `owner`/`name`. `None` when no such repo has
/// ever been connected (an A2A review of a never-seen repo → `TASK_STATE_REJECTED`, never a side
/// door around the approval gate). Case-insensitive on owner/name to match forge slug behaviour.
pub async fn find_repository(
    pool: &PgPool,
    platform: Platform,
    owner: &str,
    name: &str,
) -> Result<Option<RepoForReview>, sqlx::Error> {
    sqlx::query_as::<_, RepoForReview>(
        "SELECT id, installation_id, status FROM repositories \
         WHERE platform = $1 AND lower(owner) = lower($2) AND lower(name) = lower($3)",
    )
    .bind(platform)
    .bind(owner)
    .bind(name)
    .fetch_optional(pool)
    .await
}

/// Find the id of an already-existing task matching a [`NewTask`]'s idempotency tuple
/// (`repository_id, target_type, target_id, command_text, head_sha, run_epoch`) — the same columns as
/// `tasks_idempotency_idx`, `NULLS NOT DISTINCT` on `head_sha`. Used by the A2A review path so that
/// when [`create_task`] dedups (returns `None`), the A2A task can still be mapped onto the existing
/// underlying run instead of forking the idempotency logic.
pub async fn find_task_id_by_idempotency(
    pool: &PgPool,
    task: &NewTask,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT id FROM tasks \
         WHERE repository_id = $1 AND target_type = $2 AND target_id = $3 \
           AND command_text = $4 AND head_sha IS NOT DISTINCT FROM $5 AND run_epoch = $6 \
         ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(task.repository_id)
    .bind(&task.target_type)
    .bind(task.target_id)
    .bind(&task.command_text)
    .bind(&task.head_sha)
    .bind(task.run_epoch)
    .fetch_optional(pool)
    .await
}

/// Enqueue a standalone **index** task for a repository's default branch (Epic #75, Milestone B —
/// runs on admin approval, and on every default-branch push via `handle_push`). Skips if an index task
/// is already active for the repo (so a burst of pushes / a re-approve doesn't pile up duplicates).
/// Returns the new task id, or `None` if one was already pending/running. Unlike review tasks it has no
/// originating delivery (`webhook_delivery_id` NULL) and no SHA (the runner indexes the default-branch
/// HEAD).
///
/// `run_epoch` is computed as `MAX+1` over the same columns as `tasks_idempotency_idx` (minus
/// `run_epoch`), exactly like [`create_explicit_task`]. This is **load-bearing**: an index task carries
/// a NULL `head_sha`, so every re-index of a repo shares the idempotency tuple
/// `(repo, 'repository', repo, 'index', NULL, run_epoch)` — and `tasks_idempotency_idx` is
/// `NULLS NOT DISTINCT`. Hardcoding `run_epoch = 0` (the original bug) meant the *second* index for a
/// repo passed the `NOT EXISTS` active-guard (the first index was terminal, not active) but then
/// **collided on the unique index** → `duplicate key` error → a repo was only ever indexed once.
pub async fn create_index_task(
    pool: &PgPool,
    repository_id: i64,
    installation_id: i64,
) -> Result<Option<Uuid>, sqlx::Error> {
    let id = Uuid::new_v4();
    let inserted = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO tasks (id, repository_id, installation_id, target_type, target_id, \
         command_text, run_epoch, status) \
         SELECT $1, $2, $3, 'repository', $2, 'index', \
           (SELECT COALESCE(MAX(run_epoch), -1) + 1 FROM tasks \
              WHERE repository_id = $2 AND target_type = 'repository' AND target_id = $2 \
                AND command_text = 'index' AND head_sha IS NULL), \
           'queued' \
         WHERE NOT EXISTS ( \
           SELECT 1 FROM tasks WHERE repository_id = $2 AND command_text = 'index' \
             AND status IN ('queued', 'running', 'posting_result') \
         ) \
         RETURNING id",
    )
    .bind(id)
    .bind(repository_id)
    .bind(installation_id)
    .fetch_optional(pool)
    .await;

    // A concurrent push that cleared the `NOT EXISTS` guard at the same instant can race us to the same
    // computed epoch and trip `tasks_idempotency_idx` — that's a benign dedup (the other push queued the
    // index), not an error to surface.
    let inserted = match inserted {
        Ok(v) => v,
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => None,
        Err(e) => return Err(e),
    };

    if let Some(new_id) = inserted {
        let _ = sqlx::query("SELECT pg_notify($1, $2)")
            .bind(TASK_QUEUED_CHANNEL)
            .bind(new_id.to_string())
            .execute(pool)
            .await;
    }
    Ok(inserted)
}

/// Set a repository's approval status by its **GitHub** id (webhook path — e.g. mark `disabled` when
/// removed from the installation). Returns the repo's **local** id (so the caller can purge its index
/// data), or `None` if the repo isn't known locally.
pub async fn set_repository_status_by_platform_id(
    pool: &PgPool,
    platform: Platform,
    platform_repo_id: i64,
    status: &str,
) -> Result<Option<i64>, sqlx::Error> {
    // Clear the approval audit on any non-approved transition (e.g. disable) so stale approver/time
    // don't linger — mirrors `set_repository_status_by_id`.
    sqlx::query_scalar(
        "UPDATE repositories SET status = $3, \
           approved_at = CASE WHEN $3 = 'approved' THEN approved_at ELSE NULL END, \
           approved_by = CASE WHEN $3 = 'approved' THEN approved_by ELSE NULL END \
         WHERE platform_repo_id = $2 AND platform = $1 \
         RETURNING id",
    )
    .bind(platform)
    .bind(platform_repo_id)
    .bind(status)
    .fetch_optional(pool)
    .await
}

/// Admin action: set a repository's approval status by its **local** id, recording who/when on
/// approval. Returns the updated row, or `None` if no such repo. `approved_by` is the admin's
/// identity (OIDC subject/username); cleared for non-approved transitions.
pub async fn set_repository_status_by_id(
    pool: &PgPool,
    id: i64,
    status: &str,
    approved_by: Option<&str>,
) -> Result<Option<RepositoryRow>, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE repositories SET status = $2, \
           approved_at = CASE WHEN $2 = 'approved' THEN now() ELSE NULL END, \
           approved_by = CASE WHEN $2 = 'approved' THEN $3 ELSE NULL END \
         WHERE id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(approved_by)
    .execute(pool)
    .await?;
    if updated.rows_affected() == 0 {
        return Ok(None);
    }
    sqlx::query_as::<_, RepositoryRow>(
        "SELECT r.id, r.platform_repo_id, r.platform, r.owner, r.name, r.default_branch, r.status, \
           (r.status = 'approved') AS active, r.approved_at, r.approved_by, \
           COUNT(t.id) AS task_count, MAX(t.created_at) AS last_task_at \
         FROM repositories r LEFT JOIN tasks t ON t.repository_id = r.id \
         WHERE r.id = $1 GROUP BY r.id",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// Everything the agent runner needs to act on a task, joined with its repository identity. Served
/// by the internal runner API (`GET /internal/tasks/{id}`) so the runner never holds the GitHub App
/// key — it receives repo coordinates here and a freshly-minted installation token alongside (the
/// control plane mints it; see `internal.rs`). `installation_id` is kept server-side for that.
#[derive(Debug, sqlx::FromRow)]
pub struct TaskContextRow {
    pub id: Uuid,
    pub repository_id: i64,
    pub installation_id: i64,
    pub owner: String,
    pub name: String,
    pub default_branch: String,
    pub platform: Platform,
    pub target_type: String,
    pub target_id: i64,
    pub command_text: String,
    /// Run kind (ADR-0033): `review` or `ask`. The runner branches on this — a `review` produces
    /// diff-scoped findings, an `ask` produces a conversational answer.
    pub kind: String,
    /// Review tier (ADR-0062): `fast` (auto PR-opened — SAST + one diff-only LLM turn, no retrieval) or
    /// `deep` (`@mention` — full retrieval, multi-turn). The runner branches on this for the loop shape.
    pub tier: String,
    pub base_sha: Option<String>,
    pub head_sha: Option<String>,
    /// The `@mention` comment that triggered this task (ADR-0068), or `None` for the automatic
    /// `pull_request opened` review. When `Some`, the lifecycle reactions target this comment.
    pub trigger_comment_id: Option<i64>,
}

/// Load a task's execution context, or `None` if no such task exists. INNER JOIN on `repositories`:
/// a task always references a repository (FK), so a missing row means a bad/expired id.
pub async fn get_task_context(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<TaskContextRow>, sqlx::Error> {
    sqlx::query_as::<_, TaskContextRow>(
        "SELECT t.id, t.repository_id, t.installation_id, r.owner, r.name, r.default_branch, r.platform, \
                t.target_type, t.target_id, t.command_text, t.kind, t.tier, t.base_sha, t.head_sha, \
                t.trigger_comment_id \
         FROM tasks t JOIN repositories r ON r.id = t.repository_id \
         WHERE t.id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// Statuses the runner is allowed to report. Transitioning into a terminal one stamps
/// `completed_at` and releases the lease; `running` (re)stamps `started_at`. Anything else is
/// rejected by the handler before reaching here.
pub fn is_runner_reportable_status(status: &str) -> bool {
    matches!(
        status,
        "running" | "posting_result" | "succeeded" | "failed" | "timed_out" | "cancelled"
    )
}

/// The task's current status string, or `None` if the row is gone. Lightweight (no token mint) —
/// the runner polls this to self-cancel promptly when its task is cancelled, independent of the
/// reaper (which may be down mid-deploy).
pub async fn get_task_status(pool: &PgPool, id: Uuid) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT status FROM tasks WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Apply a runner-reported status transition. Terminal states (`succeeded`/`failed`/`timed_out`/
/// `cancelled`) stamp `completed_at` and clear the dispatcher lease so the reaper (Phase 2) won't
/// reclaim a finished task; `running` stamps `started_at` if unset. Returns `false` if no task
/// matched the id. The caller validates `status` with [`is_runner_reportable_status`] first.
///
/// `detail` is the runner's free-text status reason (#137): when `Some` it is persisted to
/// `error_detail` so the console can surface why a run did not post a review (and tell a silent no-op
/// apart from a real clean success). `None` leaves any existing `error_detail` untouched — a later
/// report without a detail must not erase a reason an earlier one recorded.
/// When an `index` task reaches a terminal status, flip the repo's `waiting_for_index` tasks — parked
/// by the ADR-0055 enqueue gate — to `queued` and wake the dispatcher. A no-op when the completed task
/// is **not** an index task (the `done` CTE is empty), so [`set_task_status`] can call it on every
/// terminal transition. Fires on ANY terminal status (including `failed`/`cancelled`) so a failed index
/// never strands reviews forever — the common `succeeded` case is the one that grounds them.
async fn release_reviews_waiting_on_index(pool: &PgPool, index_task_id: Uuid) {
    let released: Vec<Uuid> = match sqlx::query_scalar(
        "WITH done AS (SELECT repository_id FROM tasks WHERE id = $1 AND command_text = 'index') \
         UPDATE tasks SET status = 'queued', run_after = now() \
         WHERE repository_id IN (SELECT repository_id FROM done) \
           AND status = 'waiting_for_index' \
         RETURNING id",
    )
    .bind(index_task_id)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        // Do NOT swallow this: a failed release leaves the repo's reviews parked in
        // `waiting_for_index`, which the claim query never selects — they only recover when a *later*
        // index task completes. Log loudly so the stall is visible and an operator can requeue (#214
        // review). The status stamp already succeeded, so we don't fail the caller.
        Err(error) => {
            tracing::error!(
                %error, index_task_id = %index_task_id,
                "ADR-0055: releasing reviews waiting on the index FAILED — they stay parked until the \
                 next index completes; a manual requeue may be needed"
            );
            return;
        }
    };
    if released.is_empty() {
        return;
    }
    tracing::info!(
        index_task_id = %index_task_id,
        released = released.len(),
        "index complete: released tasks that were waiting for the index (ADR-0055)"
    );
    // One NOTIFY is enough — the dispatcher drains every `queued` task on any wake.
    let _ = sqlx::query("SELECT pg_notify($1, $2)")
        .bind(TASK_QUEUED_CHANNEL)
        .bind(index_task_id.to_string())
        .execute(pool)
        .await;
}

pub async fn set_task_status(
    pool: &PgPool,
    id: Uuid,
    status: &str,
    detail: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let terminal = matches!(status, "succeeded" | "failed" | "timed_out" | "cancelled");
    let result = sqlx::query(
        "UPDATE tasks SET \
             status = $2, \
             started_at = CASE WHEN $2 = 'running' THEN COALESCE(started_at, now()) ELSE started_at END, \
             completed_at = CASE WHEN $3 THEN now() ELSE completed_at END, \
             lease_owner = CASE WHEN $3 THEN NULL ELSE lease_owner END, \
             lease_expires_at = CASE WHEN $3 THEN NULL ELSE lease_expires_at END, \
             error_detail = CASE WHEN $2 = 'running' THEN NULL ELSE COALESCE($4, error_detail) END \
         WHERE id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(terminal)
    .bind(detail)
    .execute(pool)
    .await?;
    // ADR-0055: a completed index task releases the repo's reviews that were parked behind it.
    if terminal {
        release_reviews_waiting_on_index(pool, id).await;
    }
    Ok(result.rows_affected() > 0)
}

/// A semantic chunk submitted by the indexer runner (epic #5, slice 2).
pub struct CodeChunk {
    pub file_path: String,
    pub language: String,
    pub chunk_type: String,
    pub symbol_name: Option<String>,
    pub start_line: i32,
    pub end_line: i32,
    pub content: String,
    /// Embedding vector. Its length must match the `code_chunks.embedding` column — 4096 for
    /// `qwen3-embedding-8b` (migration 0005), configurable per deployment (ADR-0018).
    pub embedding: Vec<f32>,
}

/// Upsert a batch of code chunks for a repository snapshot. The embedding is passed as a Postgres
/// vector literal so no extra crate is needed; `$N::vector` casts the text on the server side.
/// Runs in a single transaction; returns the number of rows inserted or updated.
pub async fn upsert_code_chunks(
    pool: &PgPool,
    repository_id: i64,
    commit_sha: &str,
    chunks: &[CodeChunk],
) -> anyhow::Result<usize> {
    use anyhow::Context;
    let mut tx = pool.begin().await.context("begin upsert transaction")?;
    let mut count = 0usize;
    for chunk in chunks {
        let emb = vector_literal(&chunk.embedding);
        sqlx::query(
            "INSERT INTO code_chunks \
             (repository_id, commit_sha, file_path, language, chunk_type, symbol_name, \
              start_line, end_line, content, embedding) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::vector) \
             ON CONFLICT (repository_id, commit_sha, file_path, start_line, end_line) \
             DO UPDATE SET \
               language    = EXCLUDED.language, \
               chunk_type  = EXCLUDED.chunk_type, \
               symbol_name = EXCLUDED.symbol_name, \
               content     = EXCLUDED.content, \
               embedding   = EXCLUDED.embedding",
        )
        .bind(repository_id)
        .bind(commit_sha)
        .bind(&chunk.file_path)
        .bind(&chunk.language)
        .bind(&chunk.chunk_type)
        .bind(&chunk.symbol_name)
        .bind(chunk.start_line)
        .bind(chunk.end_line)
        .bind(&chunk.content)
        .bind(&emb)
        .execute(&mut *tx)
        .await
        .context("upsert code_chunks row")?;
        count += 1;
    }
    tx.commit().await.context("commit upsert transaction")?;
    Ok(count)
}

/// Render a float slice as a pgvector text literal `[f0,f1,…]` in one pre-allocated buffer
/// (`$N::vector` casts it server-side, so no extra crate is needed).
fn vector_literal(v: &[f32]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(v.len() * 12 + 2);
    s.push('[');
    for (i, f) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "{f}");
    }
    s.push(']');
    s
}

/// One semantic-search hit (a `code_chunks` row + its similarity score). Serialized straight to the
/// retrieval API the vector MCP server calls.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct CodeChunkHit {
    pub file_path: String,
    pub language: String,
    pub chunk_type: String,
    pub symbol_name: Option<String>,
    pub start_line: i32,
    pub end_line: i32,
    pub content: String,
    /// Cosine similarity in `[0,1]` (`1 - cosine_distance`); higher is closer.
    pub score: f64,
}

/// Semantic search: the `limit` nearest chunks to `query_embedding` within one repo snapshot,
/// by cosine distance (an exact scan — the 4096-dim column exceeds pgvector's ANN limit, so
/// migration 0005 carries no index). Scoped by `(repository_id, commit_sha)` so
/// a task only ever sees its own repo's index — the caller never picks the scope (trust boundary).
pub async fn search_code_chunks(
    pool: &PgPool,
    repository_id: i64,
    commit_sha: &str,
    query_embedding: &[f32],
    limit: i64,
) -> Result<Vec<CodeChunkHit>, sqlx::Error> {
    let emb = vector_literal(query_embedding);
    sqlx::query_as::<_, CodeChunkHit>(
        "SELECT file_path, language, chunk_type, symbol_name, start_line, end_line, content, \
                1.0 - (embedding <=> $1::vector) AS score \
         FROM code_chunks \
         WHERE repository_id = $2 AND commit_sha = $3 \
         ORDER BY embedding <=> $1::vector \
         LIMIT $4",
    )
    .bind(&emb)
    .bind(repository_id)
    .bind(commit_sha)
    .bind(limit)
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Integration tests: `#[sqlx::test]` provisions a fresh database, runs the migrations, and hands
    // us a pool. Requires a reachable Postgres via `DATABASE_URL` (see `compose.yaml`); skipped when
    // none is configured (CI builds images but runs no Rust test job today).

    /// The dedup contract that lets the control plane run multiple replicas: the `delivery_id`
    /// PRIMARY KEY + `ON CONFLICT DO NOTHING` means a replayed GitHub delivery is detected as a
    /// duplicate (GitHub delivers at least once), and the row is written exactly once.
    #[sqlx::test]
    async fn record_delivery_dedupes_on_delivery_id(pool: PgPool) {
        let payload = json!({ "action": "opened" });

        let first = record_delivery(
            &pool,
            Platform::GitHub,
            "delivery-abc",
            "pull_request",
            &payload,
        )
        .await
        .unwrap();
        assert!(first, "first delivery is new");

        let replay = record_delivery(
            &pool,
            Platform::GitHub,
            "delivery-abc",
            "pull_request",
            &payload,
        )
        .await
        .unwrap();
        assert!(!replay, "replayed delivery id is a duplicate");

        let other = record_delivery(&pool, Platform::GitHub, "delivery-xyz", "push", &payload)
            .await
            .unwrap();
        assert!(other, "a different delivery id is independent");

        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM webhook_deliveries WHERE delivery_id = $1")
                .bind("delivery-abc")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 1, "the replayed delivery is stored exactly once");
    }

    /// Seed the FK rows a task needs (one repository + one delivery); returns the repository id.
    async fn seed(pool: &PgPool) -> i64 {
        let repo_id = upsert_repository(pool, Platform::GitHub, 1, "octo", "repo", "main", None)
            .await
            .unwrap();
        record_delivery(pool, Platform::GitHub, "d1", "pull_request", &json!({}))
            .await
            .unwrap();
        repo_id
    }

    fn pr_task(repository_id: i64, head: &str) -> NewTask {
        NewTask {
            repository_id,
            installation_id: 99,
            webhook_delivery_id: "d1".to_string(),
            target_type: "pull_request".to_string(),
            target_id: 7,
            command_text: "review".to_string(),
            base_sha: Some("base".to_string()),
            head_sha: Some(head.to_string()),
            run_epoch: 0,
            tier: "fast".to_string(),
            trigger_comment_id: None,
        }
    }

    async fn task_status(pool: &PgPool, id: Uuid) -> String {
        sqlx::query_scalar::<_, String>("SELECT status FROM tasks WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// ADR-0055: a review enqueued while an `index` task is in flight parks in `waiting_for_index`
    /// (not `queued`), so the dispatcher's claim query never runs it against a half-built index.
    #[sqlx::test]
    async fn review_waits_for_index_when_an_index_is_in_flight(pool: PgPool) {
        let repo_id = seed(&pool).await;
        // An index task is queued = in flight.
        create_index_task(&pool, repo_id, 99)
            .await
            .unwrap()
            .expect("index task");
        // A review enqueued now must wait for it.
        let review = create_task(&pool, &pr_task(repo_id, "head1"))
            .await
            .unwrap()
            .expect("review");
        assert_eq!(task_status(&pool, review).await, "waiting_for_index");
        // It is NOT claimable (claim only takes `queued`) — the index task is what gets claimed.
        let claimed = claim_next_task(&pool, "w", std::time::Duration::from_secs(60))
            .await
            .unwrap()
            .expect("a claimable task");
        assert_ne!(claimed.id, review, "the waiting review must not be claimed");
        assert_eq!(claimed.command_text, "index");
    }

    /// ADR-0055: completing the `index` task releases the repo's waiting reviews to `queued`.
    #[sqlx::test]
    async fn index_completion_releases_waiting_reviews(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let index = create_index_task(&pool, repo_id, 99)
            .await
            .unwrap()
            .expect("index task");
        let review = create_task(&pool, &pr_task(repo_id, "head1"))
            .await
            .unwrap()
            .expect("review");
        assert_eq!(task_status(&pool, review).await, "waiting_for_index");

        set_task_status(&pool, index, "succeeded", None)
            .await
            .unwrap();
        assert_eq!(
            task_status(&pool, review).await,
            "queued",
            "the index completing releases the parked review"
        );
    }

    /// ADR-0055: with no index in flight, a review enqueues straight to `queued` (unchanged behaviour).
    #[sqlx::test]
    async fn review_is_queued_when_no_index_in_flight(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let review = create_task(&pool, &pr_task(repo_id, "head1"))
            .await
            .unwrap()
            .expect("review");
        assert_eq!(task_status(&pool, review).await, "queued");
    }

    /// ADR-0055: a FAILED index still releases waiting reviews — a failed index must never strand them.
    #[sqlx::test]
    async fn a_failed_index_still_releases_waiting_reviews(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let index = create_index_task(&pool, repo_id, 99)
            .await
            .unwrap()
            .expect("index task");
        let review = create_task(&pool, &pr_task(repo_id, "head1"))
            .await
            .unwrap()
            .expect("review");
        set_task_status(&pool, index, "failed", Some("boom"))
            .await
            .unwrap();
        assert_eq!(
            task_status(&pool, review).await,
            "queued",
            "a failed index still releases the parked review"
        );
    }

    /// ADR-0056: the failure-notice gate is false until the task has responded — then a posted review
    /// (a `reviews` row) OR any recorded comment (an inline finding, a reply, or a prior notice) flips it
    /// true, so a retry's failure never posts a second notice on top of real output.
    #[sqlx::test]
    async fn has_responded_reflects_posted_reviews_and_comments(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let task = create_task(&pool, &pr_task(repo_id, "head1"))
            .await
            .unwrap()
            .expect("task");
        // Nothing posted or queued → false (a failed run here SHOULD get a fallback notice).
        assert!(!has_responded_or_pending_content(&pool, task).await.unwrap());

        // A recorded comment (the failure notice itself, or an inline finding) flips it true.
        store_review_comments(
            &pool,
            task,
            &[ReviewCommentRef {
                platform_comment_id: 12345,
                kind: "failure_notice".to_string(),
                file: None,
                line: None,
            }],
        )
        .await
        .unwrap();
        assert!(has_responded_or_pending_content(&pool, task).await.unwrap());

        // A posted grouped review (a `reviews` row) also counts as responded.
        let task2 = create_task(&pool, &pr_task(repo_id, "head2"))
            .await
            .unwrap()
            .expect("task");
        assert!(!has_responded_or_pending_content(&pool, task2)
            .await
            .unwrap());
        upsert_review(
            &pool,
            task2,
            "summary",
            "body",
            0,
            0,
            0,
            &serde_json::json!([]),
            None,
            None,
        )
        .await
        .unwrap();
        assert!(has_responded_or_pending_content(&pool, task2)
            .await
            .unwrap());
    }

    /// ADR-0059: an enqueued intent is claimable in order, idempotent on `dedup_key`, and the
    /// posted/failed transitions move it out of (or back into, after backoff) the claim set.
    #[sqlx::test]
    async fn outbox_enqueue_claim_and_mark(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let task = create_task(&pool, &pr_task(repo_id, "h"))
            .await
            .unwrap()
            .expect("task");
        let payload = serde_json::json!({ "issue": 7, "content": "eyes" });

        // First enqueue inserts; a second with the same dedup_key is a no-op (idempotent).
        assert!(
            enqueue_outbox_post(
                &pool,
                Platform::GitHub,
                Some(task),
                99,
                "o",
                "r",
                "reaction",
                &payload,
                "k1"
            )
            .await
            .unwrap(),
            "first enqueue inserts"
        );
        assert!(
            !enqueue_outbox_post(
                &pool,
                Platform::GitHub,
                Some(task),
                99,
                "o",
                "r",
                "reaction",
                &payload,
                "k1"
            )
            .await
            .unwrap(),
            "duplicate dedup_key is a no-op"
        );

        // Claimable, carrying the coordinates to post with (no join needed).
        let batch = claim_outbox_batch(&pool, 10).await.unwrap();
        assert_eq!(batch.len(), 1);
        let row = &batch[0];
        assert_eq!((row.kind.as_str(), row.owner.as_str()), ("reaction", "o"));

        // Marking posted removes it from the claim set.
        mark_outbox_posted(&pool, row.id, Some(555)).await.unwrap();
        assert!(claim_outbox_batch(&pool, 10).await.unwrap().is_empty());

        // A failed delivery backs the row off into the future (not immediately re-claimable).
        enqueue_outbox_post(
            &pool,
            Platform::GitHub,
            Some(task),
            99,
            "o",
            "r",
            "reply",
            &payload,
            "k2",
        )
        .await
        .unwrap();
        let id = claim_outbox_batch(&pool, 10).await.unwrap()[0].id;
        mark_outbox_failed(&pool, id, "github 502").await.unwrap();
        assert!(
            claim_outbox_batch(&pool, 10).await.unwrap().is_empty(),
            "a just-failed row is backed off, not immediately re-claimable"
        );
    }

    /// RFC-0005 / ADR-0074 — the `PlatformEgress` virtual object's DB helpers: the status-guarded load,
    /// the id-by-dedup_key bridge, and the unconditional dead-letter. Verified against the same `outbox`
    /// row lifecycle the drain uses, so both egress paths agree on what "pending / settled" means.
    #[sqlx::test]
    async fn platform_egress_db_helpers(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let task = create_task(&pool, &pr_task(repo_id, "egress"))
            .await
            .unwrap()
            .expect("task");
        let payload = serde_json::json!({ "issue": 7, "content": "eyes" });
        enqueue_outbox_post(
            &pool,
            Platform::GitHub,
            Some(task),
            99,
            "o",
            "r",
            "reaction",
            &payload,
            "egress-k1",
        )
        .await
        .unwrap();

        // The producer bridge: resolve the row id from the dedup_key (what `announce` sends to Restate).
        let id = outbox_id_by_dedup_key(&pool, "egress-k1")
            .await
            .unwrap()
            .expect("a freshly-enqueued row is resolvable by dedup_key");
        assert!(
            outbox_id_by_dedup_key(&pool, "no-such-key")
                .await
                .unwrap()
                .is_none(),
            "an unknown dedup_key resolves to None, not an error"
        );

        // The status guard: a `pending` row loads (with the coordinates to post) …
        let row = load_pending_outbox_row(&pool, id)
            .await
            .unwrap()
            .expect("a pending row loads");
        assert_eq!((row.id, row.kind.as_str()), (id, "reaction"));

        // … a `posted` row does NOT (the guard is what makes redelivery an idempotent no-op — never a
        // re-post) …
        mark_outbox_posted(&pool, id, Some(555)).await.unwrap();
        assert!(
            load_pending_outbox_row(&pool, id).await.unwrap().is_none(),
            "a posted row is not pending → the handler skips it"
        );
        // … and neither does an absent id.
        assert!(load_pending_outbox_row(&pool, 10_000_000)
            .await
            .unwrap()
            .is_none());

        // Dead-letter: park a *pending* row `failed` unconditionally (the engine's retry ceiling already
        // fired), recording the error and dropping it from the claim set — mirroring the drain's terminal
        // state.
        enqueue_outbox_post(
            &pool,
            Platform::GitHub,
            Some(task),
            99,
            "o",
            "r",
            "reply",
            &payload,
            "egress-k2",
        )
        .await
        .unwrap();
        let dead_id = outbox_id_by_dedup_key(&pool, "egress-k2")
            .await
            .unwrap()
            .unwrap();
        mark_outbox_dead_letter(&pool, dead_id, "PR was deleted (404)")
            .await
            .unwrap();
        assert!(
            load_pending_outbox_row(&pool, dead_id)
                .await
                .unwrap()
                .is_none(),
            "a dead-lettered row is no longer pending"
        );
        assert!(
            claim_outbox_batch(&pool, 10).await.unwrap().is_empty(),
            "a dead-lettered row is not re-claimable by the drain either"
        );
        let (status, last_error): (String, Option<String>) =
            sqlx::query_as("SELECT status, last_error FROM outbox WHERE id = $1")
                .bind(dead_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "failed");
        assert_eq!(last_error.as_deref(), Some("PR was deleted (404)"));

        // The `AND status = 'pending'` guard: dead-lettering a row another consumer already settled is a
        // no-op — it must never clobber a `posted` row back to `failed`. `id` was marked `posted` above.
        mark_outbox_dead_letter(&pool, id, "late dead-letter")
            .await
            .unwrap();
        let (settled_status, settled_error): (String, Option<String>) =
            sqlx::query_as("SELECT status, last_error FROM outbox WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            settled_status, "posted",
            "dead-letter must not overwrite an already-posted row"
        );
        assert_eq!(
            settled_error, None,
            "the posted row's fields are untouched by the guarded no-op"
        );
    }

    /// ADR-0068: the verdict path. A clean review enqueues a 👍 reaction targeting the @mention comment
    /// (a `comment_id` in the payload) and — crucially — **no** `review` intent, so the reconciler posts
    /// nothing but the reaction. A review with findings enqueues 👎 on the PR body (no `comment_id`).
    /// Both verdicts share ONE dedup key per task (`<task>:reaction:verdict`), so a verdict flip across
    /// finalize attempts can never stack 👍 AND 👎 — the first verdict wins.
    #[sqlx::test]
    async fn verdict_reaction_targets_trigger_and_clean_pass_enqueues_no_review(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let clean = create_task(&pool, &pr_task(repo_id, "clean"))
            .await
            .unwrap()
            .expect("clean task");
        let dirty = create_task(&pool, &pr_task(repo_id, "dirty"))
            .await
            .unwrap()
            .expect("dirty task");

        // The drain (no-op) egress router — this test exercises the enqueue/dedup ledger, not the
        // Restate send-path (ADR-0074).
        let egress = crate::egress::PlatformEgressRouter::disabled();
        // Clean pass: 👍 on the @mention comment, no review intent.
        let t_clean = crate::outbox::Target {
            task_id: Some(clean),
            platform: Platform::GitHub,
            installation_id: 99,
            owner: "o",
            repo: "r",
            egress: &egress,
        };
        crate::outbox::enqueue_verdict_reaction(
            &pool,
            &t_clean,
            7,
            "+1",
            Some(555),
            "pull_request",
        )
        .await
        .unwrap();
        // A re-finalize that flips the verdict (e.g. a stray retry against the cleared buffer) is a
        // no-op: one shared `verdict` key per task, first verdict wins.
        assert!(
            !crate::outbox::enqueue_verdict_reaction(
                &pool,
                &t_clean,
                7,
                "-1",
                Some(555),
                "pull_request"
            )
            .await
            .unwrap(),
            "a flipped verdict on the same task must not enqueue a second reaction"
        );

        // Findings: 👎 on the PR body (no comment_id).
        let t_dirty = crate::outbox::Target {
            task_id: Some(dirty),
            platform: Platform::GitHub,
            installation_id: 99,
            owner: "o",
            repo: "r",
            egress: &egress,
        };
        crate::outbox::enqueue_verdict_reaction(&pool, &t_dirty, 8, "-1", None, "pull_request")
            .await
            .unwrap();

        let rows = claim_outbox_batch(&pool, 10).await.unwrap();
        // Only the two reaction intents exist — the clean pass enqueued NO review, and the verdict
        // flip did not add a third row.
        assert_eq!(rows.len(), 2, "two reactions, NO review intent, no dup");
        assert!(
            rows.iter().all(|r| r.kind == "reaction"),
            "the clean pass posts a reaction only — never a review"
        );

        let clean_row = rows.iter().find(|r| r.task_id == Some(clean)).unwrap();
        assert_eq!(clean_row.payload["content"], "+1", "first verdict (👍) won");
        assert_eq!(
            clean_row.payload["comment_id"], 555,
            "clean 👍 targets the @mention comment (ADR-0068)"
        );

        let dirty_row = rows.iter().find(|r| r.task_id == Some(dirty)).unwrap();
        assert_eq!(dirty_row.payload["content"], "-1", "findings → 👎");
        assert!(
            dirty_row.payload.get("comment_id").is_none(),
            "an auto review's 👎 targets the PR body, not a comment"
        );
    }

    /// ADR-0068 re-finalize safety: the silent-clean guard trips on a `review` intent OR an
    /// actually-posted review, and the clean-path persist never clobbers a posted row (it would null
    /// `platform_review_id` and break the ADR-0035 feedback join).
    #[sqlx::test]
    async fn silent_clean_guard_and_insert_if_absent_protect_a_posted_review(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let task = create_task(&pool, &pr_task(repo_id, "h"))
            .await
            .unwrap()
            .expect("task");
        let findings = serde_json::json!([]);

        // Nothing yet → the clean branch may proceed.
        assert!(!has_review_intent_or_posted_review(&pool, task)
            .await
            .unwrap());

        // A silent-clean persisted row (no platform_review_id) does NOT trip the guard — re-running the
        // clean path is harmless (insert-if-absent no-ops, verdict key no-ops).
        insert_review_if_absent(&pool, task, "clean", "body", 0, 0, 0, &findings)
            .await
            .unwrap();
        assert!(!has_review_intent_or_posted_review(&pool, task)
            .await
            .unwrap());

        // The reconciler upserts the POSTED copy (platform_review_id set) → guard trips…
        upsert_review(
            &pool,
            task,
            "found things",
            "body",
            2,
            0,
            0,
            &findings,
            Some("https://github.com/o/r/pull/7#pullrequestreview-9"),
            Some(9),
        )
        .await
        .unwrap();
        assert!(has_review_intent_or_posted_review(&pool, task)
            .await
            .unwrap());

        // …and a late clean-path insert is a no-op: the posted row keeps its platform_review_id.
        insert_review_if_absent(&pool, task, "clean", "body", 0, 0, 0, &findings)
            .await
            .unwrap();
        let row = get_review(&pool, task).await.unwrap().expect("review row");
        assert_eq!(
            row.platform_review_id,
            Some(9),
            "insert-if-absent must never clobber the posted review"
        );
        assert_eq!(row.inline_count, 2, "posted counts survive");

        // A pending `review` intent alone (no reviews row yet) also trips the guard.
        let task2 = create_task(&pool, &pr_task(repo_id, "h2"))
            .await
            .unwrap()
            .expect("task2");
        enqueue_outbox_post(
            &pool,
            Platform::GitHub,
            Some(task2),
            99,
            "o",
            "r",
            "review",
            &serde_json::json!({}),
            "t2:review",
        )
        .await
        .unwrap();
        assert!(has_review_intent_or_posted_review(&pool, task2)
            .await
            .unwrap());
    }

    /// #219 review: the failure-notice gate must treat a still-in-flight review intent as "responding",
    /// so a transiently-backing-off review doesn't let a misleading apology race ahead of it — but a
    /// dead-lettered (`failed`) review must NOT suppress the notice (then the review truly won't land).
    #[sqlx::test]
    async fn has_responded_or_pending_content_covers_in_flight_review(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let task = create_task(&pool, &pr_task(repo_id, "h"))
            .await
            .unwrap()
            .expect("task");
        let payload = serde_json::json!({ "pr": 7 });
        assert!(
            !has_responded_or_pending_content(&pool, task).await.unwrap(),
            "nothing posted or queued yet"
        );

        // A pending review intent → counts as responding (suppress the notice).
        enqueue_outbox_post(
            &pool,
            Platform::GitHub,
            Some(task),
            99,
            "o",
            "r",
            "review",
            &payload,
            "rk",
        )
        .await
        .unwrap();
        assert!(
            has_responded_or_pending_content(&pool, task).await.unwrap(),
            "a pending review intent means a review is coming"
        );

        // Once that review intent dead-letters, it no longer suppresses (the review won't post).
        let id = claim_outbox_batch(&pool, 10).await.unwrap()[0].id;
        for _ in 0..OUTBOX_MAX_ATTEMPTS {
            sqlx::query(
                "UPDATE outbox SET next_attempt_at = now() - interval '1 minute' WHERE id = $1",
            )
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
            mark_outbox_failed(&pool, id, "boom").await.unwrap();
        }
        assert!(
            !has_responded_or_pending_content(&pool, task).await.unwrap(),
            "a dead-lettered review no longer suppresses the failure notice"
        );
    }

    /// ADR-0033 slice 3: an `issue` target (no SHAs) persists and reads back, and the idempotency key
    /// includes `target_type` so an issue and a PR with the same number are distinct tasks.
    #[sqlx::test]
    async fn issue_target_task_round_trips_and_is_distinct_from_a_pr(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let issue = NewTask {
            repository_id: repo_id,
            installation_id: 99,
            webhook_delivery_id: "d1".to_string(),
            target_type: "issue".to_string(),
            target_id: 42,
            command_text: "explain the retry logic".to_string(),
            base_sha: None,
            head_sha: None,
            run_epoch: 0,
            tier: "deep".to_string(),
            trigger_comment_id: None,
        };
        let issue_id = create_task(&pool, &issue)
            .await
            .unwrap()
            .expect("issue task");
        let ctx = get_task_context(&pool, issue_id)
            .await
            .unwrap()
            .expect("exists");
        assert_eq!(ctx.target_type, "issue");
        assert_eq!(ctx.target_id, 42);
        assert!(ctx.base_sha.is_none() && ctx.head_sha.is_none());

        // A PR #42 (same number) is a different task — target_type discriminates the idempotency key.
        let mut pr = issue;
        pr.target_type = "pull_request".to_string();
        let pr_id = create_task(&pool, &pr).await.unwrap().expect("pr task");
        assert_ne!(issue_id, pr_id);
    }

    /// Task creation is idempotent on (repo, target, command, head SHA): a second `pull_request`
    /// event for the same head (e.g. `opened` then `synchronize`) does not create a duplicate task,
    /// but a new head SHA does.
    #[sqlx::test]
    async fn create_task_is_idempotent_on_target_and_head(pool: PgPool) {
        let repo_id = seed(&pool).await;

        let first = create_task(&pool, &pr_task(repo_id, "head1"))
            .await
            .unwrap();
        assert!(first.is_some(), "first task is created");

        let dup = create_task(&pool, &pr_task(repo_id, "head1"))
            .await
            .unwrap();
        assert!(dup.is_none(), "equivalent task is deduped");

        let new_head = create_task(&pool, &pr_task(repo_id, "head2"))
            .await
            .unwrap();
        assert!(new_head.is_some(), "a new head SHA is a new task");

        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2);
    }

    /// An explicit `@mention` command (`create_explicit_task`) must ALWAYS land a row — never be
    /// content-deduped. Two identical "review this" mentions on the same (repo, target, head) create
    /// TWO distinct tasks at consecutive epochs N and N+1, with no silent drop. This is the
    /// regression guard for the prod bug where a repeated re-request collided with the idempotency
    /// index and vanished.
    #[sqlx::test]
    async fn explicit_mention_always_creates_a_task_at_the_next_epoch(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let mention = |head: &str| NewTask {
            repository_id: repo_id,
            installation_id: 99,
            webhook_delivery_id: "d1".to_string(),
            target_type: "pull_request".to_string(),
            target_id: 7,
            command_text: "review this".to_string(),
            base_sha: Some("base".to_string()),
            head_sha: Some(head.to_string()),
            run_epoch: 0, // ignored by create_explicit_task — the INSERT computes the epoch
            tier: "deep".to_string(),
            trigger_comment_id: None,
        };

        let first = create_explicit_task(&pool, &mention("h1")).await.unwrap();
        let second = create_explicit_task(&pool, &mention("h1")).await.unwrap();
        assert_ne!(first, second, "a repeated mention is not deduped");

        let epochs: Vec<i32> = sqlx::query_scalar(
            "SELECT run_epoch FROM tasks WHERE command_text = 'review this' ORDER BY run_epoch",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(epochs, vec![0, 1], "consecutive epochs N and N+1");
    }

    /// The auto `pull_request` path keeps content-idempotency: a duplicate `opened` then
    /// `synchronize` for the SAME head and command "review" collapses to ONE task (GitHub commonly
    /// delivers both for a single head). Only the explicit `@mention` path was changed.
    #[sqlx::test]
    async fn auto_review_collapses_duplicate_opened_and_synchronize(pool: PgPool) {
        let repo_id = seed(&pool).await;

        let opened = create_task(&pool, &pr_task(repo_id, "head1"))
            .await
            .unwrap();
        assert!(opened.is_some(), "the opened event creates the task");

        let synchronize = create_task(&pool, &pr_task(repo_id, "head1"))
            .await
            .unwrap();
        assert!(
            synchronize.is_none(),
            "a duplicate for the same head is collapsed"
        );

        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "exactly one auto-review task for the head");
    }

    /// The dispatcher claim takes exactly one queued task and leaves none for the next claim — the
    /// `SKIP LOCKED` guard that lets dispatcher replicas run concurrently without double-claiming.
    #[sqlx::test]
    async fn claim_next_task_takes_one_queued_task(pool: PgPool) {
        let repo_id = seed(&pool).await;
        create_task(&pool, &pr_task(repo_id, "head1"))
            .await
            .unwrap()
            .unwrap();

        let claimed = claim_next_task(&pool, "owner-a", Duration::from_secs(60))
            .await
            .unwrap();
        let claimed = claimed.expect("a queued task is claimed");
        assert_eq!(claimed.attempts, 1, "claim increments attempts");
        assert_eq!(claimed.command_text, "review");

        let none = claim_next_task(&pool, "owner-b", Duration::from_secs(60))
            .await
            .unwrap();
        assert!(none.is_none(), "the claimed task is no longer queued");
    }

    /// Embedding-dimension reconcile: same dim → no-op; a change without the flag fails loud; with the
    /// flag it wipes + migrates the column to the new width.
    async fn embedding_dim(pool: &PgPool) -> i32 {
        sqlx::query_scalar::<_, i32>(
            "SELECT atttypmod FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
             WHERE c.relname = 'code_chunks' AND a.attname = 'embedding' AND NOT a.attisdropped",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    async fn reconcile_embedding_dimension_guards_and_migrates(pool: PgPool) {
        // Migrations create code_chunks.embedding as vector(4096) (ADR-0018).
        assert_eq!(embedding_dim(&pool).await, 4096);

        // Same dimension → no-op.
        reconcile_embedding_dimension(&pool, 4096, false)
            .await
            .unwrap();
        assert_eq!(embedding_dim(&pool).await, 4096);

        // A change without the flag fails loud (no destruction).
        assert!(reconcile_embedding_dimension(&pool, 1536, false)
            .await
            .is_err());
        assert_eq!(
            embedding_dim(&pool).await,
            4096,
            "column untouched when not allowed"
        );

        // With the flag, the column migrates to the new width.
        reconcile_embedding_dimension(&pool, 1536, true)
            .await
            .unwrap();
        assert_eq!(embedding_dim(&pool).await, 1536);
    }

    /// `cancel_active_tasks_for_pr` cancels a PR's active task when the PR is closed.
    #[sqlx::test]
    async fn cancel_active_tasks_for_pr_cancels_the_prs_task(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let id = create_task(&pool, &pr_task(repo_id, "h1"))
            .await
            .unwrap()
            .unwrap();

        // Closing the PR cancels its active task.
        let cancelled = cancel_active_tasks_for_pr(&pool, repo_id, 7).await.unwrap();
        assert_eq!(cancelled, vec![id]);
        let status: String = sqlx::query_scalar("SELECT status FROM tasks WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "cancelled");
    }

    /// A released task returns to the queue and can be claimed again (Job-launch failure path).
    #[sqlx::test]
    async fn release_task_requeues_for_another_claim(pool: PgPool) {
        let repo_id = seed(&pool).await;
        create_task(&pool, &pr_task(repo_id, "head1"))
            .await
            .unwrap()
            .unwrap();

        let first = claim_next_task(&pool, "owner-a", Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap();
        // Zero backoff so it is immediately due again.
        release_task(&pool, first.id, Duration::from_secs(0))
            .await
            .unwrap();

        // Releasing clears started_at so the dashboard doesn't show a queued task as already running.
        let started_at: Option<OffsetDateTime> =
            sqlx::query_scalar("SELECT started_at FROM tasks WHERE id = $1")
                .bind(first.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(started_at.is_none(), "release clears started_at");

        let second = claim_next_task(&pool, "owner-a", Duration::from_secs(60))
            .await
            .unwrap()
            .expect("released task is claimable again");
        assert_eq!(second.id, first.id);
        assert_eq!(second.attempts, 2, "the re-claim counts as another attempt");
    }

    /// `list_repositories` aggregates run activity (it's runtime SQL, so this is the only place the
    /// GROUP BY/JOIN is exercised): a repo with two tasks reports `task_count = 2`, an idle repo
    /// reports `0` with a null `last_task_at`, and the active repo sorts first.
    #[sqlx::test]
    async fn list_repositories_summarises_activity(pool: PgPool) {
        let active = upsert_repository(&pool, Platform::GitHub, 1, "vymalo", "shop", "main", None)
            .await
            .unwrap();
        let idle = upsert_repository(&pool, Platform::GitHub, 2, "vymalo", "idle", "trunk", None)
            .await
            .unwrap();

        for (n, delivery) in ["d-1", "d-2"].iter().enumerate() {
            // tasks.webhook_delivery_id FKs webhook_deliveries — record the delivery first, exactly as
            // the webhook handler does before creating a task.
            record_delivery(
                &pool,
                Platform::GitHub,
                delivery,
                "pull_request",
                &json!({}),
            )
            .await
            .unwrap();
            create_task(
                &pool,
                &NewTask {
                    repository_id: active,
                    installation_id: 7,
                    webhook_delivery_id: (*delivery).to_string(),
                    target_type: "pull_request".to_string(),
                    target_id: n as i64,
                    command_text: "review".to_string(),
                    base_sha: None,
                    head_sha: None,
                    run_epoch: 0,
                    tier: "deep".to_string(),
                    trigger_comment_id: None,
                },
            )
            .await
            .unwrap();
        }

        let repos = list_repositories(&pool, None).await.unwrap();
        assert_eq!(repos.len(), 2);
        // (approval-gate behaviour is covered by `repo_approval_status_transitions`)

        // Active repo (has tasks) sorts first by last_task_at.
        assert_eq!(repos[0].id, active);
        assert_eq!(repos[0].task_count, 2);
        assert!(repos[0].last_task_at.is_some());

        let idle_row = repos.iter().find(|r| r.id == idle).unwrap();
        assert_eq!(idle_row.task_count, 0);
        assert!(idle_row.last_task_at.is_none());
    }

    /// The approval gate (Epic #75): new repos are pending; register_pending is insert-only;
    /// approve/deny flip the gate + record the approver; the status filter scopes the list.
    #[sqlx::test]
    async fn repo_approval_status_transitions(pool: PgPool) {
        // A repo seen via the normal upsert path defaults to pending → gated.
        let id = upsert_repository(&pool, Platform::GitHub, 4242, "o", "r", "main", None)
            .await
            .unwrap();
        assert!(
            !repository_approved(&pool, id).await.unwrap(),
            "new repos start pending (gated)"
        );

        // register_pending is insert-only: it reports not-new for an existing repo and leaves it be.
        assert!(
            !register_pending_repository(&pool, Platform::GitHub, 4242, "o", "r", "", None)
                .await
                .unwrap()
        );
        // A brand-new repo registers as pending (reports new).
        assert!(
            register_pending_repository(&pool, Platform::GitHub, 5555, "o", "r2", "", None)
                .await
                .unwrap()
        );

        // Both pending repos show under the status filter; none are approved yet.
        let pending = list_repositories(&pool, Some("pending")).await.unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().all(|r| r.status == "pending" && !r.active));
        assert!(list_repositories(&pool, Some("approved"))
            .await
            .unwrap()
            .is_empty());

        // Approve → approved + active + records the approver; the gate opens.
        let row = set_repository_status_by_id(&pool, id, "approved", Some("alice"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "approved");
        assert!(row.active);
        assert_eq!(row.approved_by.as_deref(), Some("alice"));
        assert!(row.approved_at.is_some());
        assert!(repository_approved(&pool, id).await.unwrap());

        // Disable → not approved, approver/timestamp cleared.
        let row = set_repository_status_by_id(&pool, id, "disabled", None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "disabled");
        assert!(!row.active);
        assert!(row.approved_by.is_none() && row.approved_at.is_none());
        assert!(!repository_approved(&pool, id).await.unwrap());

        // Disable-by-github-id (the webhook removal path).
        set_repository_status_by_platform_id(&pool, Platform::GitHub, 5555, "disabled")
            .await
            .unwrap();
        // Re-install of a DISABLED repo re-opens it to pending (so the admin can re-approve).
        assert!(
            register_pending_repository(&pool, Platform::GitHub, 5555, "o", "r2", "", None)
                .await
                .unwrap(),
            "re-registering a disabled repo re-pends it"
        );
        assert!(list_repositories(&pool, Some("pending"))
            .await
            .unwrap()
            .iter()
            .any(|r| r.platform_repo_id == 5555));
        // Re-registering an APPROVED repo is a no-op (must not revert it).
        set_repository_status_by_id(&pool, id, "approved", Some("alice"))
            .await
            .unwrap();
        assert!(
            !register_pending_repository(&pool, Platform::GitHub, 4242, "o", "r", "", None)
                .await
                .unwrap(),
            "an approved repo is not re-pended"
        );
        assert!(repository_approved(&pool, id).await.unwrap());

        // Unknown local id → None.
        assert!(
            set_repository_status_by_id(&pool, 999_999, "approved", Some("x"))
                .await
                .unwrap()
                .is_none()
        );
    }

    /// Data purge (Epic #75, Milestone B): cancelling a repo's tasks is scoped to that repo, and the
    /// delete helpers are safe no-ops on an empty repo.
    #[sqlx::test]
    async fn purge_cancels_only_target_repo_tasks(pool: PgPool) {
        let repo_a = seed(&pool).await;
        let repo_b =
            upsert_repository(&pool, Platform::GitHub, 9001, "octo", "other", "main", None)
                .await
                .unwrap();
        create_task(&pool, &pr_task(repo_a, "h1"))
            .await
            .unwrap()
            .unwrap();
        create_task(&pool, &pr_task(repo_b, "h2"))
            .await
            .unwrap()
            .unwrap();

        // Cancels only repo_a's active task; repo_b's stays cancellable.
        assert_eq!(
            cancel_active_tasks_for_repo(&pool, repo_a)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            cancel_active_tasks_for_repo(&pool, repo_a)
                .await
                .unwrap()
                .is_empty(),
            "already cancelled → no-op"
        );
        assert_eq!(
            cancel_active_tasks_for_repo(&pool, repo_b)
                .await
                .unwrap()
                .len(),
            1
        );

        // Delete helpers are safe no-ops with nothing indexed.
        assert_eq!(delete_code_chunks_for_repo(&pool, repo_a).await.unwrap(), 0);
        assert_eq!(delete_repo_index_rows(&pool, repo_a).await.unwrap(), 0);
    }

    /// Index-on-approve (Epic #75, Milestone B): the repo's installation_id round-trips, and the index
    /// task is created once but not duplicated while one is already active.
    #[sqlx::test]
    async fn index_task_creation_is_deduped(pool: PgPool) {
        let id = upsert_repository(&pool, Platform::GitHub, 1212, "o", "r", "main", Some(555))
            .await
            .unwrap();
        assert_eq!(
            repository_installation_id(&pool, id).await.unwrap(),
            Some(555),
            "installation_id recorded for the clone token"
        );

        let first = create_index_task(&pool, id, 555).await.unwrap();
        assert!(first.is_some(), "first approve enqueues an index task");
        assert!(
            create_index_task(&pool, id, 555).await.unwrap().is_none(),
            "no duplicate while one index task is active"
        );

        // Regression: once the first index reaches a terminal state, a later default-branch push MUST
        // be able to enqueue a fresh index. The old code hardcoded `run_epoch = 0`, so the second insert
        // cleared the active-guard but then collided on `tasks_idempotency_idx` (NULL head_sha, epoch 0)
        // → `duplicate key` → a repo was only ever indexed once.
        set_task_status(&pool, first.unwrap(), "succeeded", None)
            .await
            .unwrap();
        let second = create_index_task(&pool, id, 555).await.unwrap();
        assert!(
            second.is_some(),
            "a push after the first index completed enqueues a new index (fresh run_epoch)"
        );
        let epoch: i32 = sqlx::query_scalar("SELECT run_epoch FROM tasks WHERE id = $1")
            .bind(second.unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            epoch, 1,
            "the re-index bumps run_epoch past the first index"
        );
    }

    /// Review persistence (Epic #75, Milestone C): upsert stores the review + findings; get returns it;
    /// a re-post upserts (one row per task); unknown task → None.
    #[sqlx::test]
    async fn review_persist_and_read(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let task_id = create_task(&pool, &pr_task(repo_id, "h1"))
            .await
            .unwrap()
            .unwrap();
        assert!(get_review(&pool, task_id).await.unwrap().is_none());

        let findings = serde_json::json!([{ "file": "a.rs", "line": 3, "severity": "error",
            "title": "t", "body": "b" }]);
        upsert_review(
            &pool,
            task_id,
            "sum",
            "body",
            1,
            2,
            0,
            &findings,
            Some("https://github.com/o/r/pull/7#pullrequestreview-1"),
            Some(987654),
        )
        .await
        .unwrap();
        let row = get_review(&pool, task_id).await.unwrap().unwrap();
        assert_eq!(row.summary, "sum");
        assert_eq!(row.inline_count, 1);
        assert_eq!(row.deferred_count, 2);
        assert_eq!(row.findings[0]["file"], "a.rs");
        assert_eq!(
            row.review_url.as_deref(),
            Some("https://github.com/o/r/pull/7#pullrequestreview-1")
        );
        assert_eq!(
            row.platform_review_id,
            Some(987654),
            "review id persisted (ADR-0035)"
        );

        // Re-post upserts in place (still one row).
        upsert_review(
            &pool, task_id, "sum2", "body2", 0, 0, 0, &findings, None, None,
        )
        .await
        .unwrap();
        assert_eq!(
            get_review(&pool, task_id).await.unwrap().unwrap().summary,
            "sum2"
        );

        assert!(get_review(&pool, Uuid::new_v4()).await.unwrap().is_none());
    }

    /// ADR-0065 × ADR-0068 composition gate: the silent-clean path is only allowed when NO prior review
    /// of this target carried findings. Empty-findings priors don't count; the current task's own review
    /// doesn't count; a prior with findings on ANY commit does (retractions must be visible).
    #[sqlx::test]
    async fn target_has_prior_findings_gates_on_nonempty_prior_findings(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let prior = create_task(&pool, &pr_task(repo_id, "h1"))
            .await
            .unwrap()
            .unwrap();
        let current = create_task(&pool, &pr_task(repo_id, "h2"))
            .await
            .unwrap()
            .unwrap();

        // No prior review at all → no prior findings.
        assert!(
            !target_has_prior_findings(&pool, repo_id, "pull_request", 7, current)
                .await
                .unwrap()
        );

        // A prior CLEAN review (empty findings array) still doesn't gate — nothing to retract.
        upsert_review(
            &pool,
            prior,
            "clean",
            "b",
            0,
            0,
            0,
            &serde_json::json!([]),
            None,
            None,
        )
        .await
        .unwrap();
        assert!(
            !target_has_prior_findings(&pool, repo_id, "pull_request", 7, current)
                .await
                .unwrap(),
            "an empty findings array is not a prior finding"
        );

        // A prior review WITH findings (any commit — h1 vs the current h2) gates the silence.
        let f = serde_json::json!([{ "file": "a.rs", "line": 3, "title": "leak", "body": "b" }]);
        upsert_review(&pool, prior, "one P1", "b", 1, 0, 0, &f, None, None)
            .await
            .unwrap();
        assert!(
            target_has_prior_findings(&pool, repo_id, "pull_request", 7, current)
                .await
                .unwrap(),
            "prior findings on any commit force the verdict to post"
        );

        // The current task's own persisted review never counts as a prior.
        assert!(
            !target_has_prior_findings(&pool, repo_id, "pull_request", 7, prior)
                .await
                .unwrap(),
            "a task is never its own prior"
        );
    }

    /// ADR-0040 + ADR-0065: a re-review on the same target finds ALL earlier reviews (newest first),
    /// each carrying its TRUE chronological ordinal (1 = the first review ever), scoped to the same
    /// `(repository_id, target_type, target_id)` and excluding the current task.
    #[sqlx::test]
    async fn all_prior_reviews_are_target_scoped_ordered_and_exclude_current(pool: PgPool) {
        let repo_id = seed(&pool).await;
        // Three tasks on the SAME PR (#7): two prior reviews and the current re-review.
        let first = create_task(&pool, &pr_task(repo_id, "h1"))
            .await
            .unwrap()
            .unwrap();
        let second = create_task(&pool, &pr_task(repo_id, "h2"))
            .await
            .unwrap()
            .unwrap();
        let rereview = create_task(&pool, &pr_task(repo_id, "h3"))
            .await
            .unwrap()
            .unwrap();

        let f1 = serde_json::json!([{ "file": "a.rs", "line": 3, "priority": "P1",
            "category": "quality", "title": "leak", "body": "b" }]);
        let f2 = serde_json::json!([{ "file": "b.rs", "line": 9, "priority": "P2",
            "category": "style", "title": "nit", "body": "b" }]);
        upsert_review(
            &pool,
            first,
            "first verdict",
            "body",
            1,
            0,
            0,
            &f1,
            None,
            None,
        )
        .await
        .unwrap();
        upsert_review(
            &pool,
            second,
            "second verdict",
            "body",
            1,
            0,
            0,
            &f2,
            None,
            None,
        )
        .await
        .unwrap();

        // Force distinct, ordered timestamps: two upserts in one fast test can land in the same
        // microsecond, and the query's tie-breaker (task_id) is a random UUID — deterministic for the
        // QUERY but not for this test's expectation of "first before second".
        sqlx::query(
            "UPDATE reviews SET created_at = created_at - interval '1 minute' WHERE task_id = $1",
        )
        .bind(first)
        .execute(&pool)
        .await
        .unwrap();

        // The re-review sees BOTH priors, newest first (second before first), excluding itself — and
        // each carries its true chronological ordinal (window over the full set, not the fetched slice).
        let priors = all_prior_reviews_for_target(&pool, repo_id, "pull_request", 7, rereview)
            .await
            .unwrap();
        assert_eq!(priors.len(), 2, "both prior reviews returned");
        assert_eq!(priors[0].1, "second verdict", "newest first");
        assert_eq!(priors[0].0, 2, "the newest review is chronologically #2");
        assert_eq!(priors[1].1, "first verdict");
        assert_eq!(priors[1].0, 1, "the oldest review is chronologically #1");
        assert_eq!(priors[0].2[0]["title"], "nit");

        // From the first task's own perspective its own review is excluded → only the second remains,
        // and its ordinal is computed over the remaining set visible to that task.
        let from_first = all_prior_reviews_for_target(&pool, repo_id, "pull_request", 7, first)
            .await
            .unwrap();
        assert_eq!(from_first.len(), 1, "a task is never its own prior review");
        assert_eq!(from_first[0].1, "second verdict");

        // A different target on the same repo doesn't leak across.
        assert!(
            all_prior_reviews_for_target(&pool, repo_id, "pull_request", 999, rereview)
                .await
                .unwrap()
                .is_empty(),
            "scoped to the target id"
        );
    }

    /// ADR-0065 (Option B): the finalize dedup source — findings already posted on the SAME head_sha,
    /// excluding the current task. A prior review on a *different* head_sha is not returned (line drift).
    #[sqlx::test]
    async fn posted_findings_for_head_is_head_scoped_and_excludes_current(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let prior_same_head = create_task(&pool, &pr_task(repo_id, "same"))
            .await
            .unwrap()
            .unwrap();
        let prior_other_head = create_task(&pool, &pr_task(repo_id, "other"))
            .await
            .unwrap()
            .unwrap();
        // The current re-review shares head_sha "same" with `prior_same_head` (a new run_epoch).
        let current = create_task(
            &pool,
            &NewTask {
                run_epoch: 1,
                ..pr_task(repo_id, "same")
            },
        )
        .await
        .unwrap()
        .unwrap();

        let f_same =
            serde_json::json!([{ "file": "a.rs", "line": 3, "title": "leak", "body": "b" }]);
        let f_other =
            serde_json::json!([{ "file": "b.rs", "line": 9, "title": "other", "body": "b" }]);
        upsert_review(
            &pool,
            prior_same_head,
            "v",
            "b",
            1,
            0,
            0,
            &f_same,
            None,
            None,
        )
        .await
        .unwrap();
        upsert_review(
            &pool,
            prior_other_head,
            "v",
            "b",
            1,
            0,
            0,
            &f_other,
            None,
            None,
        )
        .await
        .unwrap();

        let posted = posted_findings_for_head(&pool, repo_id, "pull_request", 7, "same", current)
            .await
            .unwrap();
        assert_eq!(posted.len(), 1, "only the same-head prior is returned");
        assert_eq!(posted[0][0]["title"], "leak");

        // The current task never dedups against its own (not-yet-posted) review.
        upsert_review(&pool, current, "v", "b", 1, 0, 0, &f_same, None, None)
            .await
            .unwrap();
        let posted = posted_findings_for_head(&pool, repo_id, "pull_request", 7, "same", current)
            .await
            .unwrap();
        assert_eq!(
            posted.len(),
            1,
            "still only the *other* task's review, not its own"
        );

        // A review ENQUEUED but not yet delivered (pending `outbox` row — reconciler backoff, or
        // two rapid re-reviews racing finalize) also counts: its findings ride the payload JSON.
        let queued_same_head = create_task(
            &pool,
            &NewTask {
                run_epoch: 2,
                ..pr_task(repo_id, "same")
            },
        )
        .await
        .unwrap()
        .unwrap();
        let payload = serde_json::json!({
            "pr": 7, "body": "b", "summary": "v", "comments": [],
            "inline_n": 1, "deferred_n": 0, "out_of_scope_n": 0,
            "findings_json": [{ "file": "q.rs", "line": 5, "title": "queued finding", "body": "b" }],
            "label_findings": true, "label_error": false
        });
        enqueue_outbox_post(
            &pool,
            Platform::GitHub,
            Some(queued_same_head),
            99,
            "acme",
            "rocket",
            "review",
            &payload,
            &format!("{queued_same_head}:review"),
        )
        .await
        .unwrap();
        let posted = posted_findings_for_head(&pool, repo_id, "pull_request", 7, "same", current)
            .await
            .unwrap();
        assert_eq!(posted.len(), 2, "pending outbox review counts as posted");
        assert!(
            posted.iter().any(|arr| arr[0]["title"] == "queued finding"),
            "the queued review's findings come from the outbox payload"
        );

        // Once delivered (`posted`), the outbox arm skips it — the reconciler persists it into
        // `reviews` at that point, so the first arm covers it without double-counting.
        sqlx::query("UPDATE outbox SET status = 'posted' WHERE task_id = $1")
            .bind(queued_same_head)
            .execute(&pool)
            .await
            .unwrap();
        let posted = posted_findings_for_head(&pool, repo_id, "pull_request", 7, "same", current)
            .await
            .unwrap();
        assert_eq!(
            posted.len(),
            1,
            "a delivered outbox row no longer contributes (reviews row takes over)"
        );
    }

    /// M1 feedback memory (ADR-0044): a 👎 (`-1`) on an inline finding surfaces it as rejected (joined
    /// to its title via the findings JSON); a 👍 (`+1`) does not.
    #[sqlx::test]
    async fn rejected_findings_for_repo_returns_thumbs_down_only(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let task_id = create_task(&pool, &pr_task(repo_id, "h1"))
            .await
            .unwrap()
            .unwrap();
        let findings = serde_json::json!([
            { "file": "a.rs", "line": 7, "priority": "P1", "category": "correctness", "title": "Bogus nit", "body": "b" },
            { "file": "a.rs", "line": 9, "priority": "P2", "category": "style", "title": "Liked nit", "body": "b" }
        ]);
        upsert_review(
            &pool, task_id, "sum", "body", 2, 0, 0, &findings, None, None,
        )
        .await
        .unwrap();
        store_review_comments(
            &pool,
            task_id,
            &[
                ReviewCommentRef {
                    platform_comment_id: 555,
                    kind: "inline".to_string(),
                    file: Some("a.rs".to_string()),
                    line: Some(7),
                },
                ReviewCommentRef {
                    platform_comment_id: 556,
                    kind: "inline".to_string(),
                    file: Some("a.rs".to_string()),
                    line: Some(9),
                },
            ],
        )
        .await
        .unwrap();
        reconcile_comment_feedback(
            &pool,
            task_id,
            555,
            "inline",
            &[("alice".into(), "-1".into())],
        )
        .await
        .unwrap();
        reconcile_comment_feedback(
            &pool,
            task_id,
            556,
            "inline",
            &[("bob".into(), "+1".into())],
        )
        .await
        .unwrap();

        let rejected = rejected_findings_for_repo(&pool, repo_id, 30)
            .await
            .unwrap();
        assert_eq!(
            rejected,
            vec![("a.rs".to_string(), 7, "Bogus nit".to_string())],
            "only the 👎'd finding, with its title"
        );
    }

    /// ADR-0035: reconcile reactions on a comment — new ones are inserted, vanished ones (un-react)
    /// are deleted, and an empty set clears all. `get_feedback` joins the finding's file/line.
    #[sqlx::test]
    async fn feedback_reconcile_inserts_and_removes(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let task_id = create_task(&pool, &pr_task(repo_id, "h1"))
            .await
            .unwrap()
            .unwrap();
        store_review_comments(
            &pool,
            task_id,
            &[ReviewCommentRef {
                platform_comment_id: 555,
                kind: "inline".to_string(),
                file: Some("a.rs".to_string()),
                line: Some(7),
            }],
        )
        .await
        .unwrap();

        // The comment is pollable (joined to repo identity + installation).
        let pollable = list_pollable_comments(&pool, 14, 300).await.unwrap();
        assert_eq!(pollable.len(), 1);
        assert_eq!(pollable[0].platform_comment_id, 555);
        assert_eq!(pollable[0].owner, "octo");

        // First cycle: two reactions.
        reconcile_comment_feedback(
            &pool,
            task_id,
            555,
            "inline",
            &[
                ("alice".to_string(), "+1".to_string()),
                ("bob".to_string(), "-1".to_string()),
            ],
        )
        .await
        .unwrap();
        let fb = get_feedback(&pool, task_id).await.unwrap();
        assert_eq!(fb.len(), 2);
        assert!(fb
            .iter()
            .any(|r| r.reactor == "alice" && r.reaction == "+1"));
        assert_eq!(
            fb[0].file.as_deref(),
            Some("a.rs"),
            "joined from review_comments"
        );

        // Second cycle: alice un-reacted, carol added → reconcile removes alice, keeps bob, adds carol.
        reconcile_comment_feedback(
            &pool,
            task_id,
            555,
            "inline",
            &[
                ("bob".to_string(), "-1".to_string()),
                ("carol".to_string(), "heart".to_string()),
            ],
        )
        .await
        .unwrap();
        let fb = get_feedback(&pool, task_id).await.unwrap();
        assert_eq!(fb.len(), 2);
        assert!(!fb.iter().any(|r| r.reactor == "alice"), "un-react removed");
        assert!(fb
            .iter()
            .any(|r| r.reactor == "carol" && r.reaction == "heart"));

        // Empty cycle (all reactions gone) clears the comment's feedback.
        reconcile_comment_feedback(&pool, task_id, 555, "inline", &[])
            .await
            .unwrap();
        assert!(get_feedback(&pool, task_id).await.unwrap().is_empty());
    }

    /// ADR-0034: a transcript stores in order and a re-submit replaces it (a task retry re-sends the
    /// whole run), so the stored set always reflects the latest run.
    #[sqlx::test]
    async fn transcript_replace_and_read(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let task_id = create_task(&pool, &pr_task(repo_id, "h1"))
            .await
            .unwrap()
            .unwrap();
        assert!(get_transcript(&pool, task_id).await.unwrap().is_empty());

        let first = vec![
            TranscriptInput {
                role: "assistant".to_string(),
                content: Some("let me search".to_string()),
                tool_calls: Some(
                    serde_json::json!([{ "function": { "name": "lightbridge_vector_semantic_search" } }]),
                ),
                tool_name: None,
                prompt_tokens: Some(1200),
                completion_tokens: Some(30),
                reasoning_tokens: Some(12),
                model: Some("adorsys-reviewer".to_string()),
            },
            TranscriptInput {
                role: "tool".to_string(),
                content: Some("hit: a.rs".to_string()),
                tool_calls: None,
                tool_name: Some("lightbridge_vector_semantic_search".to_string()),
                prompt_tokens: None,
                completion_tokens: None,
                reasoning_tokens: None,
                model: None,
            },
        ];
        replace_transcript(&pool, task_id, &first).await.unwrap();
        let rows = get_transcript(&pool, task_id).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].seq, 0);
        assert_eq!(rows[0].role, "assistant");
        assert_eq!(rows[0].prompt_tokens, Some(1200));
        assert_eq!(rows[1].role, "tool");
        assert_eq!(
            rows[1].tool_name.as_deref(),
            Some("lightbridge_vector_semantic_search")
        );
        // The new per-turn observability columns (0017) round-trip: model + reasoning_tokens on the
        // assistant turn, NULL on the tool-result turn.
        let (model, reasoning): (Option<String>, Option<i64>) = sqlx::query_as(
            "SELECT model, reasoning_tokens FROM agent_transcript WHERE task_id = $1 AND seq = 0",
        )
        .bind(task_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(model.as_deref(), Some("adorsys-reviewer"));
        assert_eq!(reasoning, Some(12));

        // A retry re-submits a (shorter) transcript → fully replaces the old one.
        let second = vec![TranscriptInput {
            role: "assistant".to_string(),
            content: Some("done".to_string()),
            tool_calls: None,
            tool_name: None,
            prompt_tokens: Some(500),
            completion_tokens: Some(10),
            reasoning_tokens: None,
            model: Some("adorsys-reviewer-pro".to_string()),
        }];
        replace_transcript(&pool, task_id, &second).await.unwrap();
        let rows = get_transcript(&pool, task_id).await.unwrap();
        assert_eq!(rows.len(), 1, "replaced, not appended");
        assert_eq!(rows[0].content.as_deref(), Some("done"));
    }

    /// ADR-0034/0062/0066: a review run records its offered tools + redacted base64 config on the task
    /// row at run start; a re-run overwrites in place (one task = one run). A brand-new task starts NULL.
    #[sqlx::test]
    async fn review_run_telemetry_records_and_replaces(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let task_id = create_task(&pool, &pr_task(repo_id, "h1"))
            .await
            .unwrap()
            .unwrap();

        // A fresh task has no run telemetry.
        let (tools, cfg): (Option<Value>, Option<String>) =
            sqlx::query_as("SELECT run_tools, run_config_b64 FROM tasks WHERE id = $1")
                .bind(task_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(tools.is_none() && cfg.is_none(), "starts NULL");

        let offered = json!([
            { "name": "read_file", "source": "builtin" },
            { "name": "mcp__context7__get_docs", "source": "mcp" },
        ]);
        let updated = record_review_run_telemetry(&pool, task_id, &offered, "cnfg-b64-v1")
            .await
            .unwrap();
        assert!(updated, "an existing task reports rows_affected > 0");
        // Unknown id → no row touched, reported as `false` (the handler's 404 signal — no pre-SELECT).
        let updated = record_review_run_telemetry(&pool, Uuid::new_v4(), &offered, "cnfg-b64-x")
            .await
            .unwrap();
        assert!(!updated, "an unknown task id reports no row updated");
        let (tools, cfg): (Option<Value>, Option<String>) =
            sqlx::query_as("SELECT run_tools, run_config_b64 FROM tasks WHERE id = $1")
                .bind(task_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(tools.as_ref().unwrap()[1]["source"], json!("mcp"));
        assert_eq!(cfg.as_deref(), Some("cnfg-b64-v1"));

        // A re-run (retry) overwrites in place — latest run wins.
        record_review_run_telemetry(&pool, task_id, &json!([]), "cnfg-b64-v2")
            .await
            .unwrap();
        let (tools, cfg): (Option<Value>, Option<String>) =
            sqlx::query_as("SELECT run_tools, run_config_b64 FROM tasks WHERE id = $1")
                .bind(task_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(tools.as_ref().unwrap().as_array().unwrap().len(), 0);
        assert_eq!(
            cfg.as_deref(),
            Some("cnfg-b64-v2"),
            "replaced, not appended"
        );
    }

    /// An INDEXING run never submits review telemetry, so its `tasks` row keeps both columns NULL.
    #[sqlx::test]
    async fn index_run_records_no_review_telemetry(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let index_task = create_index_task(&pool, repo_id, 99)
            .await
            .unwrap()
            .expect("index task");
        let (tools, cfg): (Option<Value>, Option<String>) =
            sqlx::query_as("SELECT run_tools, run_config_b64 FROM tasks WHERE id = $1")
                .bind(index_task)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            tools.is_none() && cfg.is_none(),
            "an index run leaves the review-telemetry columns NULL"
        );
    }

    /// The runner's task context joins repository identity onto the task, and returns `None` for an
    /// unknown id (the seam the internal API serves to the agent runner).
    #[sqlx::test]
    async fn get_task_context_joins_repo_identity(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let task_id = create_task(&pool, &pr_task(repo_id, "head1"))
            .await
            .unwrap()
            .unwrap();

        let context = get_task_context(&pool, task_id)
            .await
            .unwrap()
            .expect("task exists");
        assert_eq!(context.owner, "octo");
        assert_eq!(context.name, "repo");
        assert_eq!(context.default_branch, "main");
        assert_eq!(context.installation_id, 99);
        assert_eq!(context.command_text, "review");
        assert_eq!(context.kind, "review", "run kind round-trips (ADR-0033)");
        assert_eq!(
            context.tier, "fast",
            "the automatic PR review is the FAST tier (ADR-0062)"
        );
        assert_eq!(context.head_sha.as_deref(), Some("head1"));

        // The DEEP tier (an @mention) round-trips too.
        let deep_id = create_explicit_task(
            &pool,
            &NewTask {
                repository_id: repo_id,
                installation_id: 99,
                webhook_delivery_id: "d1".to_string(),
                target_type: "pull_request".to_string(),
                target_id: 8,
                command_text: "@lightbridge review".to_string(),
                base_sha: None,
                head_sha: Some("head2".to_string()),
                run_epoch: 0,
                tier: "deep".to_string(),
                trigger_comment_id: Some(918_273),
            },
        )
        .await
        .unwrap();
        let deep_ctx = get_task_context(&pool, deep_id)
            .await
            .unwrap()
            .expect("deep task exists");
        assert_eq!(
            deep_ctx.tier, "deep",
            "an @mention review is the DEEP tier (ADR-0062)"
        );
        // ADR-0068: the trigger comment id round-trips through create → get_task_context, so the
        // lifecycle reactions can target the @mention comment.
        assert_eq!(
            deep_ctx.trigger_comment_id,
            Some(918_273),
            "the @mention trigger comment id is persisted and loaded (ADR-0068)"
        );

        assert!(
            get_task_context(&pool, Uuid::nil())
                .await
                .unwrap()
                .is_none(),
            "unknown id yields None"
        );
    }

    /// ADR-0037 accumulation: inline findings dedup by (file, line) last-write-wins, comments append
    /// in order, the summary is single-valued, and `clear_pending_review` empties the buffer.
    #[sqlx::test]
    async fn pending_review_actions_accumulate_dedup_and_clear(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let task_id = create_task(&pool, &pr_task(repo_id, "head1"))
            .await
            .unwrap()
            .unwrap();

        // Two inline findings on the same (file, line) → the second overwrites the first.
        upsert_pending_inline(
            &pool,
            task_id,
            "a.rs",
            7,
            Some("t1"),
            Some("P2"),
            Some("style"),
            None,
            "first",
        )
        .await
        .unwrap();
        upsert_pending_inline(
            &pool,
            task_id,
            "a.rs",
            7,
            Some("t1-refined"),
            Some("P0"),
            Some("security"),
            Some("let x = 1;"),
            "second, refined",
        )
        .await
        .unwrap();
        // A finding on a different line is kept separately.
        upsert_pending_inline(
            &pool,
            task_id,
            "a.rs",
            9,
            Some("t2"),
            Some("P1"),
            Some("correctness"),
            None,
            "other",
        )
        .await
        .unwrap();
        // Comments append; the summary is single-valued (last write wins).
        add_pending_comment(&pool, task_id, "first comment")
            .await
            .unwrap();
        add_pending_comment(&pool, task_id, "second comment")
            .await
            .unwrap();
        upsert_pending_summary(&pool, task_id, "draft summary")
            .await
            .unwrap();
        upsert_pending_summary(&pool, task_id, "final summary")
            .await
            .unwrap();

        let pending = load_pending_review(&pool, task_id).await.unwrap();
        assert_eq!(
            pending.inline.len(),
            2,
            "deduped to one row per (file, line)"
        );
        let line7 = pending.inline.iter().find(|f| f.line == 7).expect("line 7");
        assert_eq!(line7.body, "second, refined", "last write wins");
        assert_eq!(line7.priority.as_deref(), Some("P0"));
        assert_eq!(line7.suggestion.as_deref(), Some("let x = 1;"));
        assert_eq!(pending.comments, vec!["first comment", "second comment"]);
        assert_eq!(pending.summary.as_deref(), Some("final summary"));
        assert!(!pending.is_empty());

        clear_pending_review(&pool, task_id).await.unwrap();
        let after = load_pending_review(&pool, task_id).await.unwrap();
        assert!(after.is_empty(), "buffer cleared on restart/flush");
    }

    /// Phase 2 (ADR-0043): the refute pass retracts one buffered inline finding by (file, line),
    /// leaving the others; retracting a missing one is a no-op.
    #[sqlx::test]
    async fn delete_pending_inline_removes_one(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let task_id = create_task(&pool, &pr_task(repo_id, "head1"))
            .await
            .unwrap()
            .unwrap();
        for (line, body) in [(7, "keep"), (9, "drop me")] {
            upsert_pending_inline(
                &pool,
                task_id,
                "a.rs",
                line,
                Some("t"),
                Some("P1"),
                Some("correctness"),
                None,
                body,
            )
            .await
            .unwrap();
        }
        // Retract the line-9 finding; line 7 stays.
        delete_pending_inline(&pool, task_id, "a.rs", 9)
            .await
            .unwrap();
        let pending = load_pending_review(&pool, task_id).await.unwrap();
        assert_eq!(pending.inline.len(), 1, "only the retracted one is gone");
        assert_eq!(pending.inline[0].line, 7);
        // Retracting a non-existent finding is a harmless no-op.
        delete_pending_inline(&pool, task_id, "a.rs", 999)
            .await
            .unwrap();
        assert_eq!(
            load_pending_review(&pool, task_id)
                .await
                .unwrap()
                .inline
                .len(),
            1
        );
    }

    /// A terminal status stamps `completed_at` and clears the lease; `running` stamps `started_at`.
    /// `set_task_status` returns false for an unknown id (so the API can answer 404).
    #[sqlx::test]
    async fn set_task_status_stamps_and_releases(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let task = claim_after_create(&pool, repo_id, "head1").await;

        assert!(set_task_status(&pool, task, "succeeded", None)
            .await
            .unwrap());

        let row: (String, Option<OffsetDateTime>, Option<String>) =
            sqlx::query_as("SELECT status, completed_at, lease_owner FROM tasks WHERE id = $1")
                .bind(task)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0, "succeeded");
        assert!(row.1.is_some(), "terminal status stamps completed_at");
        assert!(row.2.is_none(), "terminal status clears the lease");

        assert!(
            !set_task_status(&pool, Uuid::nil(), "failed", None)
                .await
                .unwrap(),
            "unknown id reports no row updated"
        );
    }

    /// #137: a reported `detail` is persisted to `error_detail`, and a later report without one does
    /// not erase it (so a "posted nothing" reason recorded on success survives).
    #[sqlx::test]
    async fn set_task_status_persists_and_preserves_detail(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let task = claim_after_create(&pool, repo_id, "head1").await;

        // A report carrying a detail records it.
        assert!(set_task_status(
            &pool,
            task,
            "succeeded",
            Some("Review produced no comments to post.")
        )
        .await
        .unwrap());
        let detail: Option<String> =
            sqlx::query_scalar("SELECT error_detail FROM tasks WHERE id = $1")
                .bind(task)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            detail.as_deref(),
            Some("Review produced no comments to post.")
        );

        // A later report without a detail must not erase the recorded reason.
        assert!(set_task_status(&pool, task, "succeeded", None)
            .await
            .unwrap());
        let preserved: Option<String> =
            sqlx::query_scalar("SELECT error_detail FROM tasks WHERE id = $1")
                .bind(task)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            preserved.as_deref(),
            Some("Review produced no comments to post."),
            "a detail-less report preserves the earlier reason"
        );

        // A retry/restart transitions back to `running` — it must CLEAR the stale reason so a
        // now-succeeding attempt isn't still flagged with the previous failure's detail.
        assert!(set_task_status(&pool, task, "running", None).await.unwrap());
        let cleared: Option<String> =
            sqlx::query_scalar("SELECT error_detail FROM tasks WHERE id = $1")
                .bind(task)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            cleared, None,
            "a fresh `running` transition clears error_detail"
        );
    }

    /// Create then claim a task (claim sets it `running` with a lease) so status-transition tests
    /// start from the state a dispatched task is really in.
    async fn claim_after_create(pool: &PgPool, repo_id: i64, head: &str) -> Uuid {
        create_task(pool, &pr_task(repo_id, head))
            .await
            .unwrap()
            .unwrap();
        claim_next_task(pool, "owner-a", Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap()
            .id
    }

    /// Dimension of the `code_chunks.embedding` column under the migrations (migration 0005,
    /// `qwen3-embedding-8b`). Test vectors must match it or the `vector(N)` insert is rejected.
    const EMBED_DIM: usize = 4096;

    /// A one-hot vector sized to the embedding column (a 1.0 at `hot`, zeros elsewhere) — distinct
    /// directions give clean, predictable cosine ordering for the search test.
    fn one_hot(hot: usize) -> Vec<f32> {
        let mut v = vec![0.0_f32; EMBED_DIM];
        v[hot] = 1.0;
        v
    }

    fn chunk_at(file: &str, line: i32, hot: usize) -> CodeChunk {
        CodeChunk {
            file_path: file.to_string(),
            language: "rust".to_string(),
            chunk_type: "function".to_string(),
            symbol_name: Some(file.to_string()),
            start_line: line,
            end_line: line + 5,
            content: format!("// {file}"),
            embedding: one_hot(hot),
        }
    }

    /// Semantic search returns the nearest chunk first (cosine), scoped to the repo+commit, and
    /// honours the limit. Exercises the real pgvector `<=>` path (an exact cosine scan — 4096-dim
    /// vectors exceed pgvector's ANN limit, so migration 0005 carries no index).
    #[sqlx::test]
    async fn search_code_chunks_ranks_by_cosine_and_scopes(pool: PgPool) {
        let repo_id = seed(&pool).await;
        let chunks = vec![
            chunk_at("a.rs", 1, 0),
            chunk_at("b.rs", 1, 5),
            chunk_at("c.rs", 1, 9),
        ];
        upsert_code_chunks(&pool, repo_id, "headsha", &chunks)
            .await
            .unwrap();
        // A chunk on a *different* commit must not show up (scope check).
        upsert_code_chunks(&pool, repo_id, "othersha", &[chunk_at("a.rs", 1, 0)])
            .await
            .unwrap();

        // Query closest to the `hot=5` direction → b.rs ranks first with score ~1.0.
        let hits = search_code_chunks(&pool, repo_id, "headsha", &one_hot(5), 2)
            .await
            .unwrap();
        assert_eq!(hits.len(), 2, "limit honoured");
        assert_eq!(hits[0].file_path, "b.rs");
        assert!(
            hits[0].score > 0.99,
            "exact direction ~1.0, got {}",
            hits[0].score
        );
        assert!(hits[0].score >= hits[1].score, "ordered by similarity");

        // Only this commit's chunks are searched (othersha excluded).
        let all = search_code_chunks(&pool, repo_id, "headsha", &one_hot(0), 50)
            .await
            .unwrap();
        assert_eq!(
            all.len(),
            3,
            "scoped to (repo, headsha) — othersha not included"
        );
    }

    /// `latest_indexed_commit` returns the most-recently-indexed snapshot (ADR-0050): `None` for an
    /// un-indexed repo, and the newest `commit_sha` once chunks exist — the single anchor reviews reuse
    /// and pin retrieval to, so the skip decision and the search scope can't disagree (no hollow index).
    #[sqlx::test]
    async fn latest_indexed_commit_returns_newest_snapshot(pool: PgPool) {
        let repo_id = seed(&pool).await;

        // Never indexed → None.
        assert_eq!(latest_indexed_commit(&pool, repo_id).await.unwrap(), None);

        // Index an older snapshot, then a newer one. Determinism does NOT rely on the wall clock: under
        // `#[sqlx::test]` `now()` can be identical for both inserts, so the `id DESC` tie-break (the
        // second insert has the higher BIGSERIAL id) is what guarantees "newer-sha" wins.
        upsert_code_chunks(&pool, repo_id, "base-sha", &[chunk_at("a.rs", 1, 0)])
            .await
            .unwrap();
        upsert_code_chunks(&pool, repo_id, "newer-sha", &[chunk_at("b.rs", 1, 0)])
            .await
            .unwrap();

        // Returns the most recent snapshot — what retrieval pins to (it provably has chunks).
        assert_eq!(
            latest_indexed_commit(&pool, repo_id)
                .await
                .unwrap()
                .as_deref(),
            Some("newer-sha")
        );
        // A different repo is unaffected.
        assert_eq!(
            latest_indexed_commit(&pool, repo_id + 9999).await.unwrap(),
            None
        );
    }

    /// Index pruning (ADR-0052): the keep-set is the latest snapshot ∪ any commit an in-flight
    /// (non-terminal) task pins; `prune_code_chunks` drops everything else (past the recency grace),
    /// and an empty keep-set is a no-op so a live index is never wiped.
    #[sqlx::test]
    async fn prune_keeps_latest_and_in_flight_and_drops_the_rest(pool: PgPool) {
        let repo_id = seed(&pool).await;

        // Three snapshots, oldest → newest; the last (`latest-sha`) wins `latest_indexed_commit` via
        // the `id DESC` tie-break.
        for (sha, file) in [
            ("stale-sha", "a.rs"),
            ("inflight-sha", "b.rs"),
            ("latest-sha", "c.rs"),
        ] {
            upsert_code_chunks(&pool, repo_id, sha, &[chunk_at(file, 1, 0)])
                .await
                .unwrap();
        }
        // Age every snapshot past the 10-minute recency grace so they're eligible to prune.
        sqlx::query("UPDATE code_chunks SET created_at = now() - interval '1 hour' WHERE repository_id = $1")
            .bind(repo_id)
            .execute(&pool)
            .await
            .unwrap();
        // An in-flight (status 'queued') review pins `inflight-sha`.
        create_task(&pool, &pr_task(repo_id, "inflight-sha"))
            .await
            .unwrap()
            .unwrap();

        // Keep-set the sweeper assembles.
        assert_eq!(
            in_use_commits(&pool, repo_id).await.unwrap(),
            vec!["inflight-sha".to_string()]
        );
        assert_eq!(
            latest_indexed_commit(&pool, repo_id)
                .await
                .unwrap()
                .as_deref(),
            Some("latest-sha")
        );
        assert_eq!(
            repos_with_stale_snapshots(&pool).await.unwrap(),
            vec![repo_id]
        );

        // Prune everything outside the keep-set: only `stale-sha` goes.
        let keep = vec!["inflight-sha".to_string(), "latest-sha".to_string()];
        assert_eq!(prune_code_chunks(&pool, repo_id, &keep).await.unwrap(), 1);
        let remaining: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT commit_sha FROM code_chunks WHERE repository_id = $1 ORDER BY commit_sha",
        )
        .bind(repo_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            remaining,
            vec!["inflight-sha".to_string(), "latest-sha".to_string()]
        );

        // Safety: an empty keep-set never deletes (guards against wiping a live index).
        assert_eq!(prune_code_chunks(&pool, repo_id, &[]).await.unwrap(), 0);
    }

    /// The sweeper gates a whole repo while an `index` task runs (ADR-0052): an index task carries a
    /// NULL `head_sha`, so it never appears in `in_use_commits` — `has_active_index_task` is what
    /// protects the snapshot it's mid-writing (and the Neo4j graph, which has no recency grace).
    #[sqlx::test]
    async fn has_active_index_task_gates_a_repo_mid_index(pool: PgPool) {
        let repo_id = seed(&pool).await;

        // A review task in flight is NOT an index task → gate stays open, and the index commit it
        // would write isn't covered by in_use_commits.
        create_task(&pool, &pr_task(repo_id, "review-head"))
            .await
            .unwrap()
            .unwrap();
        assert!(!has_active_index_task(&pool, repo_id).await.unwrap());

        // A queued index task (NULL head_sha) → gate closes; in_use_commits still doesn't list it.
        create_index_task(&pool, repo_id, 99)
            .await
            .unwrap()
            .unwrap();
        assert!(has_active_index_task(&pool, repo_id).await.unwrap());
        assert_eq!(
            in_use_commits(&pool, repo_id).await.unwrap(),
            vec!["review-head".to_string()],
            "index task's (NULL) head_sha is not in the keep-set — the gate is its only protection"
        );
    }
}
