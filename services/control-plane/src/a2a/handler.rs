//! The A2A [`RequestHandler`] for the `review` skill (RFC-0006 Phase 1).
//!
//! This is the thin glue between the SDK's transport bindings and our task plumbing. It implements
//! only the polling surface — `SendMessage` / `GetTask` / `CancelTask`; streaming, push, list, and
//! the extended card are later phases and return "unsupported". Every method is authenticated and
//! authorized; the heavy translation logic lives in [`super::mapping`] (pure, unit-tested) and the
//! optimistic-concurrency persistence in [`super::store`].
//!
//! ## Trust boundary
//!
//! The handler NEVER launches a Job or touches a forge — it creates a task row via the *same*
//! path as the webhook handler ([`crate::db::create_task`], deep tier, `run_epoch = 0`) and returns
//! a handle. Egress stays on the reconciler/Restate path; this role holds no forge credentials.

use std::collections::HashSet;

use a2a::{
    A2AError, AgentCard, CancelTaskRequest, DeleteTaskPushNotificationConfigRequest,
    GetExtendedAgentCardRequest, GetTaskPushNotificationConfigRequest, GetTaskRequest,
    ListTaskPushNotificationConfigsRequest, ListTaskPushNotificationConfigsResponse,
    ListTasksRequest, ListTasksResponse, SendMessageRequest, SendMessageResponse, StreamResponse,
    SubscribeToTaskRequest, Task, TaskPushNotificationConfig, TaskState,
};
use a2a_server::handler::RequestHandler;
use a2a_server::middleware::ServiceParams;
use a2a_server::TaskStore;
use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::mapping::{
    build_task_view, parse_review_request, review_artifacts, task_state_from_status, ParseError,
    ReviewInput,
};
use super::store::{PgTaskStore, LB_CALLER, LB_SKILL, LB_UNDERLYING};
use super::{HDR_CALLER, HDR_PERMS};
use crate::db;

/// The permission a caller must hold to invoke the `review` skill (ADR-0023).
const REVIEW_PERMISSION: &str = "a2a:review";
/// The A2A skill id this handler serves.
const SKILL_REVIEW: &str = "review";

/// Per-identity rate limit on deep-run submission (RFC-0006 R4).
#[derive(Debug, Clone, Copy)]
pub struct QuotaConfig {
    /// Max submissions per identity within the window (breach → `TASK_STATE_REJECTED`).
    pub max: i64,
    /// Rolling window in seconds.
    pub window_secs: i64,
}

/// The authenticated caller, as injected by the auth middleware (never trusted from the raw request).
struct CallerCtx {
    id: String,
    perms: HashSet<String>,
}

/// The A2A review request handler.
pub struct A2aHandler {
    pool: PgPool,
    store: PgTaskStore,
    quota: QuotaConfig,
}

impl A2aHandler {
    pub fn new(pool: PgPool, quota: QuotaConfig) -> Self {
        let store = PgTaskStore::new(pool.clone());
        Self { pool, store, quota }
    }

    /// Extract the caller identity from the middleware-injected params. The middleware validated the
    /// OIDC token and set these; a request without them never reached here in production (the layer
    /// 401s first), so their absence is an internal error.
    fn caller(params: &ServiceParams) -> Result<CallerCtx, A2AError> {
        let id = params
            .get(HDR_CALLER)
            .and_then(|v| v.first())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| A2AError::internal("a2a: missing authenticated caller"))?
            .to_string();
        let perms = params
            .get(HDR_PERMS)
            .and_then(|v| v.first())
            .map(|joined| {
                joined
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Ok(CallerCtx { id, perms })
    }

    /// Persist a REJECTED task (terminal) and return it. Rejections happen at the submission gate
    /// (missing permission / quota breach / unapproved-or-unknown repo) and never create a run.
    async fn reject(
        &self,
        caller: &CallerCtx,
        a2a_task_id: &str,
        context_id: &str,
        reason: &str,
    ) -> Result<SendMessageResponse, A2AError> {
        tracing::info!(caller = %caller.id, a2a_task_id, reason, "a2a review submission REJECTED");
        let mut stored = build_task_view(
            a2a_task_id,
            context_id,
            TaskState::Rejected,
            None,
            Some(rejection_metadata(caller, reason)),
        );
        // Carry the linkage the store lifts into columns (no underlying task for a rejection).
        stored
            .metadata
            .get_or_insert_with(Default::default)
            .insert(LB_SKILL.to_string(), Value::from(SKILL_REVIEW));
        // Best-effort persist so a later GetTask can return the rejection; the response is returned
        // regardless of a persistence hiccup.
        if let Err(error) = self.store.create(stored.clone()).await {
            tracing::warn!(%error, a2a_task_id, "failed to persist rejected a2a task");
        }
        Ok(SendMessageResponse::Task(client_view(stored)))
    }

    /// The `review` skill: authenticate → authorize → quota → repo-approval → create a deep task via
    /// the same path as the webhook handler → return a SUBMITTED handle.
    async fn submit_review(
        &self,
        caller: &CallerCtx,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        // Single-tenant deployment: a request carrying a tenant is unsupported (RFC-0006).
        if req.tenant.is_some() {
            return Err(A2AError::unsupported_operation(
                "multi-tenant A2A requests are not supported",
            ));
        }

        let a2a_task_id = Uuid::now_v7().to_string();
        let context_id = req
            .message
            .context_id
            .clone()
            .unwrap_or_else(a2a::new_context_id);

        // Parse the structured review request. A malformed body is a client error (400); an
        // unsupported skill is a precondition failure — neither is a REJECTED task.
        let input: ReviewInput = match parse_review_request(&req.message.parts) {
            Ok(input) => input,
            Err(ParseError::UnsupportedSkill(skill)) => {
                return Err(A2AError::unsupported_operation(format!(
                    "unsupported skill {skill:?}; only `review` is available in this phase"
                )));
            }
            Err(err) => return Err(A2AError::invalid_params(err.to_string())),
        };

        // Authorization: missing the per-skill permission is a submission REJECTION (RFC-0006 state
        // table), not a transport error — the caller authenticated fine, just isn't allowed.
        if !caller.perms.contains(REVIEW_PERMISSION) {
            return self
                .reject(
                    caller,
                    &a2a_task_id,
                    &context_id,
                    "caller lacks the a2a:review permission",
                )
                .await;
        }

        // Per-identity quota (RFC-0006 R4): breach → REJECTED, so a noisy client cannot queue
        // expensive deep runs without bound.
        let recent = self
            .store
            .count_recent(&caller.id, SKILL_REVIEW, self.quota.window_secs)
            .await
            .map_err(db_error)?;
        if recent >= self.quota.max {
            return self
                .reject(
                    caller,
                    &a2a_task_id,
                    &context_id,
                    "per-identity deep-run quota exceeded",
                )
                .await;
        }

        // Repo-approval gate (ADR-0063), enforced at submission — A2A is never a side door around it.
        let repo = db::find_repository(&self.pool, input.platform, &input.owner, &input.name)
            .await
            .map_err(db_error)?;
        let Some(repo) = repo else {
            return self
                .reject(
                    caller,
                    &a2a_task_id,
                    &context_id,
                    "repository is not connected to Lightbridge",
                )
                .await;
        };
        if repo.status != "approved" {
            return self
                .reject(
                    caller,
                    &a2a_task_id,
                    &context_id,
                    "repository is not approved (awaiting admin approval)",
                )
                .await;
        }
        let Some(installation_id) = repo.installation_id else {
            return self
                .reject(
                    caller,
                    &a2a_task_id,
                    &context_id,
                    "repository is connected but not fully provisioned (no installation)",
                )
                .await;
        };

        // Require a caller-supplied head SHA. The `a2a` role holds NO forge credentials, so it cannot
        // resolve a PR head itself; a null head would fall through downstream to a review of the
        // repo's DEFAULT branch (agent-runner clones the default branch and returns no PR diff), which
        // then gets posted onto the PR — a silently-wrong review. Reject instead of guessing. Rejecting
        // here (before `create_task`) creates zero task rows, exactly like the approval gate above.
        if input.head_sha.is_none() {
            return self
                .reject(
                    caller,
                    &a2a_task_id,
                    &context_id,
                    "A2A review requires an explicit headSha; this server cannot resolve a PR head without forge credentials",
                )
                .await;
        }

        // Build the deep-tier review task — the SAME shape as the webhook `@mention` path, so it rides
        // the identical idempotency tuple / run_epoch semantics.
        let new_task = db::NewTask {
            repository_id: repo.id,
            installation_id,
            webhook_delivery_id: format!("a2a:{a2a_task_id}"),
            target_type: "pull_request".to_string(),
            target_id: input.pr,
            command_text: input.prompt.clone(),
            base_sha: input.base_sha.clone(),
            head_sha: input.head_sha.clone(),
            run_epoch: 0,
            tier: "deep".to_string(),
            trigger_comment_id: None,
        };

        // Record a synthetic delivery so the task's `webhook_delivery_id` FK is satisfied and the
        // A2A trigger is auditable — then create the task idempotently.
        let provenance = json!({
            "source": "a2a",
            "caller": caller.id,
            "skill": SKILL_REVIEW,
            "repo": format!("{}/{}", input.owner, input.name),
            "pr": input.pr,
            "a2a_task_id": a2a_task_id,
        });
        db::record_delivery(
            &self.pool,
            input.platform,
            &new_task.webhook_delivery_id,
            "a2a.review",
            &provenance,
        )
        .await
        .map_err(db_error)?;

        // Content-idempotent create: dedups against a webhook-triggered review of the same head
        // (RFC-0006 R5). On dedup, map this A2A task onto the existing underlying run.
        let underlying = match db::create_task(&self.pool, &new_task)
            .await
            .map_err(db_error)?
        {
            Some(id) => id,
            None => db::find_task_id_by_idempotency(&self.pool, &new_task)
                .await
                .map_err(db_error)?
                .ok_or_else(|| {
                    A2AError::internal("a2a: create dedup'd but no existing task was found")
                })?,
        };

        // Persist the mapping (SUBMITTED) with the linkage the store lifts into columns.
        let mut stored = build_task_view(
            &a2a_task_id,
            &context_id,
            TaskState::Submitted,
            None,
            Some(submitted_metadata(caller, &underlying)),
        );
        let meta = stored.metadata.get_or_insert_with(Default::default);
        meta.insert(LB_SKILL.to_string(), Value::from(SKILL_REVIEW));
        meta.insert(
            LB_UNDERLYING.to_string(),
            Value::from(underlying.to_string()),
        );
        self.store.create(stored.clone()).await?;

        tracing::info!(
            caller = %caller.id, a2a_task_id, underlying = %underlying, pr = input.pr,
            "a2a review submitted (deep tier)"
        );
        Ok(SendMessageResponse::Task(client_view(stored)))
    }

    /// Derive the current A2A state for a mapping — live from the underlying task when present,
    /// else the stored terminal snapshot (e.g. a REJECTED submission).
    async fn current_state(&self, mapping: &super::store::Mapping) -> Result<TaskState, A2AError> {
        match mapping.underlying_task_id {
            Some(underlying) => {
                let row = db::get_task(&self.pool, underlying)
                    .await
                    .map_err(db_error)?;
                Ok(match row {
                    Some(task) => task_state_from_status(&task.status),
                    // The underlying row was purged/reaped — fall back to the stored snapshot.
                    None => state_from_wire(&mapping.state),
                })
            }
            None => Ok(state_from_wire(&mapping.state)),
        }
    }
}

#[async_trait]
impl RequestHandler for A2aHandler {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        let caller = Self::caller(params)?;
        self.submit_review(&caller, req).await
    }

    async fn get_task(
        &self,
        params: &ServiceParams,
        req: GetTaskRequest,
    ) -> Result<Task, A2AError> {
        let caller = Self::caller(params)?;
        let id = Uuid::parse_str(&req.id).map_err(|_| A2AError::task_not_found(&req.id))?;
        // Caller-scoped load — an unknown-or-not-owned id is a clean TaskNotFound (no existence leak).
        let mapping = self
            .store
            .load_owned(id, &caller.id)
            .await
            .map_err(db_error)?
            .ok_or_else(|| A2AError::task_not_found(&req.id))?;

        let state = self.current_state(&mapping).await?;

        // On a completed review, additionally return the caller-scoped artifacts (summary + findings).
        let artifacts = if state == TaskState::Completed {
            match mapping.underlying_task_id {
                Some(underlying) => db::get_review(&self.pool, underlying)
                    .await
                    .map_err(db_error)?
                    .map(|review| review_artifacts(&review.summary, &review.findings)),
                None => None,
            }
        } else {
            None
        };

        let metadata = mapping
            .underlying_task_id
            .map(|u| Map::from_iter([(LB_UNDERLYING.to_string(), Value::from(u.to_string()))]));
        Ok(build_task_view(
            &req.id,
            &mapping.context_id,
            state,
            artifacts,
            metadata,
        ))
    }

    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        let caller = Self::caller(params)?;
        let id = Uuid::parse_str(&req.id).map_err(|_| A2AError::task_not_found(&req.id))?;
        let mapping = self
            .store
            .load_owned(id, &caller.id)
            .await
            .map_err(db_error)?
            .ok_or_else(|| A2AError::task_not_found(&req.id))?;

        // Already terminal → not cancelable (spec).
        let state = self.current_state(&mapping).await?;
        if state.is_terminal() {
            return Err(A2AError::task_not_cancelable(&req.id));
        }

        // Flip the underlying task to cancelled — the runner's self-cancel poll / the reaper stop the
        // Job. (No underlying row means there is nothing running to cancel.)
        if let Some(underlying) = mapping.underlying_task_id {
            db::cancel_task_by_id(&self.pool, underlying)
                .await
                .map_err(db_error)?;
        }

        // Persist CANCELED via CAS. A concurrent writer advancing the row makes this a benign no-op —
        // the underlying cancel flag is already the source of truth — so a conflict is not fatal.
        if let Ok(Some(mut snapshot)) = self.store.get(&req.id).await {
            snapshot.status.state = TaskState::Canceled;
            if let Err(error) = self.store.update(snapshot).await {
                tracing::debug!(%error, a2a_task_id = %req.id, "cancel snapshot CAS lost a race (benign)");
            }
        }

        let metadata = mapping
            .underlying_task_id
            .map(|u| Map::from_iter([(LB_UNDERLYING.to_string(), Value::from(u.to_string()))]));
        Ok(build_task_view(
            &req.id,
            &mapping.context_id,
            TaskState::Canceled,
            None,
            metadata,
        ))
    }

    // --- Later-phase surface: explicitly unsupported in Phase 1 (polling only) ---

    async fn send_streaming_message(
        &self,
        _params: &ServiceParams,
        _req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        Err(A2AError::unsupported_operation(
            "streaming is not supported in this phase (polling only)",
        ))
    }

    async fn subscribe_to_task(
        &self,
        _params: &ServiceParams,
        _req: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        Err(A2AError::unsupported_operation(
            "task subscription is not supported in this phase (polling only)",
        ))
    }

    async fn list_tasks(
        &self,
        _params: &ServiceParams,
        _req: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        Err(A2AError::unsupported_operation(
            "ListTasks is not supported in this phase",
        ))
    }

    async fn create_push_config(
        &self,
        _params: &ServiceParams,
        _req: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        Err(A2AError::push_notification_not_supported())
    }

    async fn get_push_config(
        &self,
        _params: &ServiceParams,
        _req: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        Err(A2AError::push_notification_not_supported())
    }

    async fn list_push_configs(
        &self,
        _params: &ServiceParams,
        _req: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        Err(A2AError::push_notification_not_supported())
    }

    async fn delete_push_config(
        &self,
        _params: &ServiceParams,
        _req: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        Err(A2AError::push_notification_not_supported())
    }

    async fn get_extended_agent_card(
        &self,
        _params: &ServiceParams,
        _req: GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        Err(A2AError::unsupported_operation(
            "extended agent card is not configured",
        ))
    }
}

/// Map a DB error to an A2A internal error (details logged, not leaked to the caller).
fn db_error(error: sqlx::Error) -> A2AError {
    tracing::error!(%error, "a2a: database error");
    A2AError::internal("internal error")
}

/// Parse a SCREAMING_SNAKE wire state back to a [`TaskState`] (for stored terminal snapshots).
fn state_from_wire(wire: &str) -> TaskState {
    serde_json::from_value(Value::String(wire.to_string())).unwrap_or(TaskState::Unspecified)
}

/// Metadata for a persisted rejection: the caller (lifted into a column) and a human reason.
fn rejection_metadata(caller: &CallerCtx, reason: &str) -> Map<String, Value> {
    Map::from_iter([
        (LB_CALLER.to_string(), Value::from(caller.id.clone())),
        ("lb.rejectionReason".to_string(), Value::from(reason)),
    ])
}

/// Metadata for a submitted task: the caller (lifted into a column) and the underlying task id.
fn submitted_metadata(caller: &CallerCtx, underlying: &Uuid) -> Map<String, Value> {
    Map::from_iter([
        (LB_CALLER.to_string(), Value::from(caller.id.clone())),
        (
            LB_UNDERLYING.to_string(),
            Value::from(underlying.to_string()),
        ),
    ])
}

/// Strip internal `lb.*` linkage metadata (caller id, skill) from a task before it goes back to the
/// client. `lb.underlyingTaskId` is kept — it is the caller's own correlation handle.
fn client_view(mut task: Task) -> Task {
    if let Some(map) = task.metadata.as_mut() {
        map.remove(LB_CALLER);
        map.remove(LB_SKILL);
        if map.is_empty() {
            task.metadata = None;
        }
    }
    task
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a2a::{HDR_CALLER, HDR_PERMS};
    use crate::integrations::platform::Platform;
    use a2a::{Message, Part, Role};
    use serde_json::json;

    fn handler(pool: PgPool, max: i64) -> A2aHandler {
        A2aHandler::new(
            pool,
            QuotaConfig {
                max,
                window_secs: 3600,
            },
        )
    }

    /// Middleware-injected params for `caller` holding `perms`.
    fn params(caller: &str, perms: &[&str]) -> ServiceParams {
        let mut p = ServiceParams::new();
        p.insert(HDR_CALLER.to_string(), vec![caller.to_string()]);
        p.insert(HDR_PERMS.to_string(), vec![perms.join(",")]);
        p
    }

    fn review_req(data: Value) -> SendMessageRequest {
        SendMessageRequest {
            message: Message::new(Role::User, vec![Part::data(data)]),
            configuration: None,
            metadata: None,
            tenant: None,
        }
    }

    async fn seed_approved_repo(pool: &PgPool, owner: &str, name: &str, installation: i64) -> i64 {
        let id = db::upsert_repository(
            pool,
            Platform::GitHub,
            // A unique platform_repo_id per (owner,name) keeps parallel tests isolated.
            (owner.len() * 1000 + name.len()) as i64,
            owner,
            name,
            "main",
            Some(installation),
        )
        .await
        .unwrap();
        sqlx::query("UPDATE repositories SET status = 'approved' WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        id
    }

    fn task_of(resp: SendMessageResponse) -> Task {
        match resp {
            SendMessageResponse::Task(task) => task,
            other => panic!("expected a Task response, got {other:?}"),
        }
    }

    fn underlying_of(task: &Task) -> Option<Uuid> {
        task.metadata
            .as_ref()?
            .get(LB_UNDERLYING)
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn submit_creates_a_deep_task_and_returns_submitted(pool: PgPool) {
        seed_approved_repo(&pool, "acme", "api", 111).await;
        let h = handler(pool.clone(), 10);
        let resp = h
            .send_message(
                &params("svc-a", &["a2a:review"]),
                review_req(
                    json!({ "repo": "acme/api", "pr": 42, "prompt": "focus on auth", "headSha": "deadbeef" }),
                ),
            )
            .await
            .unwrap();
        let task = task_of(resp);
        assert_eq!(task.status.state, TaskState::Submitted);
        // The client view never leaks the caller id.
        assert!(task
            .metadata
            .as_ref()
            .map(|m| !m.contains_key(LB_CALLER))
            .unwrap_or(true));

        // A real deep-tier task row exists behind it.
        let underlying = underlying_of(&task).expect("underlying task id");
        let row = db::get_task(&pool, underlying).await.unwrap().unwrap();
        assert_eq!(row.target_id, 42);
        assert_eq!(row.command_text, "focus on auth");
        assert_eq!(row.status, "queued");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn missing_permission_is_rejected(pool: PgPool) {
        seed_approved_repo(&pool, "acme", "api", 111).await;
        let h = handler(pool.clone(), 10);
        let task = task_of(
            h.send_message(
                &params("svc-a", &["some:other"]),
                review_req(json!({ "repo": "acme/api", "pr": 1 })),
            )
            .await
            .unwrap(),
        );
        assert_eq!(task.status.state, TaskState::Rejected);
        // A rejection creates no underlying run.
        assert!(underlying_of(&task).is_none());
        // …and GetTask on it returns the terminal REJECTED (persisted, caller-scoped).
        let got = h
            .get_task(
                &params("svc-a", &["a2a:review"]),
                GetTaskRequest {
                    id: task.id.clone(),
                    history_length: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(got.status.state, TaskState::Rejected);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn quota_breach_is_rejected(pool: PgPool) {
        seed_approved_repo(&pool, "acme", "api", 111).await;
        let h = handler(pool.clone(), 2); // max 2 per window
        let ok = params("svc-a", &["a2a:review"]);
        for pr in 1..=2 {
            let t = task_of(
                h.send_message(
                    &ok,
                    review_req(json!({ "repo": "acme/api", "pr": pr, "headSha": "abc" })),
                )
                .await
                .unwrap(),
            );
            assert_eq!(t.status.state, TaskState::Submitted, "submission {pr}");
        }
        // The third breaches the per-identity quota.
        let third = task_of(
            h.send_message(
                &ok,
                review_req(json!({ "repo": "acme/api", "pr": 3, "headSha": "abc" })),
            )
            .await
            .unwrap(),
        );
        assert_eq!(third.status.state, TaskState::Rejected);
        // A different caller is unaffected (quota is per-identity).
        let other = task_of(
            h.send_message(
                &params("svc-b", &["a2a:review"]),
                review_req(json!({ "repo": "acme/api", "pr": 4, "headSha": "abc" })),
            )
            .await
            .unwrap(),
        );
        assert_eq!(other.status.state, TaskState::Submitted);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn unapproved_and_unknown_repos_are_rejected(pool: PgPool) {
        // A connected-but-pending repo.
        let id = db::upsert_repository(
            &pool,
            Platform::GitHub,
            9999,
            "acme",
            "pending",
            "main",
            Some(1),
        )
        .await
        .unwrap();
        assert_eq!(
            db::repository_status(&pool, id).await.unwrap().as_deref(),
            Some("pending")
        );
        let h = handler(pool.clone(), 10);
        let ok = params("svc-a", &["a2a:review"]);

        let pending = task_of(
            h.send_message(&ok, review_req(json!({ "repo": "acme/pending", "pr": 1 })))
                .await
                .unwrap(),
        );
        assert_eq!(pending.status.state, TaskState::Rejected);

        // A repo that was never connected at all.
        let unknown = task_of(
            h.send_message(&ok, review_req(json!({ "repo": "ghost/repo", "pr": 1 })))
                .await
                .unwrap(),
        );
        assert_eq!(unknown.status.state, TaskState::Rejected);
        // The approval gate held: no task rows were created.
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            count, 0,
            "A2A must not create a run behind the approval gate"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn missing_head_sha_is_rejected_and_creates_no_run(pool: PgPool) {
        // Approved repo, authorized caller, valid PR — the ONLY thing missing is a head SHA.
        seed_approved_repo(&pool, "acme", "api", 111).await;
        let h = handler(pool.clone(), 10);
        let task = task_of(
            h.send_message(
                &params("svc-a", &["a2a:review"]),
                // No `headSha`: the `a2a` role cannot resolve a PR head (no forge credentials), so a
                // null-head review would silently review the default branch — reject instead.
                review_req(json!({ "repo": "acme/api", "pr": 42, "prompt": "focus on auth" })),
            )
            .await
            .unwrap(),
        );
        assert_eq!(task.status.state, TaskState::Rejected);
        // A rejection creates no underlying run…
        assert!(underlying_of(&task).is_none());
        // …and, mirroring the approval-gate zero-rows guarantee, no `tasks` row exists at all.
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            count, 0,
            "a null-head A2A review must not create a run (it would review the default branch)"
        );

        // A base SHA alone does not satisfy the requirement — a head is still required.
        let base_only = task_of(
            h.send_message(
                &params("svc-a", &["a2a:review"]),
                review_req(json!({ "repo": "acme/api", "pr": 42, "baseSha": "def456" })),
            )
            .await
            .unwrap(),
        );
        assert_eq!(base_only.status.state, TaskState::Rejected);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn approved_repo_without_installation_is_rejected(pool: PgPool) {
        // Approved but not fully provisioned: no installation id (handler.rs approval gate branch).
        let id = db::upsert_repository(
            &pool,
            Platform::GitHub,
            4242,
            "acme",
            "noinstall",
            "main",
            None, // no installation
        )
        .await
        .unwrap();
        sqlx::query("UPDATE repositories SET status = 'approved' WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();

        let h = handler(pool.clone(), 10);
        let task = task_of(
            h.send_message(
                &params("svc-a", &["a2a:review"]),
                review_req(json!({ "repo": "acme/noinstall", "pr": 1, "headSha": "abc" })),
            )
            .await
            .unwrap(),
        );
        assert_eq!(task.status.state, TaskState::Rejected);
        assert!(underlying_of(&task).is_none());
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "an unprovisioned repo must not create a run");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn dedups_against_a_webhook_triggered_review(pool: PgPool) {
        let repo_id = seed_approved_repo(&pool, "acme", "api", 111).await;
        // A webhook-triggered deep review of PR 42 @ head "abc" already exists (same idempotency
        // tuple the A2A path will produce for the default prompt).
        db::record_delivery(&pool, Platform::GitHub, "wh-1", "pull_request", &json!({}))
            .await
            .unwrap();
        let webhook_task = db::create_task(
            &pool,
            &db::NewTask {
                repository_id: repo_id,
                installation_id: 111,
                webhook_delivery_id: "wh-1".to_string(),
                target_type: "pull_request".to_string(),
                target_id: 42,
                command_text: "Deep review requested via A2A.".to_string(),
                base_sha: None,
                head_sha: Some("abc".to_string()),
                run_epoch: 0,
                tier: "deep".to_string(),
                trigger_comment_id: None,
            },
        )
        .await
        .unwrap()
        .expect("webhook task created");

        // The A2A submission of the same head dedups onto the existing run (no fork of idempotency).
        let h = handler(pool.clone(), 10);
        let task = task_of(
            h.send_message(
                &params("svc-a", &["a2a:review"]),
                review_req(json!({ "repo": "acme/api", "pr": 42, "headSha": "abc" })),
            )
            .await
            .unwrap(),
        );
        assert_eq!(task.status.state, TaskState::Submitted);
        assert_eq!(
            underlying_of(&task),
            Some(webhook_task),
            "A2A review must map onto the webhook-triggered run, not create a second one"
        );
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks WHERE target_id = 42")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "exactly one underlying run for the PR head");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn get_task_maps_live_state_and_returns_artifacts_on_completion(pool: PgPool) {
        seed_approved_repo(&pool, "acme", "api", 111).await;
        let h = handler(pool.clone(), 10);
        let caller = params("svc-a", &["a2a:review"]);
        let submitted = task_of(
            h.send_message(
                &caller,
                review_req(json!({ "repo": "acme/api", "pr": 7, "headSha": "abc" })),
            )
            .await
            .unwrap(),
        );
        let a2a_id = submitted.id.clone();
        let underlying = underlying_of(&submitted).unwrap();

        let get = |id: String, c: ServiceParams| {
            let h = &h;
            async move {
                h.get_task(
                    &c,
                    GetTaskRequest {
                        id,
                        history_length: None,
                        tenant: None,
                    },
                )
                .await
                .unwrap()
            }
        };

        // queued → SUBMITTED
        assert_eq!(
            get(a2a_id.clone(), caller.clone()).await.status.state,
            TaskState::Submitted
        );

        // running → WORKING
        set_status(&pool, underlying, "running").await;
        assert_eq!(
            get(a2a_id.clone(), caller.clone()).await.status.state,
            TaskState::Working
        );

        // succeeded + a review row → COMPLETED with summary + findings artifacts
        set_status(&pool, underlying, "succeeded").await;
        let findings = json!([{ "path": "auth.rs", "severity": "P1" }]);
        db::insert_review_if_absent(
            &pool,
            underlying,
            "Found an issue",
            "body",
            1,
            0,
            0,
            &findings,
        )
        .await
        .unwrap();
        let done = get(a2a_id.clone(), caller.clone()).await;
        assert_eq!(done.status.state, TaskState::Completed);
        let arts = done.artifacts.expect("artifacts on completion");
        assert_eq!(arts.len(), 1);
        assert_eq!(arts[0].parts[0].as_text(), Some("Found an issue"));
        match &arts[0].parts[1].content {
            a2a::PartContent::Data(v) => assert_eq!(v, &findings),
            other => panic!("expected findings data part, got {other:?}"),
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn get_task_unknown_or_foreign_id_is_task_not_found(pool: PgPool) {
        seed_approved_repo(&pool, "acme", "api", 111).await;
        let h = handler(pool.clone(), 10);
        let mine = task_of(
            h.send_message(
                &params("svc-a", &["a2a:review"]),
                review_req(json!({ "repo": "acme/api", "pr": 1, "headSha": "abc" })),
            )
            .await
            .unwrap(),
        );

        // A client-supplied unknown id → TaskNotFound.
        let unknown = h
            .get_task(
                &params("svc-a", &["a2a:review"]),
                GetTaskRequest {
                    id: Uuid::now_v7().to_string(),
                    history_length: None,
                    tenant: None,
                },
            )
            .await;
        assert_eq!(unknown.unwrap_err().code, a2a::error_code::TASK_NOT_FOUND);

        // Another caller cannot read my task (no existence leak → TaskNotFound, not Forbidden).
        let foreign = h
            .get_task(
                &params("svc-b", &["a2a:review"]),
                GetTaskRequest {
                    id: mine.id.clone(),
                    history_length: None,
                    tenant: None,
                },
            )
            .await;
        assert_eq!(foreign.unwrap_err().code, a2a::error_code::TASK_NOT_FOUND);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn cancel_sets_underlying_and_rejects_terminal(pool: PgPool) {
        seed_approved_repo(&pool, "acme", "api", 111).await;
        let h = handler(pool.clone(), 10);
        let caller = params("svc-a", &["a2a:review"]);
        let submitted = task_of(
            h.send_message(
                &caller,
                review_req(json!({ "repo": "acme/api", "pr": 5, "headSha": "abc" })),
            )
            .await
            .unwrap(),
        );
        let underlying = underlying_of(&submitted).unwrap();

        let canceled = h
            .cancel_task(
                &caller,
                CancelTaskRequest {
                    id: submitted.id.clone(),
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(canceled.status.state, TaskState::Canceled);
        // The underlying task was actually cancelled (the runner's self-cancel poll then stops the Job).
        assert_eq!(
            db::get_task(&pool, underlying)
                .await
                .unwrap()
                .unwrap()
                .status,
            "cancelled"
        );

        // Cancelling an already-terminal task is not cancelable.
        let again = h
            .cancel_task(
                &caller,
                CancelTaskRequest {
                    id: submitted.id.clone(),
                    metadata: None,
                    tenant: None,
                },
            )
            .await;
        assert_eq!(
            again.unwrap_err().code,
            a2a::error_code::TASK_NOT_CANCELABLE
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn tenant_and_unsupported_surface_are_refused(pool: PgPool) {
        let h = handler(pool.clone(), 10);
        let caller = params("svc-a", &["a2a:review"]);

        // A tenant-carrying request is unsupported.
        let mut req = review_req(json!({ "repo": "acme/api", "pr": 1 }));
        req.tenant = Some("t1".to_string());
        assert_eq!(
            h.send_message(&caller, req).await.unwrap_err().code,
            a2a::error_code::UNSUPPORTED_OPERATION
        );

        // A non-review skill is unsupported (not a REJECTED task — it's a malformed call).
        assert_eq!(
            h.send_message(
                &caller,
                review_req(json!({ "skill": "ask", "repo": "acme/api", "pr": 1 }))
            )
            .await
            .unwrap_err()
            .code,
            a2a::error_code::UNSUPPORTED_OPERATION
        );

        // A malformed body → INVALID_PARAMS.
        assert_eq!(
            h.send_message(&caller, review_req(json!({ "pr": 1 })))
                .await
                .unwrap_err()
                .code,
            a2a::error_code::INVALID_PARAMS
        );

        // Phase-1 unsupported surface: ListTasks + push config.
        assert!(h
            .list_tasks(
                &caller,
                ListTasksRequest {
                    context_id: None,
                    status: None,
                    page_size: None,
                    page_token: None,
                    history_length: None,
                    status_timestamp_after: None,
                    include_artifacts: None,
                    tenant: None,
                }
            )
            .await
            .is_err());
    }

    async fn set_status(pool: &PgPool, task_id: Uuid, status: &str) {
        sqlx::query("UPDATE tasks SET status = $2 WHERE id = $1")
            .bind(task_id)
            .bind(status)
            .execute(pool)
            .await
            .unwrap();
    }
}
