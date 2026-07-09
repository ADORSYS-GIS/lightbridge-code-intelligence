//! The Postgres-backed A2A [`TaskStore`] (RFC-0006 Phase 1) — the load-bearing optimistic-concurrency
//! layer.
//!
//! ## Why CAS, and how it works without an expected-version arg
//!
//! The SDK's `TaskStore::update(task)` takes **no expected version** (issue #299): it is a blind
//! upsert. Under horizontal scaling, two `a2a` replicas can each read a task, mutate their copy, and
//! write back — the second write silently clobbering the first (a lost update). We enforce optimistic
//! concurrency at the DB layer instead.
//!
//! The trick is to round-trip the version *through the task object itself*. [`PgTaskStore::get`]
//! stamps the row's current `version` into the returned task's metadata under [`LB_VERSION`]. When the
//! caller later mutates that task and calls [`PgTaskStore::update`], the expected version rides along
//! in the metadata, and the UPDATE is `... WHERE version = <expected>` with a `version + 1` bump. If a
//! concurrent writer already advanced the row, zero rows match and `update` returns a conflict error
//! **instead of overwriting** — so a lost update fails loudly. This survives across replicas because
//! the expected version travels with the (serialized) task, not in any per-process cache.
//!
//! This is a different, lower layer than `run_epoch` (review-run idempotency on `tasks`); the two are
//! deliberately not conflated.

use a2a::{error_code, A2AError, ListTasksRequest, ListTasksResponse, Task, TaskState};
use a2a_server::task_store::{TaskStore, TaskVersion};
use async_trait::async_trait;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

/// Metadata key carrying the caller's OIDC identity on a stored task. Set by the handler on `create`.
pub const LB_CALLER: &str = "lb.callerId";
/// Metadata key carrying the A2A skill (`review`). Set by the handler on `create`.
pub const LB_SKILL: &str = "lb.skill";
/// Metadata key carrying the underlying `tasks.id` (uuid) this A2A task fronts, if any.
pub const LB_UNDERLYING: &str = "lb.underlyingTaskId";
/// Transient metadata key: the optimistic-concurrency version. Stamped by [`PgTaskStore::get`] and
/// consumed by [`PgTaskStore::update`] for CAS. Never persisted (stripped before write).
pub const LB_VERSION: &str = "lb.version";

/// A caller-scoped view of a stored mapping row — what the handler needs to serve GetTask/CancelTask
/// without exposing another caller's rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    pub context_id: String,
    pub skill: String,
    /// The underlying review task, or `None` for a REJECTED submission.
    pub underlying_task_id: Option<Uuid>,
    /// Last-persisted A2A wire state (SCREAMING_SNAKE).
    pub state: String,
}

/// Postgres-backed A2A task store. Cheap to clone (wraps a pooled handle).
#[derive(Clone)]
pub struct PgTaskStore {
    pool: PgPool,
}

impl PgTaskStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Load a caller-owned mapping row. Scoped to `caller_id` in SQL, so one caller can never read
    /// another's task — an unknown-or-not-owned id reads as `None` (→ `TaskNotFoundError`), which also
    /// avoids leaking the existence of other callers' tasks.
    pub async fn load_owned(
        &self,
        a2a_task_id: Uuid,
        caller_id: &str,
    ) -> Result<Option<Mapping>, sqlx::Error> {
        let row: Option<(String, String, Option<Uuid>, String)> = sqlx::query_as(
            "SELECT context_id, skill, underlying_task_id, state \
             FROM a2a_tasks WHERE a2a_task_id = $1 AND caller_id = $2",
        )
        .bind(a2a_task_id)
        .bind(caller_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(
            row.map(|(context_id, skill, underlying_task_id, state)| Mapping {
                context_id,
                skill,
                underlying_task_id,
                state,
            }),
        )
    }

    /// Count a caller's submissions of `skill` within the last `window_secs` seconds — the per-identity
    /// rate limit (RFC-0006 R4). DB-backed so the limit holds across replicas, not per-pod.
    pub async fn count_recent(
        &self,
        caller_id: &str,
        skill: &str,
        window_secs: i64,
    ) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT count(*) FROM a2a_tasks \
             WHERE caller_id = $1 AND skill = $2 \
               AND created_at > now() - make_interval(secs => $3::double precision)",
        )
        .bind(caller_id)
        .bind(skill)
        .bind(window_secs as f64)
        .fetch_one(&self.pool)
        .await
    }
}

/// Serialize a [`TaskState`] to its SCREAMING_SNAKE wire value (e.g. `TASK_STATE_SUBMITTED`).
fn state_wire(state: &TaskState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "TASK_STATE_UNSPECIFIED".to_string())
}

/// Read a string metadata value off a task.
fn meta_str(task: &Task, key: &str) -> Option<String> {
    task.metadata
        .as_ref()?
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Read an integer metadata value off a task (accepts a number or numeric string).
fn meta_i64(task: &Task, key: &str) -> Option<i64> {
    let v = task.metadata.as_ref()?.get(key)?;
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// Set an integer metadata value on a task, creating the metadata map if absent.
fn set_meta_i64(task: &mut Task, key: &str, value: i64) {
    task.metadata
        .get_or_insert_with(Default::default)
        .insert(key.to_string(), Value::from(value));
}

/// Remove a transient metadata key (so it is never persisted into `task_json`).
fn strip_meta(task: &mut Task, key: &str) {
    if let Some(map) = task.metadata.as_mut() {
        map.remove(key);
    }
}

/// A conflict error for a failed CAS — the row was modified concurrently. Mapped from a distinct code
/// so callers can recognise it; `INVALID_REQUEST` (400) is the closest A2A wire code for "your version
/// is stale, re-read and retry".
fn conflict_error() -> A2AError {
    A2AError::new(
        error_code::INVALID_REQUEST,
        "optimistic-concurrency conflict: the task was modified concurrently; re-read and retry",
    )
}

#[async_trait]
impl TaskStore for PgTaskStore {
    /// Insert a new task at version 1. The task's metadata must carry [`LB_CALLER`] (and normally
    /// [`LB_SKILL`]/[`LB_UNDERLYING`]); those are lifted into columns for caller-scoped queries. A
    /// duplicate id is an error (the handler generates fresh server-side ids, so this never races).
    async fn create(&self, task: Task) -> Result<TaskVersion, A2AError> {
        let id = Uuid::parse_str(&task.id)
            .map_err(|_| A2AError::invalid_params("a2a taskId must be a uuid"))?;
        let caller_id = meta_str(&task, LB_CALLER)
            .ok_or_else(|| A2AError::internal("a2a store: create without lb.callerId metadata"))?;
        let skill = meta_str(&task, LB_SKILL).unwrap_or_else(|| "review".to_string());
        let underlying = meta_str(&task, LB_UNDERLYING).and_then(|s| Uuid::parse_str(&s).ok());
        let state = state_wire(&task.status.state);
        let context_id = task.context_id.clone();
        let json = serde_json::to_value(&task)
            .map_err(|e| A2AError::internal(format!("a2a store: serialize task: {e}")))?;

        let affected = sqlx::query(
            "INSERT INTO a2a_tasks \
               (a2a_task_id, context_id, caller_id, skill, underlying_task_id, state, version, task_json) \
             VALUES ($1, $2, $3, $4, $5, $6, 1, $7) \
             ON CONFLICT (a2a_task_id) DO NOTHING",
        )
        .bind(id)
        .bind(&context_id)
        .bind(&caller_id)
        .bind(&skill)
        .bind(underlying)
        .bind(&state)
        .bind(&json)
        .execute(&self.pool)
        .await
        .map_err(|e| A2AError::internal(format!("a2a store: create: {e}")))?;

        if affected.rows_affected() == 0 {
            return Err(A2AError::invalid_request(format!(
                "a2a task already exists: {}",
                task.id
            )));
        }
        Ok(1)
    }

    /// CAS update. The expected version is read from the task's [`LB_VERSION`] metadata (stamped by
    /// [`Self::get`]); the write matches `WHERE version = expected` and bumps it. Zero matched rows →
    /// a conflict (concurrent writer advanced the row) or not-found, distinguished by a follow-up
    /// existence check. Never clobbers a concurrently-updated row.
    async fn update(&self, task: Task) -> Result<TaskVersion, A2AError> {
        let id = Uuid::parse_str(&task.id)
            .map_err(|_| A2AError::invalid_params("a2a taskId must be a uuid"))?;
        let expected = meta_i64(&task, LB_VERSION).ok_or_else(|| {
            A2AError::internal("a2a store: update without an expected version (call get first)")
        })?;
        let underlying = meta_str(&task, LB_UNDERLYING).and_then(|s| Uuid::parse_str(&s).ok());
        let state = state_wire(&task.status.state);

        // Never persist the transient version key into the stored snapshot.
        let mut to_store = task;
        strip_meta(&mut to_store, LB_VERSION);
        let json = serde_json::to_value(&to_store)
            .map_err(|e| A2AError::internal(format!("a2a store: serialize task: {e}")))?;

        let new_version: Option<i64> = sqlx::query_scalar(
            "UPDATE a2a_tasks \
             SET task_json = $2, state = $3, \
                 underlying_task_id = COALESCE($4, underlying_task_id), \
                 version = version + 1, updated_at = now() \
             WHERE a2a_task_id = $1 AND version = $5 \
             RETURNING version",
        )
        .bind(id)
        .bind(&json)
        .bind(&state)
        .bind(underlying)
        .bind(expected)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| A2AError::internal(format!("a2a store: update: {e}")))?;

        match new_version {
            Some(v) => Ok(v as TaskVersion),
            None => {
                // No row matched the expected version: either the id is unknown, or a concurrent
                // writer moved it past `expected`. Distinguish so the SDK's create-on-not-found path
                // still works and a genuine conflict is reported as such.
                let exists: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM a2a_tasks WHERE a2a_task_id = $1)",
                )
                .bind(id)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| A2AError::internal(format!("a2a store: exists check: {e}")))?;
                if exists {
                    Err(conflict_error())
                } else {
                    Err(A2AError::task_not_found(&id.to_string()))
                }
            }
        }
    }

    /// Fetch a task, stamping the current [`LB_VERSION`] into its metadata for a later CAS update.
    async fn get(&self, task_id: &str) -> Result<Option<Task>, A2AError> {
        let Ok(id) = Uuid::parse_str(task_id) else {
            return Ok(None); // a non-uuid id can't be one of ours
        };
        let row: Option<(Value, i64)> =
            sqlx::query_as("SELECT task_json, version FROM a2a_tasks WHERE a2a_task_id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| A2AError::internal(format!("a2a store: get: {e}")))?;
        match row {
            None => Ok(None),
            Some((json, version)) => {
                let mut task: Task = serde_json::from_value(json)
                    .map_err(|e| A2AError::internal(format!("a2a store: deserialize task: {e}")))?;
                set_meta_i64(&mut task, LB_VERSION, version);
                Ok(Some(task))
            }
        }
    }

    /// List stored tasks (trait completeness). Filters by contextId/status when given. The handler
    /// does not expose this in Phase 1 (ListTasks is Phase 4); it exists so the store satisfies the
    /// full trait and is covered end-to-end.
    async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
        let status_wire = req.status.as_ref().map(state_wire);
        let rows: Vec<(Value,)> = sqlx::query_as(
            "SELECT task_json FROM a2a_tasks \
             WHERE ($1::text IS NULL OR context_id = $1) \
               AND ($2::text IS NULL OR state = $2) \
             ORDER BY created_at DESC",
        )
        .bind(req.context_id.as_deref())
        .bind(status_wire.as_deref())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| A2AError::internal(format!("a2a store: list: {e}")))?;
        let tasks: Vec<Task> = rows
            .into_iter()
            .filter_map(|(json,)| serde_json::from_value(json).ok())
            .collect();
        let total = tasks.len() as i32;
        Ok(ListTasksResponse {
            tasks,
            next_page_token: String::new(),
            page_size: total,
            total_size: total,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::{TaskState, TaskStatus};
    use std::collections::HashMap;

    fn task_with(id: Uuid, caller: &str, state: TaskState) -> Task {
        let mut metadata = HashMap::new();
        metadata.insert(LB_CALLER.to_string(), Value::from(caller));
        metadata.insert(LB_SKILL.to_string(), Value::from("review"));
        Task {
            id: id.to_string(),
            context_id: format!("ctx-{caller}"),
            status: TaskStatus {
                state,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: Some(metadata),
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn create_get_roundtrips_and_stamps_version(pool: PgPool) {
        let store = PgTaskStore::new(pool);
        let id = Uuid::now_v7();
        assert_eq!(
            store
                .create(task_with(id, "svc-a", TaskState::Submitted))
                .await
                .unwrap(),
            1
        );
        // Duplicate create is rejected.
        assert!(store
            .create(task_with(id, "svc-a", TaskState::Submitted))
            .await
            .is_err());

        let fetched = store.get(&id.to_string()).await.unwrap().unwrap();
        assert_eq!(fetched.status.state, TaskState::Submitted);
        // get() stamps the current version for CAS.
        assert_eq!(meta_i64(&fetched, LB_VERSION), Some(1));

        // Unknown / non-uuid ids read as None (→ TaskNotFound at the handler), never an error.
        assert!(store
            .get(&Uuid::now_v7().to_string())
            .await
            .unwrap()
            .is_none());
        assert!(store.get("not-a-uuid").await.unwrap().is_none());
    }

    /// The load-bearing test: two concurrent updates off the same read — one wins, the other is a
    /// conflict, and NO lost update occurs (the row ends at version 2 with the winner's state).
    #[sqlx::test(migrations = "./migrations")]
    async fn cas_update_lets_one_writer_win_no_lost_update(pool: PgPool) {
        let store = PgTaskStore::new(pool);
        let id = Uuid::now_v7();
        store
            .create(task_with(id, "svc-a", TaskState::Submitted))
            .await
            .unwrap();

        // Two readers each observe version 1 (simulating two replicas).
        let mut a = store.get(&id.to_string()).await.unwrap().unwrap();
        let mut b = store.get(&id.to_string()).await.unwrap().unwrap();
        assert_eq!(meta_i64(&a, LB_VERSION), Some(1));
        assert_eq!(meta_i64(&b, LB_VERSION), Some(1));

        // Writer A wins → version 2.
        a.status.state = TaskState::Working;
        assert_eq!(store.update(a).await.unwrap(), 2);

        // Writer B (still holding expected=1) must NOT clobber — it is rejected.
        b.status.state = TaskState::Failed;
        let err = store
            .update(b)
            .await
            .expect_err("stale update must be rejected");
        assert_eq!(err.code, error_code::INVALID_REQUEST);

        // The persisted state is the winner's (WORKING), at version 2 — no lost update.
        let after = store.get(&id.to_string()).await.unwrap().unwrap();
        assert_eq!(after.status.state, TaskState::Working);
        assert_eq!(meta_i64(&after, LB_VERSION), Some(2));

        // A fresh read lets B retry successfully against the new version.
        let mut b_retry = store.get(&id.to_string()).await.unwrap().unwrap();
        b_retry.status.state = TaskState::Completed;
        assert_eq!(store.update(b_retry).await.unwrap(), 3);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn update_distinguishes_not_found_from_conflict(pool: PgPool) {
        let store = PgTaskStore::new(pool);
        // Updating an id that was never created is a not-found, even with an expected version.
        let mut ghost = task_with(Uuid::now_v7(), "svc-a", TaskState::Working);
        set_meta_i64(&mut ghost, LB_VERSION, 1);
        let err = store.update(ghost).await.expect_err("unknown id");
        assert_eq!(err.code, error_code::TASK_NOT_FOUND);

        // Updating without any expected version is a programmer error (internal), not a silent write.
        let id = Uuid::now_v7();
        store
            .create(task_with(id, "svc-a", TaskState::Submitted))
            .await
            .unwrap();
        let no_version = task_with(id, "svc-a", TaskState::Working); // no LB_VERSION stamped
        assert!(store.update(no_version).await.is_err());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn load_owned_is_caller_scoped(pool: PgPool) {
        let store = PgTaskStore::new(pool);
        let id = Uuid::now_v7();
        store
            .create(task_with(id, "svc-a", TaskState::Submitted))
            .await
            .unwrap();

        // The owner sees it…
        let owned = store.load_owned(id, "svc-a").await.unwrap();
        assert_eq!(owned.unwrap().state, "TASK_STATE_SUBMITTED");
        // …another caller does not (→ TaskNotFound, no existence leak).
        assert!(store.load_owned(id, "svc-b").await.unwrap().is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn count_recent_scopes_by_caller_and_skill(pool: PgPool) {
        let store = PgTaskStore::new(pool);
        for _ in 0..3 {
            store
                .create(task_with(Uuid::now_v7(), "svc-a", TaskState::Submitted))
                .await
                .unwrap();
        }
        store
            .create(task_with(Uuid::now_v7(), "svc-b", TaskState::Submitted))
            .await
            .unwrap();
        assert_eq!(
            store.count_recent("svc-a", "review", 3600).await.unwrap(),
            3
        );
        assert_eq!(
            store.count_recent("svc-b", "review", 3600).await.unwrap(),
            1
        );
        assert_eq!(store.count_recent("svc-a", "ask", 3600).await.unwrap(), 0);
        // A zero-length window sees nothing.
        assert_eq!(store.count_recent("svc-a", "review", 0).await.unwrap(), 0);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn list_filters_by_context_and_status(pool: PgPool) {
        let store = PgTaskStore::new(pool);
        let a = Uuid::now_v7();
        store
            .create(task_with(a, "svc-a", TaskState::Submitted))
            .await
            .unwrap();
        // Move it to WORKING via CAS.
        let mut fetched = store.get(&a.to_string()).await.unwrap().unwrap();
        fetched.status.state = TaskState::Working;
        store.update(fetched).await.unwrap();
        store
            .create(task_with(Uuid::now_v7(), "svc-b", TaskState::Submitted))
            .await
            .unwrap();

        let by_ctx = store
            .list(&ListTasksRequest {
                context_id: Some("ctx-svc-a".to_string()),
                status: None,
                page_size: None,
                page_token: None,
                history_length: None,
                status_timestamp_after: None,
                include_artifacts: None,
                tenant: None,
            })
            .await
            .unwrap();
        assert_eq!(by_ctx.tasks.len(), 1);
        assert_eq!(by_ctx.tasks[0].id, a.to_string());

        let by_status = store
            .list(&ListTasksRequest {
                context_id: None,
                status: Some(TaskState::Working),
                page_size: None,
                page_token: None,
                history_length: None,
                status_timestamp_after: None,
                include_artifacts: None,
                tenant: None,
            })
            .await
            .unwrap();
        assert_eq!(by_status.tasks.len(), 1);
        assert_eq!(by_status.total_size, 1);
    }
}
