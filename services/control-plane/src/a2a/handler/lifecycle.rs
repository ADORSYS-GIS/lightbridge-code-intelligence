//! Task-lifecycle logic for [`super::A2aHandler`] — `SendMessage` (review submission, including the
//! rejection path) and the state/snapshot building shared by `GetTask` and the streaming tail. Split
//! out of the former monolithic `a2a/handler.rs` (ADR-0086 follow-up code-quality pass) — pure move,
//! no behavior change. Kept as an `impl super::A2aHandler` block in this sibling file (inherent impls
//! may span multiple files); the trait dispatch in `handler.rs` calls through to these `pub(super)`
//! methods.

use a2a::{A2AError, SendMessageRequest, SendMessageResponse, Task, TaskState};
use a2a_server::TaskStore;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::a2a::mapping::{
    ParseError, ReviewContext, ReviewInput, build_task_view, parse_review_request,
    review_artifacts, task_state_from_status,
};
use crate::a2a::store::{LB_CALLER, LB_SKILL, LB_UNDERLYING, Mapping};
use crate::db;

use super::{A2aHandler, CallerCtx, REVIEW_PERMISSION, SKILL_REVIEW, db_error};

impl A2aHandler {
    /// Persist a REJECTED task (terminal) and return it. Rejections happen at the submission gate
    /// (missing permission / quota breach / unapproved-or-unknown repo) and never create a run.
    pub(super) async fn reject(
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
    pub(super) async fn submit_review(
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

        // Build the review task — the SAME shape as the webhook `@mention` path, so it rides the
        // identical idempotency tuple / run_epoch semantics. ADR-0103: the A2A role holds no forge
        // credentials (see this module's trust-boundary doc comment), so it can't fetch repo config to
        // resolve a preset the way the webhook entry points do — it always uses the A2A entry point's
        // platform default (`deep`, reproducing today's behavior exactly).
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
            preset: crate::preset::EntryPoint::A2a.platform_default_preset().to_string(),
            entry_point: crate::preset::EntryPoint::A2a.as_str().to_string(),
            trigger_comment_id: None,
            // The A2A ingress face has no `webhook.receive` span (ticket #246 scoped tracing to the
            // webhook path); a future A2A trace root is a separate, unscoped addition.
            trace_context: None,
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
    pub(super) async fn current_state(
        &self,
        mapping: &Mapping,
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
    pub(super) async fn build_task_snapshot(
        &self,
        a2a_task_id: &str,
        mapping: &Mapping,
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

/// Parse a SCREAMING_SNAKE wire state back to a [`TaskState`] (for stored terminal snapshots).
pub(super) fn state_from_wire(wire: &str) -> TaskState {
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
