//! A2A push-notification configs (RFC-0006 Phase 3, ADR-0079 §1) and their delivery — the `notifier`
//! role (ADR-0079 §4) — plus the terminal-A2A-task retention sweep (ADR-0077 §S3 / #321). Split out of
//! the former monolithic `db.rs` (ADR-0086 follow-up) — pure move, no behavior change.

use std::time::Duration;

use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// A2A push-notification configs (RFC-0006 Phase 3, ADR-0079 §1)
// ---------------------------------------------------------------------------

/// A stored A2A push-notification config (ADR-0079 §1), the subset of `a2a_push_configs` the CRUD
/// handler needs. The delivery-cursor / lease columns (`next_attempt_at`, `lease_*`) are the
/// notifier's (slice 2b) and are not projected here.
///
/// These queries are keyed on `config_id` / `a2a_task_id` and are **not** caller-scoped in SQL: the
/// handler proves task ownership first (via [`crate::a2a::store::PgTaskStore::load_owned`] on the
/// parent `a2a_tasks` row, exactly like `GetTask`) and only then reads/writes a config, verifying the
/// config's `a2a_task_id` matches the proven-owned task. That keeps the ownership check in one place
/// (the store) rather than duplicating the caller-id join here.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PushConfigRow {
    pub config_id: Uuid,
    pub a2a_task_id: Uuid,
    pub url: String,
    /// Caller-supplied auth token, **encrypted at rest** (ADR-0079 §3): the AEAD `nonce || ciphertext
    /// || tag` produced by `crate::a2a::push_crypto` (ChaCha20-Poly1305). NULL when the caller
    /// registered no token. Opaque bytes at this layer — the handler decrypts with the role key on
    /// read. Never logged.
    pub token_enc: Option<Vec<u8>>,
    // The delivery-state columns are populated at insert (table defaults) and asserted by this
    // slice's tests, but the CRUD handler does not yet READ them — the notifier (slice 2b) consumes
    // `delivered_seq`/`attempts`/`state` to drive the cursor, backoff, and dead-lettering. Kept on the
    // typed row now so the projection is complete; `allow(dead_code)` until slice 2b wires the reader.
    #[allow(dead_code)]
    pub delivered_seq: i64,
    #[allow(dead_code)]
    pub attempts: i32,
    #[allow(dead_code)]
    pub state: String,
    #[allow(dead_code)]
    pub created_by: String,
}

/// Insert a new push-notification config for a task (ADR-0079 §1). The caller MUST already have proven
/// ownership of `a2a_task_id` (the handler does, via `load_owned`, before calling). `delivered_seq`
/// (0), `attempts` (0), `next_attempt_at` (`now()`), and `state` (`active`) take their table defaults.
pub async fn insert_push_config(
    pool: &PgPool,
    config_id: Uuid,
    a2a_task_id: Uuid,
    url: &str,
    token_enc: Option<&[u8]>,
    created_by: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO a2a_push_configs (config_id, a2a_task_id, url, token_enc, created_by) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(config_id)
    .bind(a2a_task_id)
    .bind(url)
    .bind(token_enc)
    .bind(created_by)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Fetch a single push config by its id (or `None` if unknown). The handler verifies the returned
/// `a2a_task_id` matches the task it already proved the caller owns (ADR-0079 §1).
pub async fn get_push_config(
    pool: &PgPool,
    config_id: Uuid,
) -> Result<Option<PushConfigRow>, sqlx::Error> {
    sqlx::query_as::<_, PushConfigRow>(
        "SELECT config_id, a2a_task_id, url, token_enc, delivered_seq, attempts, state, created_by \
         FROM a2a_push_configs \
         WHERE config_id = $1",
    )
    .bind(config_id)
    .fetch_optional(pool)
    .await
}

/// List every push config registered on a task, oldest first. Caller-scoping is the handler's job
/// (it proves ownership of `a2a_task_id` first).
pub async fn list_push_configs_for_task(
    pool: &PgPool,
    a2a_task_id: Uuid,
) -> Result<Vec<PushConfigRow>, sqlx::Error> {
    sqlx::query_as::<_, PushConfigRow>(
        "SELECT config_id, a2a_task_id, url, token_enc, delivered_seq, attempts, state, created_by \
         FROM a2a_push_configs \
         WHERE a2a_task_id = $1 ORDER BY created_at ASC, config_id ASC",
    )
    .bind(a2a_task_id)
    .fetch_all(pool)
    .await
}

/// Delete a push config, scoped to its owning task in one query. Returns whether a row was removed
/// (`false` = wrong task / unknown id / already gone), so the handler maps a miss to `TaskNotFound`
/// without a prior existence SELECT — the caller-scoping is `config_id = $1 AND a2a_task_id = $2`.
pub async fn delete_push_config(
    pool: &PgPool,
    config_id: Uuid,
    a2a_task_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let result =
        sqlx::query("DELETE FROM a2a_push_configs WHERE config_id = $1 AND a2a_task_id = $2")
            .bind(config_id)
            .bind(a2a_task_id)
            .execute(pool)
            .await?;
    Ok(result.rows_affected() > 0)
}

// ---------------------------------------------------------------------------
// A2A push-notification delivery — the `notifier` role (RFC-0006 Phase 3, ADR-0079 §4)
// ---------------------------------------------------------------------------
//
// The `a2a_task_events` log (migration 0026) IS the durable queue; each `a2a_push_configs` row
// carries its own `delivered_seq` cursor into it. The notifier claims a due config under a lease
// (single in-flight delivery per config across replicas — ADR-0079 P5), delivers each event past the
// cursor in `seq` order, and advances the cursor on success / backs off on failure / dead-letters
// after too many failures. Every write here is per-config, not per-event-per-subscriber (P10).

/// A push config claimed for delivery: the subset the notifier's delivery loop needs. `token_enc`
/// stays opaque (the loop decrypts it with the role key); `delivered_seq` seeds the cursor. The
/// consecutive-failure counter is NOT projected here — a failed attempt reads the authoritative
/// post-increment value straight from [`bump_push_attempts`], avoiding a stale copy.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ClaimedPushConfig {
    pub config_id: Uuid,
    pub a2a_task_id: Uuid,
    pub url: String,
    pub token_enc: Option<Vec<u8>>,
    pub delivered_seq: i64,
}

/// Atomically claim the next `active` push config with work **due** and take a delivery lease
/// (ADR-0079 §4). "Due" = it has an undelivered event (`delivered_seq < MAX(seq)` for its task) AND
/// `next_attempt_at <= now()`. The lease (`lease_owner`/`lease_expires_at`) is what serializes
/// delivery: a config whose lease is still live is skipped, so exactly one worker delivers a given
/// config at a time (no double-send across replicas, order preserved — P5). `FOR UPDATE SKIP LOCKED`
/// makes the claim itself race-free across concurrent claimers; the lease then holds across the
/// (network) delivery that follows, after the claim's row lock is released on commit. Returns `None`
/// when nothing is due.
pub async fn claim_next_push_config(
    pool: &PgPool,
    owner: &str,
    lease: Duration,
) -> Result<Option<ClaimedPushConfig>, sqlx::Error> {
    sqlx::query_as::<_, ClaimedPushConfig>(
        "UPDATE a2a_push_configs \
         SET lease_owner = $1, lease_expires_at = now() + ($2 * interval '1 second') \
         WHERE config_id = ( \
           SELECT c.config_id FROM a2a_push_configs c \
           WHERE c.state = 'active' \
             AND c.next_attempt_at <= now() \
             AND (c.lease_expires_at IS NULL OR c.lease_expires_at < now()) \
             AND c.delivered_seq < ( \
               SELECT COALESCE(MAX(e.seq), 0) FROM a2a_task_events e \
               WHERE e.a2a_task_id = c.a2a_task_id \
             ) \
           ORDER BY c.next_attempt_at \
           FOR UPDATE SKIP LOCKED \
           LIMIT 1 \
         ) \
         RETURNING config_id, a2a_task_id, url, token_enc, delivered_seq",
    )
    .bind(owner)
    .bind(lease.as_secs_f64())
    .fetch_optional(pool)
    .await
}

/// The next event to deliver for a task — the lowest `seq` strictly greater than the cursor. Returns
/// `(seq, payload)` or `None` when the config is caught up. The payload is the serialized
/// `StreamResponse` (statusUpdate / artifactUpdate) the streaming tail also emits, so push and stream
/// deliver the identical body.
pub async fn next_push_event(
    pool: &PgPool,
    a2a_task_id: Uuid,
    after_seq: i64,
) -> Result<Option<(i64, Value)>, sqlx::Error> {
    let row: Option<(i64, Value)> = sqlx::query_as(
        "SELECT seq, payload FROM a2a_task_events \
         WHERE a2a_task_id = $1 AND seq > $2 ORDER BY seq LIMIT 1",
    )
    .bind(a2a_task_id)
    .bind(after_seq)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Advance the delivery cursor after a successful POST: set `delivered_seq`, reset `attempts` to 0,
/// clear the backoff, and **renew the lease** so the worker keeps delivering this config's remaining
/// events without another replica stealing it mid-catch-up (ADR-0079 §4).
pub async fn advance_push_delivered(
    pool: &PgPool,
    config_id: Uuid,
    seq: i64,
    lease: Duration,
) -> Result<(), sqlx::Error> {
    // `AND delivered_seq < $2` keeps the cursor strictly monotonic: if this worker's lease expired
    // mid-delivery and another worker re-claimed the config and advanced past $2, this stale write
    // is a no-op (it neither rewinds the cursor nor renews a lease it no longer holds). Without the
    // guard a >lease-duration stall could rewind delivered_seq and re-send an already-delivered event
    // out of order.
    sqlx::query(
        "UPDATE a2a_push_configs \
         SET delivered_seq = $2, attempts = 0, next_attempt_at = now(), \
             lease_expires_at = now() + ($3 * interval '1 second') \
         WHERE config_id = $1 AND delivered_seq < $2",
    )
    .bind(config_id)
    .bind(seq)
    .bind(lease.as_secs_f64())
    .execute(pool)
    .await
    .map(|_| ())
}

/// Release a caught-up config's lease (no undelivered events remain). Clears `lease_owner`/
/// `lease_expires_at` so the row is idle; it will not be re-claimed until a new event lands (the
/// claim's `delivered_seq < MAX(seq)` predicate is then false). `attempts` is already 0 on this path.
pub async fn release_push_config(pool: &PgPool, config_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE a2a_push_configs \
         SET lease_owner = NULL, lease_expires_at = NULL WHERE config_id = $1",
    )
    .bind(config_id)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Increment a config's consecutive-failure counter and return the new value — step 1 of a failed
/// delivery (ADR-0079 §4). The **lease is intentionally kept** so no other worker can claim this
/// config in the window before [`schedule_push_retry`] sets its next attempt time (which would
/// duplicate-send the same event). The caller computes the backoff / dead-letter decision from the
/// returned count and then calls [`schedule_push_retry`].
pub async fn bump_push_attempts(pool: &PgPool, config_id: Uuid) -> Result<i32, sqlx::Error> {
    sqlx::query_scalar(
        "UPDATE a2a_push_configs SET attempts = attempts + 1 WHERE config_id = $1 RETURNING attempts",
    )
    .bind(config_id)
    .fetch_one(pool)
    .await
}

/// Schedule a config's next delivery attempt after a failure — step 2, releasing the lease (ADR-0079
/// §4). Pushes `next_attempt_at` out by the caller-computed `backoff` and clears the lease so the
/// config is re-claimable once the backoff elapses. When `disable` is set (dead-letter: too many
/// consecutive failures) the config is moved to `state = 'disabled'` and stops being claimed at all —
/// the caller can re-create/re-enable it (P7).
pub async fn schedule_push_retry(
    pool: &PgPool,
    config_id: Uuid,
    backoff: Duration,
    disable: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE a2a_push_configs \
         SET next_attempt_at = now() + ($2 * interval '1 second'), \
             state = CASE WHEN $3 THEN 'disabled' ELSE state END, \
             lease_owner = NULL, lease_expires_at = NULL \
         WHERE config_id = $1",
    )
    .bind(config_id)
    .bind(backoff.as_secs_f64())
    .bind(disable)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Count the **active** push configs a caller has registered on one task (ADR-0079 P7). The handler
/// enforces a per-caller, per-task cap with this before an `insert_push_config`, so a single caller
/// cannot register an unbounded fan-out of webhooks on one task. Dead-lettered (`disabled`) configs
/// are excluded so a caller can always replace ones that were disabled by repeated delivery failure.
/// Caller-scoping of the *task* is the handler's job (it `load_owned`s first); this narrows to the
/// caller's own configs via `created_by` so one caller's configs never count against another's.
pub async fn count_active_push_configs_for_caller(
    pool: &PgPool,
    a2a_task_id: Uuid,
    created_by: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM a2a_push_configs \
         WHERE a2a_task_id = $1 AND created_by = $2 AND state = 'active'",
    )
    .bind(a2a_task_id)
    .bind(created_by)
    .fetch_one(pool)
    .await
}

/// Retention sweep for terminal A2A task mappings (ADR-0077 §S3 / #321). `a2a_tasks` (and, via
/// `ON DELETE CASCADE`, its `a2a_task_events` and `a2a_push_configs`) is otherwise append-only — every
/// A2A submission leaves a permanent mapping row plus its whole event log. Left alone it grows without
/// bound. This deletes **terminal** mappings older than `ttl_days`, a bounded `batch` per call so one
/// sweep never locks the table for long; the dispatcher's slow GC tick calls it repeatedly.
///
/// A mapping is treated as terminal when its append-only event log is **frozen** — a `final = true`
/// event exists (every COMPLETED / FAILED / CANCELED / REJECTED outcome appends one; ADR-0077) — OR its
/// last-persisted snapshot `state` is a terminal wire value (the belt-and-braces case where a
/// best-effort terminal-event append was lost, e.g. a REJECTED gate outcome). A still-running
/// (SUBMITTED / WORKING) mapping has neither and is **never** swept, whatever its age — the load-bearing
/// safety property (retention must never reap a live task out from under a subscriber/poller). The age
/// is anchored on `created_at`: an A2A review's whole lifetime is bounded by the deep-run cap (≤ ~2 h,
/// ADR-0062) plus the 3 h stream backstop, so `created_at` is within hours of completion while `ttl_days`
/// is in days — the distinction is immaterial and `created_at` cannot be newer than the true completion.
///
/// A non-positive `ttl_days` is a **skip** (returns 0), never "delete everything" — `now() -
/// make_interval(days => 0)` is `now()`, which would match every terminal row. A non-positive `batch`
/// likewise skips. Returns the number of mappings deleted.
pub async fn sweep_terminal_a2a_tasks(
    pool: &PgPool,
    ttl_days: i64,
    batch: i64,
) -> Result<u64, sqlx::Error> {
    if ttl_days <= 0 || batch <= 0 {
        return Ok(0);
    }
    let deleted = sqlx::query(
        "DELETE FROM a2a_tasks \
         WHERE a2a_task_id IN ( \
           SELECT t.a2a_task_id FROM a2a_tasks t \
           WHERE t.created_at < now() - make_interval(days => $1::int) \
             AND ( \
               EXISTS (SELECT 1 FROM a2a_task_events e \
                       WHERE e.a2a_task_id = t.a2a_task_id AND e.final) \
               OR t.state IN ( \
                 'TASK_STATE_COMPLETED', 'TASK_STATE_FAILED', \
                 'TASK_STATE_CANCELED', 'TASK_STATE_REJECTED' \
               ) \
             ) \
           ORDER BY t.created_at \
           LIMIT $2 \
         )",
    )
    .bind(ttl_days)
    .bind(batch)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(deleted)
}
