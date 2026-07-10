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
use sqlx::postgres::{PgListener, PgPoolOptions};
use sqlx::PgPool;
use uuid::Uuid;

use super::mapping::{
    build_task_view, parse_review_request, review_artifacts, task_state_from_status, ParseError,
    ReviewContext, ReviewInput,
};
use super::store::{PgTaskStore, LB_CALLER, LB_SKILL, LB_UNDERLYING};
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
/// connection or an unbounded Postgres backend. A global cap, not per-caller — per-caller fairness is
/// a later refinement (RFC-0006).
const MAX_CONCURRENT_STREAMS: u32 = 64;

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
    /// Dedicated pool for streaming `LISTEN` connections — isolated from `pool` and bounded to
    /// [`MAX_CONCURRENT_STREAMS`] so long-lived tails can neither starve the request pool nor open
    /// unbounded Postgres backends (see the const).
    listener_pool: PgPool,
    store: PgTaskStore,
    quota: QuotaConfig,
    /// The AEAD key for webhook-token encryption-at-rest (ADR-0079 §3), loaded from
    /// `A2A_PUSH_TOKEN_KEY` at role startup. `None` when unconfigured: a `create` carrying a token
    /// then **fails closed** (never storing plaintext), while a tokenless config still works.
    push_key: Option<super::push_crypto::Key>,
}

impl A2aHandler {
    pub fn new(
        pool: PgPool,
        quota: QuotaConfig,
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
        // Persisting a rejection is INTENTIONALLY best-effort: the synchronous response below already
        // carries TASK_STATE_REJECTED (the caller's authoritative outcome), so a store hiccup only
        // costs the *later* GetTask retrievability of a terminal no-op — never a lost run (a rejection
        // launches nothing). We log and swallow rather than fail the call, which would upgrade a benign
        // storage blip into a hard error on a request that already succeeded.
        if let Err(error) = self.store.create(stored.clone()).await {
            tracing::warn!(%error, a2a_task_id, "failed to persist rejected a2a task (best-effort)");
        } else if let Ok(id) = Uuid::parse_str(a2a_task_id) {
            // ADR-0077: a REJECTED submission never flows through `set_task_status` (it launches no
            // run), so append its single terminal stream event here — a stream opened on it replays one
            // event and closes. Best-effort, like the mapping snapshot above.
            if let Err(error) = crate::a2a::events::append_terminal_status(
                &self.pool,
                id,
                context_id,
                TaskState::Rejected,
                None,
            )
            .await
            {
                tracing::warn!(%error, a2a_task_id, "failed to append REJECTED stream event (best-effort)");
            }
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
        // If the mapping insert fails, the underlying run row already exists (and, having NOTIFY'd the
        // dispatcher inside `create_task`, will still execute and post its review to the PR) — nothing
        // is lost, but the caller gets an error and no pollable handle. We cannot wrap `create_task` +
        // this insert in one transaction: `create_task` fires `pg_notify` on its own pooled connection
        // and the SDK's `TaskStore::create` takes no transaction handle. So we log the orphaned run at
        // ERROR (it is reconcilable by `a2a_task_id`/`webhook_delivery_id`) and surface the error.
        if let Err(error) = self.store.create(stored.clone()).await {
            tracing::error!(
                %error, a2a_task_id, underlying = %underlying,
                "a2a mapping insert failed AFTER the underlying run was created; the run will still \
                 execute but the caller has no handle (orphaned underlying task)"
            );
            return Err(error);
        }

        tracing::info!(
            caller = %caller.id, a2a_task_id, underlying = %underlying, pr = input.pr,
            "a2a review submitted (deep tier)"
        );
        Ok(SendMessageResponse::Task(client_view(stored)))
    }

    /// Derive the current A2A state for a mapping — live from the underlying task when present,
    /// else the stored terminal snapshot (e.g. a REJECTED submission). Returns the fetched `tasks`
    /// row alongside the state so a caller that also needs the row (the completed-review path wants
    /// the SHAs/repo for the context part) reuses this single fetch instead of a second round-trip —
    /// and so state and context are read from the *same* row, with no window for a concurrent delete
    /// to slip between them.
    async fn current_state(
        &self,
        mapping: &super::store::Mapping,
    ) -> Result<(TaskState, Option<db::TaskRow>), A2AError> {
        match mapping.underlying_task_id {
            Some(underlying) => {
                let row = db::get_task(&self.pool, underlying)
                    .await
                    .map_err(db_error)?;
                let state = match &row {
                    Some(task) => task_state_from_status(&task.status),
                    // The underlying row was purged/reaped — fall back to the stored snapshot.
                    None => state_from_wire(&mapping.state),
                };
                Ok((state, row))
            }
            None => Ok((state_from_wire(&mapping.state), None)),
        }
    }

    /// Build the current A2A [`Task`] view for a mapping — the same snapshot `GetTask` returns and the
    /// initial frame of a stream. On a completed review it additionally attaches the caller-scoped
    /// artifacts (summary + findings + the review context echo).
    async fn build_task_snapshot(
        &self,
        a2a_task_id: &str,
        mapping: &super::store::Mapping,
    ) -> Result<Task, A2AError> {
        // `mapping` is already caller-scope-loaded by the caller (`get_task` / `stream_task`); this
        // helper only projects it. Reuse the single `tasks` fetch from `current_state` (returns the
        // row) so the completed-review context echo needs no second round-trip.
        let (state, task_row) = self.current_state(mapping).await?;

        // On a completed review, additionally return the caller-scoped artifacts: the summary +
        // findings, plus a context part echoing the submitted base/head SHAs, the derived scope, and
        // the posted-review permalink (so the caller can confirm what was reviewed and jump to it).
        let artifacts = if state == TaskState::Completed {
            match mapping.underlying_task_id {
                Some(underlying) => match db::get_review(&self.pool, underlying)
                    .await
                    .map_err(db_error)?
                {
                    Some(review) => {
                        // Reuse the `tasks` row already fetched for the state read (it carries the
                        // SHAs / repo / pr for the context part) — no second round-trip. `None` only
                        // if the row was concurrently deleted after the state fetch, in which case the
                        // context carries just the review_url with null SHAs (ReviewContext handles it).
                        let context =
                            review_context(task_row.as_ref(), review.review_url.as_deref());
                        Some(review_artifacts(
                            &review.summary,
                            &review.findings,
                            &context,
                        ))
                    }
                    None => None,
                },
                None => None,
            }
        } else {
            None
        };

        let metadata = mapping
            .underlying_task_id
            .map(|u| Map::from_iter([(LB_UNDERLYING.to_string(), Value::from(u.to_string()))]));
        Ok(build_task_view(
            a2a_task_id,
            &mapping.context_id,
            state,
            artifacts,
            metadata,
        ))
    }

    /// The replay-then-tail SSE stream backing `SubscribeToTask` and the streaming leg of
    /// `SendStreamingMessage` (ADR-0077). Same caller-scoped ownership check as `GetTask`
    /// (unknown/foreign id → `TaskNotFound`); then: emit the initial `Task` snapshot, **replay** the
    /// task's `a2a_task_events` in `seq` order, then **tail** — waking on the per-task `NOTIFY` channel
    /// with a bounded fallback poll, re-querying `seq > last_emitted` each wake, and closing on the
    /// `final = true` event (or the terminal-state backstop). The `seq`-cursor SELECT is the source of
    /// truth; `NOTIFY` is only a wake hint, so a missed notify costs latency, never a lost/misordered
    /// event.
    async fn stream_task(
        &self,
        caller: &CallerCtx,
        a2a_task_id: &str,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let id = Uuid::parse_str(a2a_task_id).map_err(|_| A2AError::task_not_found(a2a_task_id))?;
        let mapping = self
            .store
            .load_owned(id, &caller.id)
            .await
            .map_err(db_error)?
            .ok_or_else(|| A2AError::task_not_found(a2a_task_id))?;

        let snapshot = self.build_task_snapshot(a2a_task_id, &mapping).await?;
        let already_terminal = snapshot.status.state.is_terminal();
        let pool = self.pool.clone();
        // Streams LISTEN on the dedicated, bounded listener pool — never the request pool (see
        // MAX_CONCURRENT_STREAMS): a long tail must not hold a request connection.
        let listener_pool = self.listener_pool.clone();
        let underlying = mapping.underlying_task_id;
        let channel = crate::a2a::events::task_channel(&id);

        let stream = async_stream::stream! {
            // 1) Initial Task snapshot (the current GetTask view).
            yield Ok(StreamResponse::Task(snapshot));

            let mut last_seq: i64 = 0;

            // 2) Replay everything already logged (seq > 0), strictly in seq order.
            match crate::a2a::events::fetch_events_after(&pool, id, last_seq).await {
                Ok(events) => {
                    for (seq, payload, is_final) in events {
                        match serde_json::from_value::<StreamResponse>(payload) {
                            Ok(event) => yield Ok(event),
                            Err(error) => {
                                tracing::error!(%error, a2a_task_id = %id, seq, "a2a: corrupt stream event payload");
                                yield Err(A2AError::internal("internal error"));
                                return;
                            }
                        }
                        last_seq = seq;
                        if is_final {
                            return; // terminal event → close
                        }
                    }
                }
                Err(error) => {
                    yield Err(db_error(error));
                    return;
                }
            }

            // A task already terminal at subscribe replays its sequence and closes without tailing.
            if already_terminal {
                return;
            }

            // 3) Tail: NOTIFY-wake with a bounded fallback poll; the seq-cursor SELECT is authoritative.
            // The listener draws from the dedicated stream pool; when it is saturated (too many
            // concurrent streams on this replica) the acquire times out fast — surface that as a clear,
            // retryable "capacity" error rather than a generic internal failure.
            let mut listener = match PgListener::connect_with(&listener_pool).await {
                Ok(listener) => listener,
                Err(error) => {
                    tracing::warn!(%error, a2a_task_id = %id, "a2a: stream listener pool saturated or unavailable");
                    yield Err(A2AError::internal(
                        "streaming capacity reached; retry shortly or poll GetTask",
                    ));
                    return;
                }
            };
            if let Err(error) = listener.listen(&channel).await {
                tracing::error!(%error, a2a_task_id = %id, "a2a: stream LISTEN failed");
                yield Err(A2AError::internal("internal error"));
                return;
            }

            let start = tokio::time::Instant::now();
            loop {
                // Drain any events past the cursor (also covers events that landed between replay and
                // LISTEN — a missed notify never loses them because the cursor SELECT is the truth).
                match crate::a2a::events::fetch_events_after(&pool, id, last_seq).await {
                    Ok(events) => {
                        for (seq, payload, is_final) in events {
                            match serde_json::from_value::<StreamResponse>(payload) {
                                Ok(event) => yield Ok(event),
                                Err(error) => {
                                    tracing::error!(%error, a2a_task_id = %id, seq, "a2a: corrupt stream event payload");
                                    yield Err(A2AError::internal("internal error"));
                                    return;
                                }
                            }
                            last_seq = seq;
                            if is_final {
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        yield Err(db_error(error));
                        return;
                    }
                }

                // Backstop (ADR-0077 risk S7): if the live state is already terminal, do one FINAL
                // drain before closing. The terminal `set_task_status` transaction commits the status
                // flip and its terminal events (COMPLETED + any artifact) atomically, so a
                // live-terminal read means those events are already durable. Returning here WITHOUT
                // re-draining would drop them whenever the completion commits in the window between the
                // drain at the top of this loop and this check — the stream would close after WORKING,
                // never delivering the terminal event (an R6 violation: the stream must close *at* the
                // terminal state, delivering it). Re-fetch once; in the genuine crash case (the run
                // finished but no terminal event was ever appended) this finds nothing and we still
                // close rather than tail forever.
                if is_live_terminal(&pool, underlying).await {
                    match crate::a2a::events::fetch_events_after(&pool, id, last_seq).await {
                        Ok(events) => {
                            // Final drain: emit whatever the terminal commit left past the cursor, then
                            // close unconditionally (no need to advance `last_seq` — we never loop again).
                            for (seq, payload, is_final) in events {
                                match serde_json::from_value::<StreamResponse>(payload) {
                                    Ok(event) => yield Ok(event),
                                    Err(error) => {
                                        tracing::error!(%error, a2a_task_id = %id, seq, "a2a: corrupt stream event payload");
                                        yield Err(A2AError::internal("internal error"));
                                        return;
                                    }
                                }
                                if is_final {
                                    return;
                                }
                            }
                        }
                        Err(error) => {
                            yield Err(db_error(error));
                            return;
                        }
                    }
                    return;
                }

                // Per-connection max lifetime caps a truly stuck tail (ADR-0077 S2/S7).
                if start.elapsed() >= MAX_STREAM_LIFETIME {
                    tracing::warn!(a2a_task_id = %id, "a2a: stream hit max lifetime; closing tail");
                    return;
                }

                // Wake on NOTIFY or the fallback poll, whichever first. Errors/timeouts just re-loop and
                // re-query the cursor.
                let _ = tokio::time::timeout(TAIL_POLL_FALLBACK, listener.recv()).await;
            }
        };

        Ok(Box::pin(stream))
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

/// Build the completed-review [`ReviewContext`] from the underlying `tasks` row and the persisted
/// review permalink. The row is normally present (the state read that decided COMPLETED fetched it);
/// `None` only on a concurrent delete racing between that fetch and here, in which case the context
/// echoes just the `review_url` with null SHAs/repo — the artifact shape stays stable either way.
fn review_context(task: Option<&db::TaskRow>, review_url: Option<&str>) -> ReviewContext {
    match task {
        Some(task) => ReviewContext {
            repo: match (&task.repo_owner, &task.repo_name) {
                (Some(owner), Some(name)) => Some(format!("{owner}/{name}")),
                _ => None,
            },
            pr: Some(task.target_id),
            base_sha: task.base_sha.clone(),
            head_sha: task.head_sha.clone(),
            review_url: review_url.map(str::to_string),
        },
        None => ReviewContext {
            review_url: review_url.map(str::to_string),
            ..Default::default()
        },
    }
}

/// Map a DB error to an A2A internal error (details logged, not leaked to the caller).
fn db_error(error: sqlx::Error) -> A2AError {
    tracing::error!(%error, "a2a: database error");
    A2AError::internal("internal error")
}

/// Whether the live underlying-task state is terminal — the streaming tail's terminal-close backstop
/// (ADR-0077 S7). A reaped underlying row (or no underlying at all) reads as terminal (nothing more can
/// happen); a transient DB error reads as NOT terminal so the tail keeps polling rather than closing
/// early on a blip.
async fn is_live_terminal(pool: &PgPool, underlying: Option<Uuid>) -> bool {
    match underlying {
        Some(underlying) => match db::get_task(pool, underlying).await {
            Ok(Some(task)) => task_state_from_status(&task.status).is_terminal(),
            Ok(None) => true,
            Err(error) => {
                tracing::warn!(%error, "a2a: stream terminal backstop query failed (keep tailing)");
                false
            }
        },
        None => true,
    }
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

    /// A fixed 32-byte webhook-token encryption key for tests, so a `create` (encrypt) and a later
    /// assertion (decrypt) share the same key deterministically.
    const TEST_KEY_BYTES: [u8; 32] = [7u8; 32];

    fn test_key() -> super::super::push_crypto::Key {
        super::super::push_crypto::Key::from_bytes(&TEST_KEY_BYTES).unwrap()
    }

    /// The default test handler: a configured encryption key, so tokened push configs work.
    fn handler(pool: PgPool, max: i64) -> A2aHandler {
        A2aHandler::new(
            pool,
            QuotaConfig {
                max,
                window_secs: 3600,
            },
            Some(test_key()),
        )
    }

    /// A handler with NO encryption key — exercises the fail-closed path (a tokened `create` is
    /// rejected and stores nothing).
    fn handler_no_key(pool: PgPool, max: i64) -> A2aHandler {
        A2aHandler::new(
            pool,
            QuotaConfig {
                max,
                window_secs: 3600,
            },
            None,
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

    /// Regression (lightbridge): rejections are submission-gate no-ops that launch no run, so they
    /// must not consume the caller's deep-run quota. A caller who hits unknown repos and burns
    /// rejection rows can still submit a legitimate approved review.
    #[sqlx::test(migrations = "./migrations")]
    async fn rejections_do_not_consume_quota(pool: PgPool) {
        seed_approved_repo(&pool, "acme", "api", 111).await;
        let h = handler(pool.clone(), 1); // a single deep-run slot per window
        let ok = params("svc-a", &["a2a:review"]);

        // Two submissions to an unknown repo → both REJECTED, both persist rejection rows.
        for pr in 1..=2 {
            let rejected = task_of(
                h.send_message(&ok, review_req(json!({ "repo": "ghost/repo", "pr": pr })))
                    .await
                    .unwrap(),
            );
            assert_eq!(rejected.status.state, TaskState::Rejected, "reject {pr}");
        }

        // Despite max=1 and two prior rejection rows, a legitimate approved submission still lands —
        // the rejections did not exhaust the quota.
        let submitted = task_of(
            h.send_message(
                &ok,
                review_req(json!({ "repo": "acme/api", "pr": 42, "headSha": "abc" })),
            )
            .await
            .unwrap(),
        );
        assert_eq!(submitted.status.state, TaskState::Submitted);
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

        // succeeded + a review row → COMPLETED with summary + findings + context artifacts
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
        assert_eq!(arts[0].parts.len(), 3);
        assert_eq!(arts[0].parts[0].as_text(), Some("Found an issue"));
        match &arts[0].parts[1].content {
            a2a::PartContent::Data(v) => assert_eq!(v, &findings),
            other => panic!("expected findings data part, got {other:?}"),
        }
        // The context part echoes the effective SHAs (headSha "abc", no base → whole-tree scope),
        // the repo, and the PR — the caller can confirm exactly what was reviewed.
        match &arts[0].parts[2].content {
            a2a::PartContent::Data(v) => {
                assert_eq!(v["repo"], json!("acme/api"));
                assert_eq!(v["pr"], json!(7));
                assert_eq!(v["headSha"], json!("abc"));
                assert_eq!(v["baseSha"], json!(null));
                assert_eq!(v["scope"], json!("whole-tree"));
            }
            other => panic!("expected context data part, got {other:?}"),
        }
    }

    /// Companion to the whole-tree completion test: a run submitted WITH a base SHA echoes a
    /// diff-scoped context (`baseSha` present ⇒ `scope: "diff"`). Exercises the base-present branch
    /// of the derived scope through the full handler path, not just the pure builder. (The `scope`
    /// is derived from the *request*; see the `ReviewContext` docs for the runner-fallback caveat.)
    #[sqlx::test(migrations = "./migrations")]
    async fn completed_review_with_base_echoes_diff_scoped_context(pool: PgPool) {
        seed_approved_repo(&pool, "acme", "api", 111).await;
        let h = handler(pool.clone(), 10);
        let caller = params("svc-a", &["a2a:review"]);
        let submitted = task_of(
            h.send_message(
                &caller,
                review_req(
                    json!({ "repo": "acme/api", "pr": 9, "headSha": "head9", "baseSha": "base9" }),
                ),
            )
            .await
            .unwrap(),
        );
        let underlying = underlying_of(&submitted).unwrap();
        set_status(&pool, underlying, "succeeded").await;
        db::insert_review_if_absent(&pool, underlying, "ok", "body", 0, 0, 0, &json!([]))
            .await
            .unwrap();
        let done = h
            .get_task(
                &caller,
                GetTaskRequest {
                    id: submitted.id.clone(),
                    history_length: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();
        let arts = done.artifacts.expect("artifacts on completion");
        match &arts[0].parts[2].content {
            a2a::PartContent::Data(v) => {
                assert_eq!(v["baseSha"], json!("base9"));
                assert_eq!(v["headSha"], json!("head9"));
                assert_eq!(v["scope"], json!("diff"));
            }
            other => panic!("expected context data part, got {other:?}"),
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

    /// Regression (Gemini HIGH): when the underlying `tasks` row is deleted, the FK's `ON DELETE SET
    /// NULL` nulls `a2a_tasks.underlying_task_id`, but the serialized `task_json` still carries the dead
    /// uuid in `lb.underlyingTaskId`. A naive `get`→`update` round-trip would write that stale uuid back
    /// via `COALESCE(...)` and hit a foreign-key violation (23503 → 500). `get` now re-derives the
    /// metadata from the column, so the round-trip stays FK-safe.
    #[sqlx::test(migrations = "./migrations")]
    async fn get_update_is_fk_safe_after_underlying_task_deleted(pool: PgPool) {
        let repo_id = seed_approved_repo(&pool, "acme", "api", 111).await;
        db::record_delivery(&pool, Platform::GitHub, "wh-fk", "pull_request", &json!({}))
            .await
            .unwrap();
        let underlying = db::create_task(
            &pool,
            &db::NewTask {
                repository_id: repo_id,
                installation_id: 111,
                webhook_delivery_id: "wh-fk".to_string(),
                target_type: "pull_request".to_string(),
                target_id: 7,
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
        .expect("underlying task created");

        // Persist an A2A mapping pointing at that underlying run.
        let store = PgTaskStore::new(pool.clone());
        let a2a_id = Uuid::now_v7();
        let metadata = Map::from_iter([
            (LB_CALLER.to_string(), Value::from("svc-a")),
            (LB_SKILL.to_string(), Value::from(SKILL_REVIEW)),
            (
                LB_UNDERLYING.to_string(),
                Value::from(underlying.to_string()),
            ),
        ]);
        store
            .create(build_task_view(
                &a2a_id.to_string(),
                "ctx-a",
                TaskState::Submitted,
                None,
                Some(metadata),
            ))
            .await
            .unwrap();

        // Reap the underlying task → FK `ON DELETE SET NULL` nulls the column, but task_json is stale.
        sqlx::query("DELETE FROM tasks WHERE id = $1")
            .bind(underlying)
            .execute(&pool)
            .await
            .unwrap();

        // get() must re-sync the metadata to the (now NULL) column — the stale uuid is stripped.
        let mut got = store.get(&a2a_id.to_string()).await.unwrap().unwrap();
        assert!(
            underlying_of(&got).is_none(),
            "stale lb.underlyingTaskId must be stripped after the underlying row is deleted"
        );

        // The load-bearing assertion: the subsequent CAS update no longer FK-violates.
        got.status.state = TaskState::Working;
        store
            .update(got)
            .await
            .expect("update after underlying deletion must not hit a foreign-key violation");
    }

    async fn set_status(pool: &PgPool, task_id: Uuid, status: &str) {
        sqlx::query("UPDATE tasks SET status = $2 WHERE id = $1")
            .bind(task_id)
            .bind(status)
            .execute(pool)
            .await
            .unwrap();
    }

    // ---------------------------------------------------------------------------
    // Streaming (RFC-0006 Phase 2, ADR-0077)
    // ---------------------------------------------------------------------------

    use a2a::StreamResponse;
    use futures::stream::BoxStream;
    use futures::StreamExt;
    use std::time::Duration;

    /// A bounded-timeout stream collector: drains a `BoxStream` to completion, but if any single item
    /// (or the close) takes longer than `budget`, the test FAILS rather than hanging forever.
    async fn drain_stream(
        stream: BoxStream<'static, Result<StreamResponse, A2AError>>,
        budget: Duration,
    ) -> Vec<StreamResponse> {
        let mut stream = stream;
        let mut out = Vec::new();
        loop {
            match tokio::time::timeout(budget, stream.next()).await {
                Ok(Some(Ok(event))) => out.push(event),
                Ok(Some(Err(error))) => panic!("stream yielded an error: {error:?}"),
                Ok(None) => break, // the stream closed
                Err(_) => panic!(
                    "stream did not produce the next item / close within {budget:?} (hang guard)"
                ),
            }
        }
        out
    }

    /// The A2A state carried by a status-bearing stream event (a `Task` snapshot or a status-update).
    fn event_state(event: &StreamResponse) -> Option<TaskState> {
        match event {
            StreamResponse::Task(task) => Some(task.status.state.clone()),
            StreamResponse::StatusUpdate(update) => Some(update.status.state.clone()),
            _ => None,
        }
    }

    /// Submit an approved A2A review and return `(a2a_id, underlying_task_id)`.
    async fn submit(h: &A2aHandler, caller: &ServiceParams, pr: i64) -> (String, Uuid) {
        let task = task_of(
            h.send_message(
                caller,
                review_req(json!({ "repo": "acme/api", "pr": pr, "headSha": "abc" })),
            )
            .await
            .unwrap(),
        );
        let underlying = underlying_of(&task).expect("underlying id");
        (task.id, underlying)
    }

    async fn event_rows(pool: &PgPool, a2a_id: &str) -> Vec<(i64, String, Option<String>, bool)> {
        let id = Uuid::parse_str(a2a_id).unwrap();
        sqlx::query_as(
            "SELECT seq, kind, state, final FROM a2a_task_events WHERE a2a_task_id = $1 ORDER BY seq",
        )
        .bind(id)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    fn subscribe_req(id: &str) -> SubscribeToTaskRequest {
        SubscribeToTaskRequest {
            id: id.to_string(),
            tenant: None,
        }
    }

    /// A status transition on an A2A-fronted task appends exactly one gap-free, monotonic event; a
    /// non-A2A task appends none.
    #[sqlx::test(migrations = "./migrations")]
    async fn transitions_append_gapfree_events_and_non_a2a_appends_none(pool: PgPool) {
        seed_approved_repo(&pool, "acme", "api", 111).await;
        let h = handler(pool.clone(), 10);
        let (a2a_id, underlying) = submit(&h, &params("svc-a", &["a2a:review"]), 7).await;

        // Submission alone appends nothing — the initial state is carried by the Task snapshot.
        assert!(event_rows(&pool, &a2a_id).await.is_empty());

        // Each transition through set_task_status appends exactly one status-update.
        db::set_task_status(&pool, underlying, "running", None)
            .await
            .unwrap();
        db::set_task_status(&pool, underlying, "succeeded", None)
            .await
            .unwrap();

        let rows = event_rows(&pool, &a2a_id).await;
        let seqs: Vec<i64> = rows.iter().map(|(s, ..)| *s).collect();
        assert_eq!(seqs, vec![1, 2], "seq is gap-free and monotonic from 1");
        assert_eq!(rows[0].2.as_deref(), Some("TASK_STATE_WORKING"));
        assert!(!rows[0].3, "WORKING is not terminal");
        assert_eq!(rows[1].2.as_deref(), Some("TASK_STATE_COMPLETED"));
        assert!(rows[1].3, "COMPLETED is the terminal event (final=true)");

        // A non-A2A task (no a2a_tasks front) appends no events.
        db::record_delivery(
            &pool,
            Platform::GitHub,
            "wh-plain",
            "pull_request",
            &json!({}),
        )
        .await
        .unwrap();
        let plain = db::create_task(
            &pool,
            &db::NewTask {
                repository_id: seed_approved_repo(&pool, "acme", "api", 111).await,
                installation_id: 111,
                webhook_delivery_id: "wh-plain".to_string(),
                target_type: "pull_request".to_string(),
                target_id: 999,
                command_text: "plain".to_string(),
                base_sha: None,
                head_sha: Some("zzz".to_string()),
                run_epoch: 0,
                tier: "deep".to_string(),
                trigger_comment_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        db::set_task_status(&pool, plain, "running", None)
            .await
            .unwrap();
        db::set_task_status(&pool, plain, "succeeded", None)
            .await
            .unwrap();
        let total: i64 = sqlx::query_scalar("SELECT count(*) FROM a2a_task_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(total, 2, "only the A2A-fronted task produced events");
    }

    /// A task already terminal at subscribe replays its full sequence in order and closes on `final`,
    /// with no tailing. The silent-clean ordering (review row present before succeeded) puts the
    /// artifact-update before the terminal status-update.
    #[sqlx::test(migrations = "./migrations")]
    async fn terminal_task_replays_in_order_and_closes(pool: PgPool) {
        seed_approved_repo(&pool, "acme", "api", 111).await;
        let h = handler(pool.clone(), 10);
        let caller = params("svc-a", &["a2a:review"]);
        let (a2a_id, underlying) = submit(&h, &caller, 7).await;

        db::set_task_status(&pool, underlying, "running", None)
            .await
            .unwrap();
        // Persist the review BEFORE succeeded (the silent-clean path) so the artifact rides the stream.
        let findings = json!([{ "path": "auth.rs", "severity": "P1" }]);
        db::insert_review_if_absent(&pool, underlying, "Found it", "body", 1, 0, 0, &findings)
            .await
            .unwrap();
        db::set_task_status(&pool, underlying, "succeeded", None)
            .await
            .unwrap();

        let stream = h
            .subscribe_to_task(&caller, subscribe_req(&a2a_id))
            .await
            .unwrap();
        let events = drain_stream(stream, Duration::from_secs(5)).await;

        // Task snapshot, then WORKING, then the artifact-update, then the terminal COMPLETED — closed.
        assert!(matches!(events[0], StreamResponse::Task(_)));
        assert_eq!(event_state(&events[0]), Some(TaskState::Completed));
        assert_eq!(event_state(&events[1]), Some(TaskState::Working));
        assert!(matches!(events[2], StreamResponse::ArtifactUpdate(_)));
        assert_eq!(event_state(&events[3]), Some(TaskState::Completed));
        assert_eq!(
            events.len(),
            4,
            "stream closed right after the terminal event"
        );
        // The artifact carries the summary + findings the caller expects.
        if let StreamResponse::ArtifactUpdate(update) = &events[2] {
            assert_eq!(update.artifact.parts[0].as_text(), Some("Found it"));
        } else {
            panic!("expected an artifact-update at index 2");
        }
    }

    /// Two subscribers on the same task see identical, identically-ordered event sequences.
    #[sqlx::test(migrations = "./migrations")]
    async fn two_subscribers_see_identical_ordered_sequences(pool: PgPool) {
        seed_approved_repo(&pool, "acme", "api", 111).await;
        let h = handler(pool.clone(), 10);
        let caller = params("svc-a", &["a2a:review"]);
        let (a2a_id, underlying) = submit(&h, &caller, 7).await;
        db::set_task_status(&pool, underlying, "running", None)
            .await
            .unwrap();
        db::set_task_status(&pool, underlying, "succeeded", None)
            .await
            .unwrap();

        let s1 = h
            .subscribe_to_task(&caller, subscribe_req(&a2a_id))
            .await
            .unwrap();
        let s2 = h
            .subscribe_to_task(&caller, subscribe_req(&a2a_id))
            .await
            .unwrap();
        let e1 = drain_stream(s1, Duration::from_secs(5)).await;
        let e2 = drain_stream(s2, Duration::from_secs(5)).await;

        let states1: Vec<_> = e1.iter().filter_map(event_state).collect();
        let states2: Vec<_> = e2.iter().filter_map(event_state).collect();
        assert_eq!(
            states1, states2,
            "concurrent subscribers converge on one order"
        );
        assert_eq!(
            states1,
            vec![
                TaskState::Completed,
                TaskState::Working,
                TaskState::Completed
            ]
        );
    }

    /// Streaming and polling never disagree: an event exists for every transition a poller could
    /// observe (WORKING, then the terminal COMPLETED).
    #[sqlx::test(migrations = "./migrations")]
    async fn streaming_and_polling_agree_on_transitions(pool: PgPool) {
        seed_approved_repo(&pool, "acme", "api", 111).await;
        let h = handler(pool.clone(), 10);
        let caller = params("svc-a", &["a2a:review"]);
        let (a2a_id, underlying) = submit(&h, &caller, 7).await;

        // Poll after each transition; collect the states a poller observes.
        let mut polled = Vec::new();
        for status in ["running", "succeeded"] {
            db::set_task_status(&pool, underlying, status, None)
                .await
                .unwrap();
            let view = h
                .get_task(
                    &caller,
                    GetTaskRequest {
                        id: a2a_id.clone(),
                        history_length: None,
                        tenant: None,
                    },
                )
                .await
                .unwrap();
            polled.push(view.status.state);
        }
        assert_eq!(polled, vec![TaskState::Working, TaskState::Completed]);

        // Every polled transition has a matching status-update event in the log.
        let logged: Vec<TaskState> = event_rows(&pool, &a2a_id)
            .await
            .into_iter()
            .filter(|(_, kind, ..)| kind == "status-update")
            .map(|(_, _, state, _)| state_from_wire(&state.unwrap()))
            .collect();
        assert_eq!(
            logged, polled,
            "the event log carries exactly the transitions a poller sees"
        );
    }

    /// A foreign or unknown id subscription is `TaskNotFound` (no existence leak), same as GetTask.
    #[sqlx::test(migrations = "./migrations")]
    async fn foreign_or_unknown_subscription_is_task_not_found(pool: PgPool) {
        seed_approved_repo(&pool, "acme", "api", 111).await;
        let h = handler(pool.clone(), 10);
        let (a2a_id, _) = submit(&h, &params("svc-a", &["a2a:review"]), 7).await;

        // Another caller cannot subscribe to my task.
        let foreign = h
            .subscribe_to_task(&params("svc-b", &["a2a:review"]), subscribe_req(&a2a_id))
            .await;
        assert_eq!(
            foreign.err().map(|e| e.code),
            Some(a2a::error_code::TASK_NOT_FOUND)
        );

        // An unknown id is also TaskNotFound.
        let unknown = h
            .subscribe_to_task(
                &params("svc-a", &["a2a:review"]),
                subscribe_req(&Uuid::now_v7().to_string()),
            )
            .await;
        assert_eq!(
            unknown.err().map(|e| e.code),
            Some(a2a::error_code::TASK_NOT_FOUND)
        );
    }

    /// A REJECTED submission (via the streaming leg of SendMessage) streams its single terminal event
    /// and closes.
    #[sqlx::test(migrations = "./migrations")]
    async fn rejected_streaming_submission_closes_on_one_terminal_event(pool: PgPool) {
        seed_approved_repo(&pool, "acme", "api", 111).await;
        let h = handler(pool.clone(), 10);
        // Missing the a2a:review permission → REJECTED at the gate, but still streamable by its owner.
        let caller = params("svc-a", &["some:other"]);
        let stream = h
            .send_streaming_message(&caller, review_req(json!({ "repo": "acme/api", "pr": 1 })))
            .await
            .unwrap();
        let events = drain_stream(stream, Duration::from_secs(5)).await;

        // Snapshot (REJECTED) + the single terminal status-update, then close.
        assert_eq!(event_state(&events[0]), Some(TaskState::Rejected));
        assert!(matches!(events[1], StreamResponse::StatusUpdate(_)));
        assert_eq!(event_state(&events[1]), Some(TaskState::Rejected));
        assert_eq!(events.len(), 2, "a rejection replays one event and closes");
    }

    /// Subscribing while the task is still WORKING replays the history and then TAILS: a live
    /// transition driven after subscribe is delivered, and the stream closes on the terminal event.
    #[sqlx::test(migrations = "./migrations")]
    async fn subscribe_then_tail_delivers_a_live_transition(pool: PgPool) {
        seed_approved_repo(&pool, "acme", "api", 111).await;
        let h = handler(pool.clone(), 10);
        let caller = params("svc-a", &["a2a:review"]);
        let (a2a_id, underlying) = submit(&h, &caller, 7).await;
        // Move to running (WORKING, non-terminal) BEFORE subscribing.
        db::set_task_status(&pool, underlying, "running", None)
            .await
            .unwrap();

        let stream = h
            .subscribe_to_task(&caller, subscribe_req(&a2a_id))
            .await
            .unwrap();

        // Drive the terminal transition shortly after the subscription is tailing.
        let pool2 = pool.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            db::set_task_status(&pool2, underlying, "succeeded", None)
                .await
                .unwrap();
        });

        let events = drain_stream(stream, Duration::from_secs(10)).await;
        let states: Vec<_> = events.iter().filter_map(event_state).collect();
        // Snapshot(WORKING), replayed WORKING, then the tailed COMPLETED — closed on final.
        assert_eq!(
            states,
            vec![TaskState::Working, TaskState::Working, TaskState::Completed]
        );
    }

    // ---------------------------------------------------------------------------
    // Push notifications (RFC-0006 Phase 3, ADR-0079 §1/§3): config CRUD
    // ---------------------------------------------------------------------------

    use a2a::{
        DeleteTaskPushNotificationConfigRequest, GetTaskPushNotificationConfigRequest,
        ListTaskPushNotificationConfigsRequest, TaskPushNotificationConfig,
    };

    /// A public literal-IP webhook URL that passes the SSRF validator WITHOUT any DNS (93.184.216.34 =
    /// example.com's address, used in the ssrf.rs tests as the canonical public IP). Using a literal IP
    /// keeps these tests hermetic — no real resolver call, no CI DNS dependency.
    const OK_WEBHOOK: &str = "https://93.184.216.34/webhook";

    fn push_config(task_id: &str, url: &str, token: Option<&str>) -> TaskPushNotificationConfig {
        TaskPushNotificationConfig {
            url: url.to_string(),
            id: None,
            task_id: task_id.to_string(),
            token: token.map(str::to_string),
            authentication: None,
            tenant: None,
        }
    }

    fn get_cfg_req(task_id: &str, id: &str) -> GetTaskPushNotificationConfigRequest {
        GetTaskPushNotificationConfigRequest {
            task_id: task_id.to_string(),
            id: id.to_string(),
            tenant: None,
        }
    }

    fn list_cfg_req(task_id: &str) -> ListTaskPushNotificationConfigsRequest {
        ListTaskPushNotificationConfigsRequest {
            task_id: task_id.to_string(),
            page_size: None,
            page_token: None,
            tenant: None,
        }
    }

    fn del_cfg_req(task_id: &str, id: &str) -> DeleteTaskPushNotificationConfigRequest {
        DeleteTaskPushNotificationConfigRequest {
            task_id: task_id.to_string(),
            id: id.to_string(),
            tenant: None,
        }
    }

    /// Count the push-config rows for a task (a raw DB probe the handler layer doesn't expose).
    async fn config_count(pool: &PgPool, a2a_id: &str) -> i64 {
        let id = Uuid::parse_str(a2a_id).unwrap();
        sqlx::query_scalar("SELECT count(*) FROM a2a_push_configs WHERE a2a_task_id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// create stores a config, returns it with a server-assigned id, and the row lands with the
    /// table defaults (`state='active'`, `delivered_seq=0`) and the caller as `created_by`.
    #[sqlx::test(migrations = "./migrations")]
    async fn create_push_config_stores_and_returns_with_id(pool: PgPool) {
        seed_approved_repo(&pool, "acme", "api", 111).await;
        let h = handler(pool.clone(), 10);
        let caller = params("svc-a", &["a2a:review"]);
        let (a2a_id, _) = submit(&h, &caller, 7).await;

        let created = h
            .create_push_config(&caller, push_config(&a2a_id, OK_WEBHOOK, Some("s3cr3t")))
            .await
            .unwrap();
        let config_id = created.id.clone().expect("server-assigned config id");
        assert!(Uuid::parse_str(&config_id).is_ok(), "config id is a uuid");
        assert_eq!(created.url, OK_WEBHOOK);
        assert_eq!(created.task_id, a2a_id);

        // The stored row exists with the expected defaults + caller ownership + ENCRYPTED token.
        let row = db::get_push_config(&pool, Uuid::parse_str(&config_id).unwrap())
            .await
            .unwrap()
            .expect("row persisted");
        assert_eq!(row.a2a_task_id.to_string(), a2a_id);
        assert_eq!(row.url, OK_WEBHOOK);
        assert_eq!(row.state, "active");
        assert_eq!(row.delivered_seq, 0);
        assert_eq!(row.attempts, 0);
        assert_eq!(row.created_by, "svc-a");
        // Encryption-at-rest (ADR-0079 §3): the stored bytes are ciphertext, NOT the plaintext token,
        // yet decrypt with the role key recovers it exactly.
        let stored = row.token_enc.as_deref().expect("token stored");
        assert_ne!(
            stored,
            b"s3cr3t".as_ref(),
            "token must be stored encrypted, not plaintext"
        );
        assert!(
            !stored.windows(6).any(|w| w == b"s3cr3t"),
            "the plaintext token must not appear anywhere in the stored bytes"
        );
        assert!(
            stored.len() > "s3cr3t".len() + 12 + 16 - 1,
            "ciphertext carries the 12-byte nonce + 16-byte tag"
        );
        assert_eq!(
            super::super::push_crypto::decrypt(stored, &test_key()).as_deref(),
            Some("s3cr3t"),
            "decrypt with the role key recovers the original token"
        );
    }

    /// A `create`→`get` round-trip on a tokened config faithfully recovers the token: it is encrypted
    /// on write and decrypted on read with the same role key, so the caller sees its own secret back.
    #[sqlx::test(migrations = "./migrations")]
    async fn create_then_get_round_trips_the_token(pool: PgPool) {
        seed_approved_repo(&pool, "acme", "api", 111).await;
        let h = handler(pool.clone(), 10);
        let caller = params("svc-a", &["a2a:review"]);
        let (a2a_id, _) = submit(&h, &caller, 7).await;

        let token = "bearer-abc-123!@#";
        let cfg_id = h
            .create_push_config(&caller, push_config(&a2a_id, OK_WEBHOOK, Some(token)))
            .await
            .unwrap()
            .id
            .unwrap();

        let got = h
            .get_push_config(&caller, get_cfg_req(&a2a_id, &cfg_id))
            .await
            .unwrap();
        assert_eq!(
            got.token.as_deref(),
            Some(token),
            "get decrypts and echoes the original token"
        );
    }

    /// Fail-closed (ADR-0079 §3): with NO encryption key configured, a `create` that carries a token
    /// is rejected (`invalid_params`) and stores nothing — a token is never persisted in plaintext. A
    /// tokenless `create` still succeeds with no key.
    #[sqlx::test(migrations = "./migrations")]
    async fn create_with_token_but_no_key_fails_closed_and_stores_nothing(pool: PgPool) {
        seed_approved_repo(&pool, "acme", "api", 111).await;
        let h = handler_no_key(pool.clone(), 10);
        let caller = params("svc-a", &["a2a:review"]);
        let (a2a_id, _) = submit(&h, &caller, 7).await;

        // A tokened create with no key → rejected, nothing stored.
        let err = h
            .create_push_config(&caller, push_config(&a2a_id, OK_WEBHOOK, Some("s3cr3t")))
            .await
            .expect_err("a tokened create must fail closed when no key is configured");
        assert_eq!(err.code, a2a::error_code::INVALID_PARAMS);
        assert_eq!(
            config_count(&pool, &a2a_id).await,
            0,
            "a fail-closed create must not store a row (least of all a plaintext token)"
        );

        // A tokenless create still works fine with no key (encryption only matters for a secret).
        let created = h
            .create_push_config(&caller, push_config(&a2a_id, OK_WEBHOOK, None))
            .await
            .expect("a tokenless config needs no key");
        assert!(created.id.is_some());
        assert_eq!(created.token, None);
        assert_eq!(config_count(&pool, &a2a_id).await, 1);
    }

    /// The security-relevant path: a non-HTTPS / metadata / private URL is rejected as `invalid_params`
    /// and NOTHING is stored (the SSRF validator runs before any DB write).
    #[sqlx::test(migrations = "./migrations")]
    async fn create_push_config_rejects_ssrf_urls_and_stores_nothing(pool: PgPool) {
        seed_approved_repo(&pool, "acme", "api", 111).await;
        let h = handler(pool.clone(), 10);
        let caller = params("svc-a", &["a2a:review"]);
        let (a2a_id, _) = submit(&h, &caller, 7).await;

        for bad in [
            "http://93.184.216.34/webhook",        // plaintext scheme
            "https://169.254.169.254/latest/meta", // cloud metadata IP
            "https://10.0.0.1/webhook",            // RFC 1918 private
        ] {
            let err = h
                .create_push_config(&caller, push_config(&a2a_id, bad, None))
                .await
                .expect_err("SSRF URL must be rejected");
            assert_eq!(
                err.code,
                a2a::error_code::INVALID_PARAMS,
                "{bad} should be invalid_params"
            );
        }
        assert_eq!(
            config_count(&pool, &a2a_id).await,
            0,
            "a rejected webhook URL must store no config row"
        );
    }

    /// Multiple configs per task coexist; list returns them all; get fetches one; delete removes it and
    /// a subsequent get is TaskNotFound.
    #[sqlx::test(migrations = "./migrations")]
    async fn multiple_configs_list_get_and_delete(pool: PgPool) {
        seed_approved_repo(&pool, "acme", "api", 111).await;
        let h = handler(pool.clone(), 10);
        let caller = params("svc-a", &["a2a:review"]);
        let (a2a_id, _) = submit(&h, &caller, 7).await;

        let c1 = h
            .create_push_config(&caller, push_config(&a2a_id, OK_WEBHOOK, Some("t1")))
            .await
            .unwrap()
            .id
            .unwrap();
        let c2 = h
            .create_push_config(
                &caller,
                push_config(&a2a_id, "https://93.184.216.34/second", None),
            )
            .await
            .unwrap()
            .id
            .unwrap();
        assert_ne!(c1, c2, "each config gets its own id");

        // list returns both.
        let listed = h
            .list_push_configs(&caller, list_cfg_req(&a2a_id))
            .await
            .unwrap();
        let mut ids: Vec<String> = listed
            .configs
            .iter()
            .map(|c| c.id.clone().unwrap())
            .collect();
        ids.sort();
        let mut want = vec![c1.clone(), c2.clone()];
        want.sort();
        assert_eq!(ids, want);
        assert!(listed.next_page_token.is_none());

        // get returns c1 with its stored token echoed back.
        let got = h
            .get_push_config(&caller, get_cfg_req(&a2a_id, &c1))
            .await
            .unwrap();
        assert_eq!(got.id.as_deref(), Some(c1.as_str()));
        assert_eq!(got.url, OK_WEBHOOK);
        assert_eq!(got.token.as_deref(), Some("t1"));

        // delete c1, then a get on it is TaskNotFound and list has only c2 left.
        h.delete_push_config(&caller, del_cfg_req(&a2a_id, &c1))
            .await
            .unwrap();
        let after = h.get_push_config(&caller, get_cfg_req(&a2a_id, &c1)).await;
        assert_eq!(
            after.unwrap_err().code,
            a2a::error_code::TASK_NOT_FOUND,
            "a deleted config reads as TaskNotFound"
        );
        let remaining = h
            .list_push_configs(&caller, list_cfg_req(&a2a_id))
            .await
            .unwrap();
        assert_eq!(remaining.configs.len(), 1);
        assert_eq!(remaining.configs[0].id.as_deref(), Some(c2.as_str()));
    }

    /// Caller-scoping (ADR-0079 P9): caller B cannot create/get/list/delete on caller A's task — every
    /// method returns TaskNotFound (no existence leak), exactly like GetTask.
    #[sqlx::test(migrations = "./migrations")]
    async fn push_config_crud_is_caller_scoped(pool: PgPool) {
        seed_approved_repo(&pool, "acme", "api", 111).await;
        let h = handler(pool.clone(), 10);
        let alice = params("svc-a", &["a2a:review"]);
        let bob = params("svc-b", &["a2a:review"]);
        let (a2a_id, _) = submit(&h, &alice, 7).await;

        // Alice registers a config; Bob cannot see or touch it.
        let cfg_id = h
            .create_push_config(&alice, push_config(&a2a_id, OK_WEBHOOK, None))
            .await
            .unwrap()
            .id
            .unwrap();

        // Bob create on Alice's task → TaskNotFound (and stores nothing).
        let bob_create = h
            .create_push_config(&bob, push_config(&a2a_id, OK_WEBHOOK, None))
            .await;
        assert_eq!(
            bob_create.unwrap_err().code,
            a2a::error_code::TASK_NOT_FOUND
        );
        assert_eq!(
            config_count(&pool, &a2a_id).await,
            1,
            "Bob's foreign create must not add a row"
        );

        // Bob get / list / delete on Alice's task → all TaskNotFound.
        assert_eq!(
            h.get_push_config(&bob, get_cfg_req(&a2a_id, &cfg_id))
                .await
                .unwrap_err()
                .code,
            a2a::error_code::TASK_NOT_FOUND
        );
        assert_eq!(
            h.list_push_configs(&bob, list_cfg_req(&a2a_id))
                .await
                .unwrap_err()
                .code,
            a2a::error_code::TASK_NOT_FOUND
        );
        assert_eq!(
            h.delete_push_config(&bob, del_cfg_req(&a2a_id, &cfg_id))
                .await
                .unwrap_err()
                .code,
            a2a::error_code::TASK_NOT_FOUND
        );
        // Alice's config survived Bob's delete attempt.
        assert_eq!(config_count(&pool, &a2a_id).await, 1);
    }

    /// An unknown task id, and a config id that belongs to a *different* task, both read as
    /// TaskNotFound — the config must belong to the proven-owned task.
    #[sqlx::test(migrations = "./migrations")]
    async fn push_config_unknown_task_and_cross_task_config_are_not_found(pool: PgPool) {
        seed_approved_repo(&pool, "acme", "api", 111).await;
        let h = handler(pool.clone(), 10);
        let caller = params("svc-a", &["a2a:review"]);
        let (task_a, _) = submit(&h, &caller, 7).await;
        let (task_b, _) = submit(&h, &caller, 8).await;

        // Unknown task id → TaskNotFound on create.
        let unknown = Uuid::now_v7().to_string();
        assert_eq!(
            h.create_push_config(&caller, push_config(&unknown, OK_WEBHOOK, None))
                .await
                .unwrap_err()
                .code,
            a2a::error_code::TASK_NOT_FOUND
        );

        // A config registered on task_b is not reachable via task_a (same caller, different task).
        let cfg_on_b = h
            .create_push_config(&caller, push_config(&task_b, OK_WEBHOOK, None))
            .await
            .unwrap()
            .id
            .unwrap();
        assert_eq!(
            h.get_push_config(&caller, get_cfg_req(&task_a, &cfg_on_b))
                .await
                .unwrap_err()
                .code,
            a2a::error_code::TASK_NOT_FOUND,
            "a config id from another task must not resolve under task_a"
        );
        assert_eq!(
            h.delete_push_config(&caller, del_cfg_req(&task_a, &cfg_on_b))
                .await
                .unwrap_err()
                .code,
            a2a::error_code::TASK_NOT_FOUND
        );
        // The cross-task delete attempt did not remove the real config on task_b.
        assert_eq!(config_count(&pool, &task_b).await, 1);
    }
}
