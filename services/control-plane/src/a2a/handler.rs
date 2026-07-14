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

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use a2a::{
    A2AError, AgentCard, CancelTaskRequest, DeleteTaskPushNotificationConfigRequest,
    GetExtendedAgentCardRequest, GetTaskPushNotificationConfigRequest, GetTaskRequest,
    ListTaskPushNotificationConfigsRequest, ListTaskPushNotificationConfigsResponse,
    ListTasksRequest, ListTasksResponse, SendMessageRequest, SendMessageResponse, StreamResponse,
    SubscribeToTaskRequest, Task, TaskPushNotificationConfig, TaskState,
};
use a2a_server::TaskStore;
use a2a_server::handler::RequestHandler;
use a2a_server::middleware::ServiceParams;
use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::{Map, Value};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use super::mapping::build_task_view;
use super::store::{LB_UNDERLYING, PgTaskStore};
use super::{HDR_CALLER, HDR_PERMS};
use crate::db;

/// The permission a caller must hold to invoke the `review` skill (ADR-0023).
const REVIEW_PERMISSION: &str = "a2a:review";
/// The A2A skill id this handler serves.
const SKILL_REVIEW: &str = "review";

/// Fallback poll cadence for a streaming tail when no `NOTIFY` arrives (mirrors the dispatcher's
/// `LISTEN`-with-timeout loop). A missed notify costs at most this much latency, never correctness.
const TAIL_POLL_FALLBACK: std::time::Duration = std::time::Duration::from_secs(5);
/// Hard cap on a single streaming connection's lifetime — a backstop for a tail that never sees its
/// terminal event (ADR-0077 S2/S7). Comfortably longer than a 2 h deep run (ADR-0062) so it never cuts
/// a legitimate long tail short.
const MAX_STREAM_LIFETIME: std::time::Duration = std::time::Duration::from_secs(3 * 60 * 60);
/// Streaming subscriptions get their OWN small connection pool, separate from the request pool that
/// serves `SendMessage`/`GetTask`/`CancelTask`. A [`PgListener`] holds its connection for the whole
/// stream lifetime (up to [`MAX_STREAM_LIFETIME`] = 3 h), so serving tails from the request pool would
/// let a handful of long-lived streams starve every other query on the replica. This size therefore
/// doubles as the **per-replica cap on concurrent streams**: past it a new subscription fails fast
/// (a short acquire timeout) with a clear "capacity reached" error rather than pinning a request
/// connection or an unbounded Postgres backend. This is the GLOBAL cap; a per-caller cap
/// ([`HandlerLimits::max_streams_per_caller`]) sits in front of it so one caller cannot consume the
/// whole global pool.
const MAX_CONCURRENT_STREAMS: u32 = 64;

/// Default per-caller concurrent-subscription cap (RFC-0006): one caller may hold at most this many
/// live streams on a replica, so it can never monopolize the global [`MAX_CONCURRENT_STREAMS`] pool.
/// From `A2A_MAX_STREAMS_PER_CALLER`.
const DEFAULT_MAX_STREAMS_PER_CALLER: u32 = 8;

/// Default per-caller, per-task cap on active push-notification configs (ADR-0079 P7): a caller may
/// register at most this many webhooks on any one task, bounding webhook fan-out abuse. From
/// `A2A_MAX_PUSH_CONFIGS_PER_TASK`.
const DEFAULT_PUSH_CONFIG_CAP: i64 = 10;

/// Per-identity rate limit on deep-run submission (RFC-0006 R4).
#[derive(Debug, Clone, Copy)]
pub struct QuotaConfig {
    /// Max submissions per identity within the window (breach → `TASK_STATE_REJECTED`).
    pub max: i64,
    /// Rolling window in seconds.
    pub window_secs: i64,
}

/// Per-caller abuse bounds for push + streaming (ADR-0079 P7 / RFC-0006 streaming fairness), resolved
/// from env at role startup. Separate from [`QuotaConfig`] (deep-run submission quota) — these bound
/// *resource* fan-out (webhooks per task, concurrent streams per caller), not run submission.
#[derive(Debug, Clone, Copy)]
pub struct HandlerLimits {
    /// Max **active** push configs one caller may hold on a single task.
    pub push_config_cap: i64,
    /// Max concurrent streaming subscriptions one caller may hold on this replica.
    pub max_streams_per_caller: u32,
}

impl Default for HandlerLimits {
    fn default() -> Self {
        Self {
            push_config_cap: DEFAULT_PUSH_CONFIG_CAP,
            max_streams_per_caller: DEFAULT_MAX_STREAMS_PER_CALLER,
        }
    }
}

impl HandlerLimits {
    /// Resolve from env: `A2A_MAX_PUSH_CONFIGS_PER_TASK` and `A2A_MAX_STREAMS_PER_CALLER`. A
    /// non-positive / unparseable value falls back to the default, and both are floored to 1 so the
    /// role can never be configured into a zero-cap (every request rejected) or unbounded state.
    pub fn from_env() -> Self {
        let push_config_cap = std::env::var("A2A_MAX_PUSH_CONFIGS_PER_TASK")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(DEFAULT_PUSH_CONFIG_CAP)
            .max(1);
        let max_streams_per_caller = std::env::var("A2A_MAX_STREAMS_PER_CALLER")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(DEFAULT_MAX_STREAMS_PER_CALLER)
            .max(1);
        Self {
            push_config_cap,
            max_streams_per_caller,
        }
    }
}

/// In-memory per-caller live-stream counter (RFC-0006 streaming fairness). A subscription acquires a
/// slot at subscribe and releases it when the stream ends — via a [`StreamSlot`] RAII guard, so the
/// decrement happens on disconnect / clean close / error alike (the guard is dropped when the SSE
/// generator is dropped, whichever way it ends). Per-replica: the cap bounds one caller's concurrent
/// streams on THIS pod, which is exactly what protects the pod's [`MAX_CONCURRENT_STREAMS`] pool.
#[derive(Clone, Default)]
struct StreamCounters {
    inner: Arc<Mutex<HashMap<String, u32>>>,
}

impl StreamCounters {
    /// Try to take a slot for `caller_id` under `cap`. `Some(guard)` when the caller is below the cap
    /// (count incremented; released on guard drop); `None` when already at the cap (nothing changes).
    fn try_acquire(&self, caller_id: &str, cap: u32) -> Option<StreamSlot> {
        // A poisoned lock only means a prior holder panicked mid-update; the counter is still coherent
        // enough to keep bounding, so recover the guard rather than propagating the panic.
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let count = map.entry(caller_id.to_string()).or_insert(0);
        if *count >= cap {
            // Don't leave a stray 0-entry we just inserted for a capped caller (only possible when the
            // caller had an entry already, since a fresh entry is 0 < cap≥1 — but be explicit).
            if *count == 0 {
                map.remove(caller_id);
            }
            return None;
        }
        *count += 1;
        Some(StreamSlot {
            counters: self.inner.clone(),
            caller_id: caller_id.to_string(),
        })
    }

    #[cfg(test)]
    fn count(&self, caller_id: &str) -> u32 {
        let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.get(caller_id).copied().unwrap_or(0)
    }
}

/// RAII slot for one live stream: decrements its caller's counter on drop (disconnect / close / error),
/// removing the map entry when it hits zero so the map never accumulates idle callers.
struct StreamSlot {
    counters: Arc<Mutex<HashMap<String, u32>>>,
    caller_id: String,
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        let mut map = self.counters.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = map.get_mut(&self.caller_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                map.remove(&self.caller_id);
            }
        }
    }
}

/// The authenticated caller, as injected by the auth middleware (never trusted from the raw request).
struct CallerCtx {
    id: String,
    perms: HashSet<String>,
}

/// The A2A review request handler.
pub struct A2aHandler {
    pool: PgPool,
    /// Dedicated pool for streaming `LISTEN` connections — isolated from `pool` and bounded to
    /// [`MAX_CONCURRENT_STREAMS`] so long-lived tails can neither starve the request pool nor open
    /// unbounded Postgres backends (see the const).
    listener_pool: PgPool,
    store: PgTaskStore,
    quota: QuotaConfig,
    /// Per-caller push + streaming abuse bounds (ADR-0079 P7 / RFC-0006 streaming fairness).
    limits: HandlerLimits,
    /// Per-caller live-stream counter enforcing [`HandlerLimits::max_streams_per_caller`].
    stream_counters: StreamCounters,
    /// The AEAD key for webhook-token encryption-at-rest (ADR-0079 §3), loaded from
    /// `A2A_PUSH_TOKEN_KEY` at role startup. `None` when unconfigured: a `create` carrying a token
    /// then **fails closed** (never storing plaintext), while a tokenless config still works.
    push_key: Option<super::push_crypto::Key>,
}

impl A2aHandler {
    pub fn new(
        pool: PgPool,
        quota: QuotaConfig,
        limits: HandlerLimits,
        push_key: Option<super::push_crypto::Key>,
    ) -> Self {
        let store = PgTaskStore::new(pool.clone());
        // Derived from the request pool's own connect options (so it targets the same database,
        // including in tests) but with its own small cap and a short acquire timeout, so a saturated
        // stream pool surfaces as a fast, explicit error instead of blocking. `connect_lazy_with`
        // opens nothing until the first subscription.
        let listener_pool = PgPoolOptions::new()
            .max_connections(MAX_CONCURRENT_STREAMS)
            .acquire_timeout(std::time::Duration::from_millis(250))
            .connect_lazy_with((*pool.connect_options()).clone());
        Self {
            pool,
            listener_pool,
            store,
            quota,
            limits,
            stream_counters: StreamCounters::default(),
            push_key,
        }
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
}

mod lifecycle;
mod streaming;

#[cfg(test)]
mod tests;

/// Map a DB error to an A2A internal error (details logged, not leaked to the caller).
fn db_error(error: sqlx::Error) -> A2AError {
    tracing::error!(%error, "a2a: database error");
    A2AError::internal("internal error")
}

/// Project a stored push-config row into the A2A [`TaskPushNotificationConfig`] wire type (ADR-0079
/// §1). Echoes the caller's own token back — it is the caller's secret and the read is caller-scoped —
/// so a `create`→`get` round-trip is faithful. The token is stored **encrypted at rest** (§3), so it
/// is decrypted here with the role's `push_key`. A decrypt failure — a wrong/rotated key, or a token
/// stored while a key was configured but now absent — echoes `token: None` and logs at `warn`; it is
/// never a 500 (the rest of the config is still useful) and the token bytes/key are never logged.
/// `authentication` is not persisted separately in this slice (only the caller `token`; see the
/// migration), so it comes back `None`.
fn push_config_view(
    row: db::PushConfigRow,
    push_key: Option<&super::push_crypto::Key>,
) -> TaskPushNotificationConfig {
    let token = row.token_enc.as_deref().and_then(|bytes| match push_key {
        Some(key) => {
            let decoded = crate::a2a::push_crypto::decrypt(bytes, key);
            if decoded.is_none() {
                tracing::warn!(
                    config_id = %row.config_id,
                    "a2a: stored webhook token failed to decrypt (wrong/rotated key?); echoing token=None"
                );
            }
            decoded
        }
        None => {
            tracing::warn!(
                config_id = %row.config_id,
                "a2a: stored webhook token present but no encryption key is configured; echoing token=None"
            );
            None
        }
    });
    TaskPushNotificationConfig {
        url: row.url,
        id: Some(row.config_id.to_string()),
        task_id: row.a2a_task_id.to_string(),
        token,
        authentication: None,
        tenant: None,
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

        self.build_task_snapshot(&req.id, &mapping).await
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

        // Already terminal → not cancelable (spec). (The fetched row is unused on the cancel path.)
        let (state, _) = self.current_state(&mapping).await?;
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

        // ADR-0077: the underlying flip above bypasses `set_task_status`, so append the terminal CANCELED
        // stream event here (locking the run row so it serializes against a concurrent status append; the
        // `has_final` guard then makes whichever runs second a no-op). Best-effort, like the CAS snapshot.
        if let Err(error) = crate::a2a::events::append_terminal_status(
            &self.pool,
            id,
            &mapping.context_id,
            TaskState::Canceled,
            mapping.underlying_task_id,
        )
        .await
        {
            tracing::warn!(%error, a2a_task_id = %req.id, "failed to append CANCELED stream event (best-effort)");
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

    // --- Streaming (RFC-0006 Phase 2, ADR-0077): replay-then-tail the append-only event log ---

    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        // The streaming leg of SendMessage submits via the Phase-1 path first, then streams the task it
        // created (a REJECTED submission streams its single terminal event and closes).
        let caller = Self::caller(params)?;
        let SendMessageResponse::Task(task) = self.submit_review(&caller, req).await? else {
            return Err(A2AError::internal(
                "a2a: streaming submit did not yield a task handle",
            ));
        };
        self.stream_task(&caller, &task.id).await
    }

    async fn subscribe_to_task(
        &self,
        params: &ServiceParams,
        req: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let caller = Self::caller(params)?;
        self.stream_task(&caller, &req.id).await
    }

    // --- Later-phase surface: explicitly unsupported ---

    async fn list_tasks(
        &self,
        _params: &ServiceParams,
        _req: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        Err(A2AError::unsupported_operation(
            "ListTasks is not supported in this phase",
        ))
    }

    // --- Push notifications (RFC-0006 Phase 3, ADR-0079 §1/§3): config CRUD ---
    //
    // Every method is CALLER-SCOPED exactly like `GetTask`: it resolves the target `a2a_task_id` and
    // `load_owned(task_id, caller)`s it, so an unknown/foreign task — or a config whose task the caller
    // does not own — is a clean `TaskNotFound` (no existence leak, ADR-0079 P9). `create` additionally
    // validates the webhook URL synchronously (ADR-0079 §2) BEFORE any DB write, so a private/invalid
    // URL is rejected and stores nothing. Delivery/notifier code and the card flip are slice 2b.

    async fn create_push_config(
        &self,
        params: &ServiceParams,
        req: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let caller = Self::caller(params)?;
        // Single-tenant deployment (RFC-0006), mirroring `submit_review`.
        if req.tenant.is_some() {
            return Err(A2AError::unsupported_operation(
                "multi-tenant A2A requests are not supported",
            ));
        }
        // Caller-scoped ownership check on the parent task (same as GetTask) — before anything else.
        let task_id =
            Uuid::parse_str(&req.task_id).map_err(|_| A2AError::task_not_found(&req.task_id))?;
        self.store
            .load_owned(task_id, &caller.id)
            .await
            .map_err(db_error)?
            .ok_or_else(|| A2AError::task_not_found(&req.task_id))?;

        // Per-caller, per-task cap (ADR-0079 P7): a caller may hold at most `push_config_cap` ACTIVE
        // webhooks on one task, bounding webhook fan-out abuse. Checked (on the caller's own configs,
        // via `created_by`) BEFORE the SSRF resolution + DB write, so an over-cap create is a cheap
        // fast reject that resolves no DNS and stores nothing. Counting only `active` configs lets a
        // caller replace ones that were dead-lettered by repeated delivery failure. Other callers'
        // configs on the same task never count against this caller (the cap is per-caller).
        let active = db::count_active_push_configs_for_caller(&self.pool, task_id, &caller.id)
            .await
            .map_err(db_error)?;
        if active >= self.limits.push_config_cap {
            tracing::info!(
                caller = %caller.id, a2a_task_id = %task_id, active,
                cap = self.limits.push_config_cap,
                "a2a push config create rejected: per-caller per-task cap reached"
            );
            return Err(A2AError::invalid_params(format!(
                "push-notification config limit reached for this task ({} of {} used); \
                 delete an existing config before registering another",
                active, self.limits.push_config_cap
            )));
        }

        // SSRF validation at REGISTRATION (ADR-0079 §2), synchronously and BEFORE any DB write: a
        // non-HTTPS / private / metadata / cluster-CIDR / invalid URL is rejected as `invalid_params`
        // and nothing is stored. The validator re-runs at every delivery attempt (slice 2b) for the
        // DNS-rebinding/TOCTOU defence. `SsrfPolicy::default()` enforces the fixed blocked ranges; the
        // operator extra-CIDR list (cluster Service/Pod CIDRs) is config-wired in the deploy slice.
        // TODO(ADR-0079 §2, slice 3): thread the operator-configured `SsrfPolicy` here instead of default.
        let policy = crate::a2a::ssrf::SsrfPolicy::default();
        crate::a2a::ssrf::validate_webhook_url(
            &req.url,
            &policy,
            crate::a2a::ssrf::system_resolver,
        )
        .map_err(|error| A2AError::invalid_params(error.to_string()))?;

        // Encrypt the caller's auth token at rest (ADR-0079 §3): ChaCha20-Poly1305 AEAD, a fresh
        // per-config nonce prepended to the ciphertext (see `push_crypto`). **Fail closed:** if a
        // caller supplies a token but no encryption key is configured, reject rather than store it in
        // plaintext — nothing is written (this runs before the DB insert). A config with *no* token
        // needs no key and stores `NULL`, unchanged. The token bytes never touch a log line.
        let token_enc = match req.token.as_deref() {
            Some(token) => match self.push_key.as_ref() {
                Some(key) => Some(crate::a2a::push_crypto::encrypt(token.as_bytes(), key)),
                None => {
                    return Err(A2AError::invalid_params(
                        "push token encryption is not configured; this server cannot accept a \
                         webhook auth token",
                    ));
                }
            },
            None => None,
        };
        // Multiple configs per task are allowed (spec) — mint a fresh server-side id every time.
        let config_id = Uuid::now_v7();
        db::insert_push_config(
            &self.pool,
            config_id,
            task_id,
            &req.url,
            token_enc.as_deref(),
            &caller.id,
        )
        .await
        .map_err(db_error)?;

        tracing::info!(
            caller = %caller.id, a2a_task_id = %task_id, config_id = %config_id,
            "a2a push config registered"
        );
        // Echo the stored config back with its server-assigned id and the normalized task id.
        Ok(TaskPushNotificationConfig {
            id: Some(config_id.to_string()),
            task_id: task_id.to_string(),
            ..req
        })
    }

    async fn get_push_config(
        &self,
        params: &ServiceParams,
        req: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let caller = Self::caller(params)?;
        if req.tenant.is_some() {
            return Err(A2AError::unsupported_operation(
                "multi-tenant A2A requests are not supported",
            ));
        }
        let task_id =
            Uuid::parse_str(&req.task_id).map_err(|_| A2AError::task_not_found(&req.task_id))?;
        self.store
            .load_owned(task_id, &caller.id)
            .await
            .map_err(db_error)?
            .ok_or_else(|| A2AError::task_not_found(&req.task_id))?;

        let config_id = Uuid::parse_str(&req.id).map_err(|_| A2AError::task_not_found(&req.id))?;
        // The config must belong to the task the caller just proved it owns — a config on any other
        // task (even the caller's own) reads as TaskNotFound, no existence leak.
        let row = db::get_push_config(&self.pool, config_id)
            .await
            .map_err(db_error)?
            .filter(|row| row.a2a_task_id == task_id)
            .ok_or_else(|| A2AError::task_not_found(&req.id))?;
        Ok(push_config_view(row, self.push_key.as_ref()))
    }

    async fn list_push_configs(
        &self,
        params: &ServiceParams,
        req: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        let caller = Self::caller(params)?;
        if req.tenant.is_some() {
            return Err(A2AError::unsupported_operation(
                "multi-tenant A2A requests are not supported",
            ));
        }
        let task_id =
            Uuid::parse_str(&req.task_id).map_err(|_| A2AError::task_not_found(&req.task_id))?;
        self.store
            .load_owned(task_id, &caller.id)
            .await
            .map_err(db_error)?
            .ok_or_else(|| A2AError::task_not_found(&req.task_id))?;

        let configs = db::list_push_configs_for_task(&self.pool, task_id)
            .await
            .map_err(db_error)?
            .into_iter()
            .map(|row| push_config_view(row, self.push_key.as_ref()))
            .collect();
        // No pagination in this slice: all of a task's configs fit in one page (per-task config caps
        // are the abuse bound — ADR-0079 P7). A `next_page_token` is a later refinement.
        Ok(ListTaskPushNotificationConfigsResponse {
            configs,
            next_page_token: None,
        })
    }

    async fn delete_push_config(
        &self,
        params: &ServiceParams,
        req: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        let caller = Self::caller(params)?;
        if req.tenant.is_some() {
            return Err(A2AError::unsupported_operation(
                "multi-tenant A2A requests are not supported",
            ));
        }
        let task_id =
            Uuid::parse_str(&req.task_id).map_err(|_| A2AError::task_not_found(&req.task_id))?;
        self.store
            .load_owned(task_id, &caller.id)
            .await
            .map_err(db_error)?
            .ok_or_else(|| A2AError::task_not_found(&req.task_id))?;

        let config_id = Uuid::parse_str(&req.id).map_err(|_| A2AError::task_not_found(&req.id))?;
        // Delete scoped to the proven-owned task in a single query: a config on another task (or a
        // guessed id) matches zero rows → TaskNotFound. No pre-SELECT is needed — this is the last
        // operation, so there is no later INSERT that could FK-violate on a missing row.
        let deleted = db::delete_push_config(&self.pool, config_id, task_id)
            .await
            .map_err(db_error)?;
        if !deleted {
            return Err(A2AError::task_not_found(&req.id));
        }
        tracing::info!(
            caller = %caller.id, a2a_task_id = %task_id, config_id = %config_id,
            "a2a push config deleted"
        );
        Ok(())
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
