//! Production of the A2A streaming event log (RFC-0006 Phase 2, ADR-0077).
//!
//! The `a2a` role only *reads* the [`a2a_task_events`](../../migrations/0026_a2a_task_events.sql)
//! table; this module is the *write* side — a projection of state transitions the pipeline already
//! performs, appended by the control plane. Nothing here launches a Job or touches a forge (ADR-0029
//! holds): it turns a `tasks.status` flip (or a REJECTED/CANCELED gate outcome) into an ordered,
//! per-A2A-task event a subscriber can replay-then-tail.
//!
//! ## Ordering & concurrency (the R6 guarantee)
//!
//! `seq` is `COALESCE(MAX(seq), 0) + 1 WHERE a2a_task_id = $1`, assigned **inside** the appending
//! transaction. The two producing paths are serialized per underlying `tasks` row:
//!
//! - [`append_transition_events`] rides the `set_task_status` transaction, whose `UPDATE tasks … WHERE
//!   id = $1` holds the row write-lock until commit — so two status flips on the same task cannot
//!   interleave their appends.
//! - [`append_terminal_status`] (the REJECTED / CANCELED gate outcomes, which bypass `set_task_status`)
//!   takes the *same* `tasks` row lock first when an underlying run exists, serializing it against the
//!   status path.
//!
//! Every append is also guarded by [`has_final`]: once a `final = true` event exists for an
//! `a2a_task_id`, the log is frozen — no event can ever follow the terminal one (so a late re-finalize,
//! a runner re-report, or a cancel racing a completion can never append *after* the terminal event,
//! and a subscriber that closes on `final` never misses a later event). A duplicate `seq` remains a
//! hard PK violation (retryable), never a silent gap.

use a2a::{
    Artifact, StreamResponse, TaskArtifactUpdateEvent, TaskState, TaskStatus, TaskStatusUpdateEvent,
};
use serde_json::Value;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::mapping::{ReviewContext, review_artifacts, task_state_from_status};

/// Postgres `LISTEN`/`NOTIFY` channel prefix for a per-A2A-task stream. The SSE tail loop listens on
/// `{prefix}{a2a_task_id}`; producers `NOTIFY` it as a *wake hint* only (the `seq`-cursor SELECT is
/// authoritative, so a missed notify costs latency, never a lost or misordered event).
const CHANNEL_PREFIX: &str = "a2a_task_events:";

/// The per-task `NOTIFY` channel name. Fits Postgres' 63-byte identifier limit (16-byte prefix + a
/// 36-byte UUID = 52 bytes).
pub(crate) fn task_channel(a2a_task_id: &Uuid) -> String {
    format!("{CHANNEL_PREFIX}{a2a_task_id}")
}

/// Serialize a [`TaskState`] to its SCREAMING_SNAKE wire value (e.g. `TASK_STATE_WORKING`).
fn state_wire(state: &TaskState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "TASK_STATE_UNSPECIFIED".to_string())
}

/// The wrapped `StreamResponse` body for a status-update event (what a subscriber emits verbatim).
fn status_update_payload(a2a_task_id: &Uuid, context_id: &str, state: TaskState) -> Value {
    let event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: a2a_task_id.to_string(),
        context_id: context_id.to_string(),
        status: TaskStatus {
            state,
            message: None,
            timestamp: None,
        },
        metadata: None,
    });
    serde_json::to_value(event).unwrap_or(Value::Null)
}

/// The wrapped `StreamResponse` body for an artifact-update event.
fn artifact_update_payload(a2a_task_id: &Uuid, context_id: &str, artifact: Artifact) -> Value {
    let event = StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
        task_id: a2a_task_id.to_string(),
        context_id: context_id.to_string(),
        artifact,
        append: None,
        last_chunk: Some(true),
        metadata: None,
    });
    serde_json::to_value(event).unwrap_or(Value::Null)
}

/// Whether a terminal (`final = true`) event already exists for this A2A task. Once true, the log is
/// frozen (no event may follow the terminal one).
async fn has_final(conn: &mut PgConnection, a2a_task_id: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM a2a_task_events WHERE a2a_task_id = $1 AND final)",
    )
    .bind(a2a_task_id)
    .fetch_one(conn)
    .await
}

/// Append one event, assigning `seq = COALESCE(MAX(seq), 0) + 1` for the task inside the caller's
/// transaction. A duplicate `seq` (should never happen given the serialization above) is a PK
/// violation the caller surfaces as a retryable error, never a silent gap.
async fn append_event(
    conn: &mut PgConnection,
    a2a_task_id: Uuid,
    kind: &str,
    state: Option<&str>,
    final_: bool,
    payload: &Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO a2a_task_events (a2a_task_id, seq, kind, state, final, payload) \
         VALUES ($1, \
                 (SELECT COALESCE(MAX(seq), 0) + 1 FROM a2a_task_events WHERE a2a_task_id = $1), \
                 $2, $3, $4, $5)",
    )
    .bind(a2a_task_id)
    .bind(kind)
    .bind(state)
    .bind(final_)
    .bind(payload)
    .execute(conn)
    .await
    .map(|_| ())
}

/// Emit `NOTIFY` on the task's stream channel inside the transaction (delivered on commit).
async fn notify(conn: &mut PgConnection, a2a_task_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(task_channel(&a2a_task_id))
        .bind(a2a_task_id.to_string())
        .execute(conn)
        .await
        .map(|_| ())
}

/// Build the completed-review artifact (summary + findings + review context) for `underlying_id`, or
/// `None` if no review row is persisted yet. Mirrors the `GetTask` completed branch so streaming and
/// polling deliver the *same* artifact.
///
/// NOTE (ADR-0077 timing): the `reviews` row for a *posted* review lands asynchronously via the
/// reconciler (`upsert_review`), which typically runs **after** the runner reports `succeeded`. So on
/// the common posting path the artifact is not yet available when the terminal `COMPLETED` event is
/// appended, and the stream closes without it (the caller retrieves it with a follow-up `GetTask`, per
/// the ADR's "mix streaming and polling" contract). On the silent-clean path (ADR-0068), the review
/// row is written by `insert_review_if_absent` *before* `succeeded`, so the artifact IS streamed.
async fn completed_artifact(
    conn: &mut PgConnection,
    underlying_id: Uuid,
) -> Result<Option<Artifact>, sqlx::Error> {
    let review: Option<(String, Value, Option<String>)> =
        sqlx::query_as("SELECT summary, findings, review_url FROM reviews WHERE task_id = $1")
            .bind(underlying_id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((summary, findings, review_url)) = review else {
        return Ok(None);
    };

    // The effective SHAs / repo / pr for the context part; the row may already be reaped, in which case
    // the context echoes only the review_url (ReviewContext handles the nulls). Columns:
    // (repo owner, repo name, pr/target_id, base_sha, head_sha).
    type ContextRow = (
        Option<String>,
        Option<String>,
        i64,
        Option<String>,
        Option<String>,
    );
    let ctx: Option<ContextRow> = sqlx::query_as(
        "SELECT r.owner, r.name, t.target_id, t.base_sha, t.head_sha \
             FROM tasks t LEFT JOIN repositories r ON r.id = t.repository_id WHERE t.id = $1",
    )
    .bind(underlying_id)
    .fetch_optional(&mut *conn)
    .await?;

    let context = match ctx {
        Some((owner, name, target_id, base_sha, head_sha)) => ReviewContext {
            repo: match (owner, name) {
                (Some(o), Some(n)) => Some(format!("{o}/{n}")),
                _ => None,
            },
            pr: Some(target_id),
            base_sha,
            head_sha,
            review_url,
        },
        None => ReviewContext {
            review_url,
            ..Default::default()
        },
    };

    Ok(review_artifacts(&summary, &findings, &context)
        .into_iter()
        .next())
}

/// Append A2A stream events for a `tasks.status` transition, for every A2A task that fronts
/// `underlying_id`. Runs **inside** the `set_task_status` transaction (see [`crate::db::set_task_status`]),
/// so an event exists for every transition a poller could observe — streaming and polling can never
/// disagree. A non-A2A task (no `a2a_tasks` row) appends nothing.
///
/// On a `succeeded` transition, a completed-review artifact-update is appended **before** the terminal
/// `COMPLETED` status-update (lower `seq`) when the review row is already persisted — so a subscriber
/// sees the artifact, then the terminal event, then closes.
pub async fn append_transition_events(
    conn: &mut PgConnection,
    underlying_id: Uuid,
    status: &str,
) -> Result<(), sqlx::Error> {
    let fronts: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT a2a_task_id, context_id FROM a2a_tasks WHERE underlying_task_id = $1",
    )
    .bind(underlying_id)
    .fetch_all(&mut *conn)
    .await?;
    if fronts.is_empty() {
        return Ok(());
    }

    let mapped = task_state_from_status(status);
    let is_terminal = mapped.is_terminal();
    let wire = state_wire(&mapped);

    // Load the completed artifact once (only relevant on the succeeded → COMPLETED transition).
    let artifact = if mapped == TaskState::Completed {
        completed_artifact(&mut *conn, underlying_id).await?
    } else {
        None
    };

    for (a2a_task_id, context_id) in fronts {
        // The log is frozen once terminal: never append after a final event (idempotent re-reports, a
        // cancel racing a completion, etc. all no-op here).
        if has_final(&mut *conn, a2a_task_id).await? {
            continue;
        }

        if let Some(artifact) = &artifact {
            let payload = artifact_update_payload(&a2a_task_id, &context_id, artifact.clone());
            append_event(
                &mut *conn,
                a2a_task_id,
                "artifact-update",
                None,
                false,
                &payload,
            )
            .await?;
        }

        let payload = status_update_payload(&a2a_task_id, &context_id, mapped.clone());
        append_event(
            &mut *conn,
            a2a_task_id,
            "status-update",
            Some(&wire),
            is_terminal,
            &payload,
        )
        .await?;

        notify(&mut *conn, a2a_task_id).await?;
    }
    Ok(())
}

/// Append a single terminal `status-update` for one A2A task that never flows through
/// `set_task_status`: a submission REJECTED at the gate (no underlying run) or a caller CANCEL (the
/// underlying flip bypasses `set_task_status`). Best-effort — a failure is logged by the caller and does
/// not fail the request, exactly like the mapping snapshot it accompanies.
///
/// When `underlying` is `Some`, the `tasks` row is locked `FOR UPDATE` first so this serializes against
/// a concurrent `set_task_status` append on the same run (the `has_final` guard then makes whichever
/// runs second a no-op).
pub async fn append_terminal_status(
    pool: &PgPool,
    a2a_task_id: Uuid,
    context_id: &str,
    state: TaskState,
    underlying: Option<Uuid>,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;

    if let Some(underlying) = underlying {
        // Take the same row lock set_task_status holds, so the two producers can't interleave.
        sqlx::query("SELECT id FROM tasks WHERE id = $1 FOR UPDATE")
            .bind(underlying)
            .fetch_optional(&mut *tx)
            .await?;
    }

    if has_final(&mut tx, a2a_task_id).await? {
        tx.commit().await?;
        return Ok(());
    }

    let wire = state_wire(&state);
    let payload = status_update_payload(&a2a_task_id, context_id, state);
    append_event(
        &mut tx,
        a2a_task_id,
        "status-update",
        Some(&wire),
        true,
        &payload,
    )
    .await?;
    notify(&mut tx, a2a_task_id).await?;
    tx.commit().await
}

/// Fetch the ordered event payloads for a task with `seq > after_seq` (the replay/tail cursor). Returns
/// `(seq, payload, final)` in `seq` order — the sole ordering authority (never wall-clock).
pub async fn fetch_events_after(
    pool: &PgPool,
    a2a_task_id: Uuid,
    after_seq: i64,
) -> Result<Vec<(i64, Value, bool)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT seq, payload, final FROM a2a_task_events \
         WHERE a2a_task_id = $1 AND seq > $2 ORDER BY seq",
    )
    .bind(a2a_task_id)
    .bind(after_seq)
    .fetch_all(pool)
    .await
}
