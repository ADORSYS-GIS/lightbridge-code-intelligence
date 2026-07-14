use super::lifecycle::state_from_wire;
use super::*;
use crate::a2a::store::{LB_CALLER, LB_SKILL};
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
    handler_with_limits(pool, max, HandlerLimits::default(), Some(test_key()))
}

/// A handler with NO encryption key — exercises the fail-closed path (a tokened `create` is
/// rejected and stores nothing).
fn handler_no_key(pool: PgPool, max: i64) -> A2aHandler {
    handler_with_limits(pool, max, HandlerLimits::default(), None)
}

/// A handler with explicit per-caller limits, for the cap tests.
fn handler_with_limits(
    pool: PgPool,
    max: i64,
    limits: HandlerLimits,
    key: Option<super::super::push_crypto::Key>,
) -> A2aHandler {
    A2aHandler::new(
        pool,
        QuotaConfig {
            max,
            window_secs: 3600,
        },
        limits,
        key,
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

/// A review request in the ADR-0078 `text` + `data` form: a natural-language instruction part
/// ahead of the structured target part.
fn review_req_with_text(text: &str, data: Value) -> SendMessageRequest {
    SendMessageRequest {
        message: Message::new(Role::User, vec![Part::text(text), Part::data(data)]),
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
    assert!(
        task.metadata
            .as_ref()
            .map(|m| !m.contains_key(LB_CALLER))
            .unwrap_or(true)
    );

    // A real deep-tier task row exists behind it.
    let underlying = underlying_of(&task).expect("underlying task id");
    let row = db::get_task(&pool, underlying).await.unwrap().unwrap();
    assert_eq!(row.target_id, 42);
    assert_eq!(row.command_text, "focus on auth");
    assert_eq!(row.status, "queued");
}

/// ADR-0078: a natural-language `text` instruction reaches the run's `command_text` (winning over
/// `data.prompt`), while the target still comes solely from the `data` part.
#[sqlx::test(migrations = "./migrations")]
async fn text_instruction_becomes_command_text_over_data_prompt(pool: PgPool) {
    seed_approved_repo(&pool, "acme", "api", 111).await;
    let h = handler(pool.clone(), 10);
    let task = task_of(
        h.send_message(
            &params("svc-a", &["a2a:review"]),
            review_req_with_text(
                "Focus on the auth changes.",
                json!({ "repo": "acme/api", "pr": 42, "prompt": "ignored hint", "headSha": "deadbeef" }),
            ),
        )
        .await
        .unwrap(),
    );
    assert_eq!(task.status.state, TaskState::Submitted);
    let underlying = underlying_of(&task).expect("underlying task id");
    let row = db::get_task(&pool, underlying).await.unwrap().unwrap();
    // The text wins over `data.prompt`; the target is still PR 42 from the data part.
    assert_eq!(row.command_text, "Focus on the auth changes.");
    assert_eq!(row.target_id, 42);
}

/// ADR-0078: a `text`-only message (no `data` target) is a guided `INVALID_PARAMS`, not a task —
/// the role cannot resolve a target from prose.
#[sqlx::test(migrations = "./migrations")]
async fn text_only_message_is_invalid_params_with_guidance(pool: PgPool) {
    let h = handler(pool.clone(), 10);
    let err = h
        .send_message(
            &params("svc-a", &["a2a:review"]),
            SendMessageRequest {
                message: Message::new(Role::User, vec![Part::text("review PR 128, focus on auth")]),
                configuration: None,
                metadata: None,
                tenant: None,
            },
        )
        .await
        .expect_err("text-only must be an error, not a task");
    let msg = err.to_string();
    for field in ["repo", "pr", "headSha"] {
        assert!(msg.contains(field), "guided error names `{field}`: {msg}");
    }
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
    assert!(
        h.list_tasks(
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
        .is_err()
    );
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
use futures::StreamExt;
use futures::stream::BoxStream;
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

/// Per-caller, per-task push-config cap (ADR-0079 P7): the N+1th ACTIVE config on a task is
/// rejected (`invalid_params`) and stores nothing; the cap is independent per task; and deleting a
/// config frees a slot (only active configs count).
#[sqlx::test(migrations = "./migrations")]
async fn push_config_per_caller_cap_is_enforced(pool: PgPool) {
    seed_approved_repo(&pool, "acme", "api", 111).await;
    // Cap = 2 active configs per caller per task.
    let h = handler_with_limits(
        pool.clone(),
        10,
        HandlerLimits {
            push_config_cap: 2,
            ..HandlerLimits::default()
        },
        Some(test_key()),
    );
    let caller = params("svc-a", &["a2a:review"]);
    let (task_a, _) = submit(&h, &caller, 7).await;
    let (task_b, _) = submit(&h, &caller, 8).await;

    // Two configs on task_a fill the cap.
    let c1 = h
        .create_push_config(&caller, push_config(&task_a, OK_WEBHOOK, None))
        .await
        .unwrap()
        .id
        .unwrap();
    h.create_push_config(&caller, push_config(&task_a, OK_WEBHOOK, None))
        .await
        .unwrap();

    // The 3rd is rejected and stores nothing (still exactly 2 rows on task_a).
    let over = h
        .create_push_config(&caller, push_config(&task_a, OK_WEBHOOK, None))
        .await;
    assert_eq!(
        over.unwrap_err().code,
        a2a::error_code::INVALID_PARAMS,
        "the N+1th config on a task is rejected"
    );
    assert_eq!(
        config_count(&pool, &task_a).await,
        2,
        "the over-cap create stored no row"
    );

    // The cap is per-task: task_b has its own independent budget.
    h.create_push_config(&caller, push_config(&task_b, OK_WEBHOOK, None))
        .await
        .expect("a different task has its own cap");
    assert_eq!(config_count(&pool, &task_b).await, 1);

    // Deleting a config on task_a frees a slot (only ACTIVE configs count) → a new create lands.
    h.delete_push_config(&caller, del_cfg_req(&task_a, &c1))
        .await
        .unwrap();
    h.create_push_config(&caller, push_config(&task_a, OK_WEBHOOK, None))
        .await
        .expect("a freed slot admits a new config");
    assert_eq!(config_count(&pool, &task_a).await, 2);
}

/// The per-caller stream counter: increments on acquire, rejects beyond the cap, and decrements
/// (frees a slot) when a slot guard is dropped — the RAII release the live stream relies on.
#[test]
fn stream_counter_increments_rejects_and_decrements_on_drop() {
    let counters = StreamCounters::default();
    let cap = 2;

    let s1 = counters.try_acquire("alice", cap).expect("1st slot");
    let s2 = counters.try_acquire("alice", cap).expect("2nd slot");
    assert_eq!(counters.count("alice"), 2);

    // A 3rd for the same caller is rejected — nothing changes.
    assert!(
        counters.try_acquire("alice", cap).is_none(),
        "beyond the cap is rejected"
    );
    assert_eq!(counters.count("alice"), 2);

    // A different caller has its own budget.
    let b1 = counters.try_acquire("bob", cap).expect("bob's own slot");
    assert_eq!(counters.count("bob"), 1);

    // Dropping a slot frees it (the on-close/disconnect release) → a re-acquire succeeds.
    drop(s2);
    assert_eq!(counters.count("alice"), 1);
    let _s3 = counters
        .try_acquire("alice", cap)
        .expect("a freed slot re-acquires");
    assert_eq!(counters.count("alice"), 2);

    // Dropping the last slot for a caller removes the map entry (no idle-caller accumulation).
    drop(b1);
    assert_eq!(counters.count("bob"), 0);

    drop(s1);
    drop(_s3);
    assert_eq!(counters.count("alice"), 0);
}

/// End-to-end wiring: with a per-caller stream cap of 1, a second concurrent subscription for the
/// same caller is refused with the clean "capacity" error, and dropping the first stream frees the
/// slot so a subsequent subscription succeeds (the RAII release on stream end).
#[sqlx::test(migrations = "./migrations")]
async fn per_caller_stream_cap_refuses_second_and_frees_on_drop(pool: PgPool) {
    seed_approved_repo(&pool, "acme", "api", 111).await;
    let h = handler_with_limits(
        pool.clone(),
        10,
        HandlerLimits {
            max_streams_per_caller: 1,
            ..HandlerLimits::default()
        },
        Some(test_key()),
    );
    let caller = params("svc-a", &["a2a:review"]);
    let (a2a_id, _) = submit(&h, &caller, 7).await;

    // First subscription takes the one available slot (held open, not drained).
    let s1 = h
        .subscribe_to_task(&caller, subscribe_req(&a2a_id))
        .await
        .expect("first subscription acquires the slot");

    // A second concurrent subscription for the same caller is refused (capacity), no existence leak
    // of a different kind — same message the global-saturation path uses.
    let second = h.subscribe_to_task(&caller, subscribe_req(&a2a_id)).await;
    let err = second.err().expect("second subscription is refused");
    assert_eq!(err.code, a2a::error_code::INTERNAL_ERROR);
    assert!(
        err.message.contains("capacity"),
        "the clean capacity error is returned, got: {}",
        err.message
    );

    // Dropping the first stream releases its slot (RAII), so a fresh subscription now succeeds.
    drop(s1);
    let _s3 = h
        .subscribe_to_task(&caller, subscribe_req(&a2a_id))
        .await
        .expect("a freed slot admits a new subscription");
}
