use super::*;
use serde_json::json;
// The REAL journaled step-result types (ADR-0087) — used by the durable_step round-trip so the
// test proves `from_value::<T>` rehydration through jsonb, not a hand-authored Value.
use lci_agent_types::{AssistantTurn, FunctionCallReq, ToolCallReq, ToolOutcome, TurnTelemetry};

// Integration tests: `#[sqlx::test]` provisions a fresh database, runs the migrations, and hands
// us a pool. Requires a reachable Postgres via `DATABASE_URL` (see `compose.yaml`); skipped when
// none is configured locally. CI runs them against a live Postgres — see
// `.github/workflows/control-plane-tests.yml` (`cargo test -p control-plane` with a postgres service).

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
        trace_context: None,
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
    assert!(
        !has_responded_or_pending_content(&pool, task2)
            .await
            .unwrap()
    );
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
    assert!(
        has_responded_or_pending_content(&pool, task2)
            .await
            .unwrap()
    );
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

/// ADR-0088 merge bar: replaying the terminal `propose_pr` step opens EXACTLY ONE PR intent. The
/// dedup key `(task_id, run_epoch)` makes the outbox insert idempotent, and the offloaded branch
/// blob is idempotent on its content hash — so an at-least-once/replayed proposal never opens a
/// duplicate PR and never duplicates the offloaded patch.
#[sqlx::test]
async fn pr_open_intent_is_dedup_keyed_by_task_and_run_epoch(pool: PgPool) {
    let repo_id = seed(&pool).await;
    let task = create_task(&pool, &pr_task(repo_id, "open"))
        .await
        .unwrap()
        .expect("task");
    let run_epoch = 0;
    let key = crate::outbox::pr_open_dedup_key(task, run_epoch);
    let payload = serde_json::json!({
        "branch": "open/357", "title": "Add feature", "body": "…", "content_hash": "abc123",
    });

    // Offload the patch twice (a replay re-stores the same bytes) — idempotent on content hash.
    put_pr_open_blob(&pool, "abc123", task, run_epoch, b"PATCH-BYTES")
        .await
        .unwrap();
    put_pr_open_blob(&pool, "abc123", task, run_epoch, b"PATCH-BYTES")
        .await
        .unwrap();
    assert_eq!(
        get_pr_open_blob(&pool, "abc123").await.unwrap().as_deref(),
        Some(&b"PATCH-BYTES"[..]),
        "the offloaded branch rehydrates by content hash"
    );

    // First proposal enqueues the intent; a replay with the SAME (task, run_epoch) key is a no-op.
    assert!(
        enqueue_outbox_post(
            &pool,
            Platform::GitHub,
            Some(task),
            99,
            "o",
            "r",
            "pr_open",
            &payload,
            &key,
        )
        .await
        .unwrap(),
        "first propose_pr enqueues the pr_open intent"
    );
    assert!(
        !enqueue_outbox_post(
            &pool,
            Platform::GitHub,
            Some(task),
            99,
            "o",
            "r",
            "pr_open",
            &payload,
            &key,
        )
        .await
        .unwrap(),
        "a replayed propose_pr with the same (task, run_epoch) key opens no second PR"
    );

    // Exactly one pr_open row exists for this task.
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox WHERE task_id = $1 AND kind = 'pr_open'")
            .bind(task)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1, "replay must open exactly one PR intent");
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

    // Clean pass: 👍 on the @mention comment, no review intent.
    let t_clean = crate::outbox::Target {
        task_id: Some(clean),
        platform: Platform::GitHub,
        installation_id: 99,
        owner: "o",
        repo: "r",
    };
    crate::outbox::enqueue_verdict_reaction(&pool, &t_clean, 7, "+1", Some(555), "pull_request")
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
    assert!(
        !has_review_intent_or_posted_review(&pool, task)
            .await
            .unwrap()
    );

    // A silent-clean persisted row (no platform_review_id) does NOT trip the guard — re-running the
    // clean path is harmless (insert-if-absent no-ops, verdict key no-ops).
    insert_review_if_absent(&pool, task, "clean", "body", 0, 0, 0, &findings)
        .await
        .unwrap();
    assert!(
        !has_review_intent_or_posted_review(&pool, task)
            .await
            .unwrap()
    );

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
    assert!(
        has_review_intent_or_posted_review(&pool, task)
            .await
            .unwrap()
    );

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
    assert!(
        has_review_intent_or_posted_review(&pool, task2)
            .await
            .unwrap()
    );
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
        trace_context: None,
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
        trace_context: None,
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
    assert!(
        reconcile_embedding_dimension(&pool, 1536, false)
            .await
            .is_err()
    );
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
                trace_context: None,
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

/// `list_tasks` is the dashboard/TUI run list and must return most-recent-first. `tasks.id` is a
/// random UUIDv4, so ordering by it is effectively random — this guards against regressing to an
/// id-based ORDER BY (which hid recent runs entirely once older rows crowded the LIMIT window).
#[sqlx::test]
async fn list_tasks_returns_most_recent_first(pool: PgPool) {
    let repo = upsert_repository(&pool, Platform::GitHub, 9, "vymalo", "runs", "main", None)
        .await
        .unwrap();

    // Create three tasks, then stamp deterministic, distinct created_at values so the expected
    // order is unambiguous and independent of the (random) UUIDs.
    let mut ids = Vec::new();
    for (n, delivery) in ["r-1", "r-2", "r-3"].iter().enumerate() {
        record_delivery(
            &pool,
            Platform::GitHub,
            delivery,
            "pull_request",
            &json!({}),
        )
        .await
        .unwrap();
        let id = create_task(
            &pool,
            &NewTask {
                repository_id: repo,
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
                trace_context: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        ids.push(id);
    }
    let (oldest, middle, newest) = (ids[0], ids[1], ids[2]);

    // Insertion order is NOT recency order: make the first-inserted the newest.
    for (id, hours_ago) in [(newest, 1_i32), (middle, 2), (oldest, 3)] {
        sqlx::query("UPDATE tasks SET created_at = now() - ($2 * interval '1 hour') WHERE id = $1")
            .bind(id)
            .bind(hours_ago)
            .execute(&pool)
            .await
            .unwrap();
    }

    let tasks = list_tasks(&pool, 100).await.unwrap();
    let order: Vec<Uuid> = tasks.iter().map(|t| t.id).collect();
    assert_eq!(
        order,
        vec![newest, middle, oldest],
        "list_tasks must return most-recent-first by created_at"
    );
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
    assert!(
        list_repositories(&pool, Some("approved"))
            .await
            .unwrap()
            .is_empty()
    );

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
    assert!(
        list_repositories(&pool, Some("pending"))
            .await
            .unwrap()
            .iter()
            .any(|r| r.platform_repo_id == 5555)
    );
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
    let repo_b = upsert_repository(&pool, Platform::GitHub, 9001, "octo", "other", "main", None)
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

    let f_same = serde_json::json!([{ "file": "a.rs", "line": 3, "title": "leak", "body": "b" }]);
    let f_other = serde_json::json!([{ "file": "b.rs", "line": 9, "title": "other", "body": "b" }]);
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
    assert!(
        fb.iter()
            .any(|r| r.reactor == "alice" && r.reaction == "+1")
    );
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
    assert!(
        fb.iter()
            .any(|r| r.reactor == "carol" && r.reaction == "heart")
    );

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
            trace_context: None,
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
    add_pending_comment(&pool, task_id, None, None, "first comment")
        .await
        .unwrap();
    add_pending_comment(&pool, task_id, None, None, "second comment")
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

/// ADR-0087 Gap 1 (resume-aware buffer): the `running` transition only clears the review buffer
/// on a *genuinely fresh* attempt (no journaled steps). A *resumed* run (a `durable_step` row
/// exists for the same `(task, run_epoch)`) replays its write-step results instead of
/// re-executing them, so clearing would drop findings that never get re-buffered. This drives the
/// real handler decision (`is_fresh_attempt` + the conditional `clear_pending_review`) end to end.
#[sqlx::test]
async fn running_transition_preserves_buffer_on_resume_but_clears_on_fresh(pool: PgPool) {
    let repo_id = seed(&pool).await;
    let task = claim_after_create(&pool, repo_id, "head-resume").await;
    let run_epoch = durable_step_run_epoch(&pool, task).await.unwrap().unwrap();

    // Buffer one finding, then simulate a crash-and-resume: a journaled write step exists for the
    // SAME run_epoch. The resumed runner reports `running`.
    upsert_pending_inline(
        &pool,
        task,
        "a.rs",
        3,
        Some("t"),
        Some("P1"),
        Some("correctness"),
        None,
        "a finding from the prior attempt",
    )
    .await
    .unwrap();
    upsert_durable_step(&pool, task, run_epoch, "tools:5", "{\"ok\":true}", "hash-5")
        .await
        .unwrap();

    // The handler gate: a resumed run is NOT a fresh attempt, so the clear is SKIPPED.
    assert!(
        !crate::http::internal::is_fresh_attempt(&pool, task).await,
        "a run with journaled steps is a resume, not a fresh attempt"
    );
    if crate::http::internal::is_fresh_attempt(&pool, task).await {
        clear_pending_review(&pool, task).await.unwrap();
    }
    let after_resume = load_pending_review(&pool, task).await.unwrap();
    assert_eq!(
        after_resume.inline.len(),
        1,
        "the resumed run keeps the buffered finding (replay does not re-buffer it)"
    );

    // Inverse: with NO durable steps (the flag-off / first-attempt world) the clear runs — exactly
    // today's ADR-0037 behavior.
    purge_durable_steps(&pool, task, run_epoch).await.unwrap();
    assert!(
        crate::http::internal::is_fresh_attempt(&pool, task).await,
        "with no journaled steps the run is a fresh attempt"
    );
    if crate::http::internal::is_fresh_attempt(&pool, task).await {
        clear_pending_review(&pool, task).await.unwrap();
    }
    let after_fresh = load_pending_review(&pool, task).await.unwrap();
    assert!(
        after_fresh.is_empty(),
        "a fresh attempt clears the buffer (byte-identical to pre-ADR-0087)"
    );
}

/// ADR-0087 Gap 2 (`add_comment` dedup): a replayed reply with the same `(task, run_epoch,
/// call_id)` is a no-op (one row); a different `call_id` is a distinct reply (two rows); a NULL
/// `call_id` (legacy / flag-off path) still appends unconditionally (no dedup).
#[sqlx::test]
async fn add_pending_comment_dedups_on_call_id(pool: PgPool) {
    let repo_id = seed(&pool).await;
    let task = claim_after_create(&pool, repo_id, "head-dedup").await;
    let run_epoch = durable_step_run_epoch(&pool, task).await.unwrap().unwrap();

    let count = |pool: PgPool| async move {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM pending_review_actions WHERE action = 'comment'",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };

    // Same call_id twice → the replay is a no-op.
    add_pending_comment(&pool, task, Some(run_epoch), Some("call_1"), "first")
        .await
        .unwrap();
    add_pending_comment(
        &pool,
        task,
        Some(run_epoch),
        Some("call_1"),
        "first (replayed)",
    )
    .await
    .unwrap();
    assert_eq!(
        count(pool.clone()).await,
        1,
        "a replayed reply with the same call_id dedups to one row"
    );

    // A different call_id is a genuinely distinct reply.
    add_pending_comment(&pool, task, Some(run_epoch), Some("call_2"), "second")
        .await
        .unwrap();
    assert_eq!(count(pool.clone()).await, 2, "a different call_id appends");

    // Legacy path: NULL call_id is excluded from the partial index → always appends.
    add_pending_comment(&pool, task, None, None, "legacy-a")
        .await
        .unwrap();
    add_pending_comment(&pool, task, None, None, "legacy-b")
        .await
        .unwrap();
    assert_eq!(
        count(pool.clone()).await,
        4,
        "NULL-call_id comments append unconditionally (no dedup)"
    );
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

    assert!(
        set_task_status(&pool, task, "succeeded", None)
            .await
            .unwrap()
    );

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
    assert!(
        set_task_status(
            &pool,
            task,
            "succeeded",
            Some("Review produced no comments to post.")
        )
        .await
        .unwrap()
    );
    let detail: Option<String> = sqlx::query_scalar("SELECT error_detail FROM tasks WHERE id = $1")
        .bind(task)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        detail.as_deref(),
        Some("Review produced no comments to post.")
    );

    // A later report without a detail must not erase the recorded reason.
    assert!(
        set_task_status(&pool, task, "succeeded", None)
            .await
            .unwrap()
    );
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
    sqlx::query(
        "UPDATE code_chunks SET created_at = now() - interval '1 hour' WHERE repository_id = $1",
    )
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

// ── ADR-0087 durable-step store: the PRODUCTION journal path ─────────────────────────────────
// The resume proof (agent-clients `InMemoryStepStore` tests) never touches Postgres. These tests
// exercise the real `ControlPlaneStepStore` backing: the `jsonb` write + `result::text` read
// round-trip, the `(task_id, run_epoch, step_name)` keying, and the replay-idempotent upsert.

/// A realistic `AssistantTurn` (an `llm_turn:{n}` step result) built from the REAL loop type, so
/// the round-trip is asserted against exactly what the loop serializes — including the serde
/// contract the loop relies on: `ToolCallReq.kind` renames to `"type"` and `extra_content: None`
/// is skipped. A hand-authored `Value` would silently drift from this. Carries `telemetry` too
/// (#411/#417): that's the field this journal round-trip must preserve so a resumed turn's
/// reasoning/token counts survive replay instead of silently reading back as `reasoning_chars: 0`.
fn assistant_turn_result() -> AssistantTurn {
    AssistantTurn {
        content: Some("Looking at the diff now.".into()),
        tool_calls: vec![ToolCallReq {
            id: "call_abc".into(),
            kind: "function".into(),
            function: FunctionCallReq {
                name: "read_file".into(),
                arguments: "{\"path\":\"src/db.rs\"}".into(),
            },
            extra_content: None,
        }],
        telemetry: Some(TurnTelemetry {
            model: "glm-5p2".into(),
            prompt_tokens: Some(20_775),
            completion_tokens: Some(370),
            reasoning_tokens: Some(0),
            reasoning: Some("Let me look at the diff before deciding.".into()),
        }),
    }
}

/// A realistic `tools:{n}` step result: the loop journals `T = Vec<(usize, ToolOutcome)>` (the
/// ordered read-batch outcomes). `ToolOutcome` is externally tagged, so this serializes to
/// `[[0,{"Continue":"…"}]]` — a shape a hand-written literal is easy to get wrong.
fn tool_output_result() -> Vec<(usize, ToolOutcome)> {
    vec![(0, ToolOutcome::Continue("fn main() {}\n".into()))]
}

/// The gate #363 P1: the entire resume proof runs on `InMemoryStepStore`; this drives the REAL
/// Postgres store with the REAL journaled types. For each of `AssistantTurn` (`llm_turn`) and
/// `Vec<(usize, ToolOutcome)>` (`tools`): serialize via `to_value` exactly as the loop does, upsert,
/// fetch back, and assert BOTH (a) the fetched `Value` semantically equals the journaled `Value`
/// (jsonb normalizes whitespace/key order/number formatting, so this is *not* a raw-string check),
/// AND (b) `serde_json::from_value::<T>(fetched)` rehydrates to the ORIGINAL typed value — the exact
/// call `CheckpointRuntime::step` makes on replay. (b) is the assertion that catches a type which
/// survives `Value`-equality but breaks typed rehydration through jsonb.
#[sqlx::test]
async fn durable_step_round_trips_a_real_step_result_through_jsonb(pool: PgPool) {
    let task_id = Uuid::new_v4();
    let run_epoch = 3;

    // ── llm_turn: AssistantTurn ──────────────────────────────────────────────────────────────
    let turn = assistant_turn_result();
    let turn_value = serde_json::to_value(&turn).unwrap();
    upsert_durable_step(
        &pool,
        task_id,
        run_epoch,
        "llm_turn:0",
        &serde_json::to_string(&turn_value).unwrap(),
        "sha256:turn",
    )
    .await
    .expect("upsert the journaled AssistantTurn");

    let fetched = fetch_durable_step(&pool, task_id, run_epoch, "llm_turn:0")
        .await
        .expect("fetch the journaled step")
        .expect("the step was journaled, so it must be found");
    assert_eq!(
        fetched.content_hash, "sha256:turn",
        "the content hash round-trips verbatim"
    );
    let fetched_value: serde_json::Value =
        serde_json::from_str(fetched.result.as_deref().expect("result is set"))
            .expect("result::text is valid JSON");
    assert_eq!(
        fetched_value, turn_value,
        "(a) the AssistantTurn Value round-trips semantically through jsonb"
    );
    let rehydrated: AssistantTurn = serde_json::from_value(fetched_value).expect(
        "(b) result::text rehydrates into the real AssistantTurn — the step<T> replay call",
    );
    assert_eq!(
        rehydrated, turn,
        "(b) from_value::<AssistantTurn> yields the original typed value (what CheckpointRuntime::step does)"
    );

    // ── tools: Vec<(usize, ToolOutcome)> ─────────────────────────────────────────────────────
    let outputs = tool_output_result();
    let outputs_value = serde_json::to_value(&outputs).unwrap();
    upsert_durable_step(
        &pool,
        task_id,
        run_epoch,
        "tools:0",
        &serde_json::to_string(&outputs_value).unwrap(),
        "sha256:tools",
    )
    .await
    .expect("upsert the journaled tool outputs");

    let fetched_tools = fetch_durable_step(&pool, task_id, run_epoch, "tools:0")
        .await
        .unwrap()
        .expect("tools:0 is journaled");
    let fetched_tools_value: serde_json::Value =
        serde_json::from_str(fetched_tools.result.as_deref().unwrap()).unwrap();
    assert_eq!(
        fetched_tools_value, outputs_value,
        "(a) the tool-output Vec Value round-trips semantically through jsonb"
    );
    let rehydrated_tools: Vec<(usize, ToolOutcome)> = serde_json::from_value(fetched_tools_value)
        .expect(
            "(b) result::text rehydrates into Vec<(usize, ToolOutcome)> — the step<T> replay call",
        );
    assert_eq!(
        rehydrated_tools, outputs,
        "(b) from_value::<Vec<(usize, ToolOutcome)>> yields the original typed outputs"
    );
}

/// The `(task_id, run_epoch, step_name)` keying + the replay-idempotent upsert: a wrong tuple
/// component finds nothing (the replay gap where the loop continues live), and re-upserting the
/// same key overwrites in place rather than duplicating (`ON CONFLICT DO UPDATE`).
#[sqlx::test]
async fn durable_step_keys_on_the_run_identity_tuple_and_upsert_is_idempotent(pool: PgPool) {
    let task_id = Uuid::new_v4();
    let run_epoch = 1;

    let turn = assistant_turn_result();
    let turn_value = serde_json::to_value(&turn).unwrap();
    upsert_durable_step(
        &pool,
        task_id,
        run_epoch,
        "llm_turn:0",
        &serde_json::to_string(&turn_value).unwrap(),
        "hash-turn",
    )
    .await
    .unwrap();
    let tools_value = serde_json::to_value(tool_output_result()).unwrap();
    upsert_durable_step(
        &pool,
        task_id,
        run_epoch,
        "tools:0",
        &serde_json::to_string(&tools_value).unwrap(),
        "hash-tools",
    )
    .await
    .unwrap();

    // Each step is keyed independently by name.
    let got_turn = fetch_durable_step(&pool, task_id, run_epoch, "llm_turn:0")
        .await
        .unwrap()
        .expect("llm_turn:0 is journaled");
    let rehydrated: AssistantTurn =
        serde_json::from_str(got_turn.result.as_deref().unwrap()).unwrap();
    assert_eq!(rehydrated, turn, "the right step returns its own result");

    // A wrong tuple component → the replay gap (Ok(None), continue live), not a spurious hit.
    assert!(
        fetch_durable_step(&pool, task_id, run_epoch, "llm_turn:99")
            .await
            .unwrap()
            .is_none(),
        "an un-journaled step name is a gap, not a hit"
    );
    assert!(
        fetch_durable_step(&pool, task_id, run_epoch + 1, "llm_turn:0")
            .await
            .unwrap()
            .is_none(),
        "a different run_epoch is a different run — no cross-epoch bleed"
    );
    assert!(
        fetch_durable_step(&pool, Uuid::new_v4(), run_epoch, "llm_turn:0")
            .await
            .unwrap()
            .is_none(),
        "a different task_id sees none of this run's journal"
    );

    // Re-running the same step overwrites its row (replay-idempotent) rather than duplicating.
    let revised = json!({ "content": "revised", "tool_calls": [] });
    upsert_durable_step(
        &pool,
        task_id,
        run_epoch,
        "llm_turn:0",
        &serde_json::to_string(&revised).unwrap(),
        "hash-revised",
    )
    .await
    .unwrap();
    let after = fetch_durable_step(&pool, task_id, run_epoch, "llm_turn:0")
        .await
        .unwrap()
        .expect("still present after the re-upsert");
    assert_eq!(
        after.content_hash, "hash-revised",
        "the upsert overwrote in place"
    );
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM durable_step WHERE task_id = $1 AND run_epoch = $2 AND step_name = $3",
    )
    .bind(task_id)
    .bind(run_epoch)
    .bind("llm_turn:0")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows, 1,
        "ON CONFLICT overwrote the row — it did not duplicate"
    );
}

/// `durable_step_run_epoch` resolves the run's `run_epoch` from the task row (the server-side
/// derivation that keeps the agent from supplying its own epoch — the trust boundary), and returns
/// `None` for an unknown task.
#[sqlx::test]
async fn durable_step_run_epoch_resolves_from_the_task_row(pool: PgPool) {
    let repo_id = seed(&pool).await;
    let task_id = create_task(&pool, &pr_task(repo_id, "head1"))
        .await
        .unwrap()
        .expect("the review task");

    let epoch = durable_step_run_epoch(&pool, task_id)
        .await
        .unwrap()
        .expect("a live task resolves its run_epoch");
    assert_eq!(epoch, 0, "pr_task seeds run_epoch 0");

    assert!(
        durable_step_run_epoch(&pool, Uuid::new_v4())
            .await
            .unwrap()
            .is_none(),
        "an unknown task has no run_epoch"
    );
}

// ---------------------------------------------------------------------------
// #400 (bug 2): the six `ORDER BY <timestamp>` queries with no secondary tie-breaker could return
// same-timestamp rows in non-deterministic order. Each fix adds an `id`-family tie-break; these
// tests pin the tie-break by forcing a genuine timestamp tie and asserting the resulting order is
// exactly the (timestamp, id) order — not merely "stable within one run", which a tiny freshly
// inserted table can appear to satisfy even without the fix.
// ---------------------------------------------------------------------------

/// `list_pollable_comments`'s `ORDER BY rc.created_at, rc.id`: two review comments on the same task
/// forced to an identical `created_at` must list in exactly the (created_at, id) order — computed
/// independently here from `review_comments` directly — not whatever order the heap returns.
#[sqlx::test]
async fn list_pollable_comments_ties_break_on_id(pool: PgPool) {
    let repo_id = seed(&pool).await;
    let task_id = create_task(&pool, &pr_task(repo_id, "head1"))
        .await
        .unwrap()
        .unwrap();

    store_review_comments(
        &pool,
        task_id,
        &[
            ReviewCommentRef {
                platform_comment_id: 1,
                kind: "inline".to_string(),
                file: Some("a.rs".to_string()),
                line: Some(1),
            },
            ReviewCommentRef {
                platform_comment_id: 2,
                kind: "inline".to_string(),
                file: Some("b.rs".to_string()),
                line: Some(2),
            },
        ],
    )
    .await
    .unwrap();

    // Force an exact created_at tie (separate INSERTs would otherwise likely differ by microseconds).
    sqlx::query("UPDATE review_comments SET created_at = now() WHERE task_id = $1")
        .bind(task_id)
        .execute(&pool)
        .await
        .unwrap();

    let rows: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT id, platform_comment_id FROM review_comments WHERE task_id = $1 ORDER BY id",
    )
    .bind(task_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    let expected_order: Vec<i64> = rows.iter().map(|(_, pcid)| *pcid).collect();

    let pollable = list_pollable_comments(&pool, 7, 3600).await.unwrap();
    let order: Vec<i64> = pollable
        .into_iter()
        .filter(|c| c.task_id == task_id)
        .map(|c| c.platform_comment_id)
        .collect();
    assert_eq!(
        order, expected_order,
        "a created_at tie must break on id, matching an independent (created_at, id) query"
    );
}

/// `get_feedback`'s `ORDER BY f.created_at, f.id`: `reconcile_comment_feedback` inserts several
/// reactions inside ONE transaction, so they share the exact same `created_at` (Postgres `now()` is
/// stable for a whole transaction) — a genuine tie with no manual timestamp manipulation needed.
#[sqlx::test]
async fn get_feedback_ties_break_on_id(pool: PgPool) {
    let repo_id = seed(&pool).await;
    let task_id = create_task(&pool, &pr_task(repo_id, "head1"))
        .await
        .unwrap()
        .unwrap();

    reconcile_comment_feedback(
        &pool,
        task_id,
        555,
        "inline",
        &[
            ("alice".to_string(), "+1".to_string()),
            ("bob".to_string(), "-1".to_string()),
            ("carol".to_string(), "heart".to_string()),
        ],
    )
    .await
    .unwrap();

    let rows: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, reactor FROM review_feedback WHERE task_id = $1 ORDER BY id")
            .bind(task_id)
            .fetch_all(&pool)
            .await
            .unwrap();
    let expected_order: Vec<String> = rows.iter().map(|(_, reactor)| reactor.clone()).collect();

    // Assert repeatedly: a same-timestamp tie with no secondary key can flip between calls even
    // within one connection, so this also proves the order is stable, not just correct once.
    for _ in 0..3 {
        let feedback = get_feedback(&pool, task_id).await.unwrap();
        let order: Vec<String> = feedback.into_iter().map(|f| f.reactor).collect();
        assert_eq!(
            order, expected_order,
            "a created_at tie must break on id, and stay stable across repeated reads"
        );
    }
}

/// `claim_next_push_config`'s `ORDER BY c.next_attempt_at, c.config_id`: two due configs on the same
/// task forced to an identical `next_attempt_at` must claim in ascending `config_id` order — the
/// lower id first, then the higher, then nothing (both now leased).
#[sqlx::test]
async fn claim_next_push_config_ties_break_on_config_id(pool: PgPool) {
    let a2a_task_id = Uuid::new_v4();
    let task_json = json!({ "id": a2a_task_id.to_string() });
    sqlx::query(
        "INSERT INTO a2a_tasks (a2a_task_id, context_id, caller_id, skill, state, version, task_json) \
         VALUES ($1, 'ctx', 'caller', 'review', 'TASK_STATE_WORKING', 1, $2)",
    )
    .bind(a2a_task_id)
    .bind(&task_json)
    .execute(&pool)
    .await
    .unwrap();
    // One undelivered event so both configs are "due" (delivered_seq 0 < max(seq)).
    sqlx::query(
        "INSERT INTO a2a_task_events (a2a_task_id, seq, kind, state, final, payload) \
         VALUES ($1, 1, 'status-update', 'TASK_STATE_WORKING', false, $2)",
    )
    .bind(a2a_task_id)
    .bind(&task_json)
    .execute(&pool)
    .await
    .unwrap();

    let config_a = Uuid::new_v4();
    let config_b = Uuid::new_v4();
    insert_push_config(
        &pool,
        config_a,
        a2a_task_id,
        "https://93.184.216.34/a",
        None,
        "caller",
    )
    .await
    .unwrap();
    insert_push_config(
        &pool,
        config_b,
        a2a_task_id,
        "https://93.184.216.34/b",
        None,
        "caller",
    )
    .await
    .unwrap();
    // Force an exact tie on next_attempt_at (both default to now(), but separate INSERTs can differ
    // by microseconds).
    sqlx::query("UPDATE a2a_push_configs SET next_attempt_at = now() WHERE a2a_task_id = $1")
        .bind(a2a_task_id)
        .execute(&pool)
        .await
        .unwrap();

    let (lower, higher) = if config_a < config_b {
        (config_a, config_b)
    } else {
        (config_b, config_a)
    };

    let first = claim_next_push_config(&pool, "owner-a", Duration::from_secs(60))
        .await
        .unwrap()
        .expect("one of the two due configs is claimed");
    assert_eq!(first.config_id, lower, "the lower config_id wins the tie");

    let second = claim_next_push_config(&pool, "owner-b", Duration::from_secs(60))
        .await
        .unwrap()
        .expect("the second due config is claimed next");
    assert_eq!(second.config_id, higher);

    assert!(
        claim_next_push_config(&pool, "owner-c", Duration::from_secs(60))
            .await
            .unwrap()
            .is_none(),
        "both configs are now leased — nothing left to claim"
    );
}

/// `claim_next_task`'s `ORDER BY priority DESC, created_at, id`: two queued tasks with the same
/// (default) priority and an identical `created_at` must claim in ascending `id` order.
#[sqlx::test]
async fn claim_next_task_ties_break_on_id(pool: PgPool) {
    let repo_id = seed(&pool).await;
    let first = create_task(&pool, &pr_task(repo_id, "head1"))
        .await
        .unwrap()
        .unwrap();
    let second = create_task(&pool, &pr_task(repo_id, "head2"))
        .await
        .unwrap()
        .unwrap();

    // Force an exact created_at tie (both already share the default priority 100).
    sqlx::query("UPDATE tasks SET created_at = now() WHERE id = ANY($1)")
        .bind([first, second].as_slice())
        .execute(&pool)
        .await
        .unwrap();

    let (lower, higher) = if first < second {
        (first, second)
    } else {
        (second, first)
    };

    let claimed_first = claim_next_task(&pool, "owner-a", Duration::from_secs(60))
        .await
        .unwrap()
        .expect("a queued task is claimed");
    assert_eq!(
        claimed_first.id, lower,
        "the lower id wins the created_at tie"
    );

    let claimed_second = claim_next_task(&pool, "owner-b", Duration::from_secs(60))
        .await
        .unwrap()
        .expect("the second queued task is claimed next");
    assert_eq!(claimed_second.id, higher);

    assert!(
        claim_next_task(&pool, "owner-c", Duration::from_secs(60))
            .await
            .unwrap()
            .is_none(),
        "both tasks are now running — nothing left to claim"
    );
}

/// `list_reapable_tasks`'s `ORDER BY started_at NULLS FIRST, id`: two expired-lease `running` tasks
/// with an identical `started_at` must list in ascending `id` order.
#[sqlx::test]
async fn list_reapable_tasks_ties_break_on_id(pool: PgPool) {
    let repo_id = seed(&pool).await;
    let first = create_task(&pool, &pr_task(repo_id, "head1"))
        .await
        .unwrap()
        .unwrap();
    let second = create_task(&pool, &pr_task(repo_id, "head2"))
        .await
        .unwrap()
        .unwrap();

    claim_next_task(&pool, "owner-a", Duration::from_secs(60))
        .await
        .unwrap();
    claim_next_task(&pool, "owner-a", Duration::from_secs(60))
        .await
        .unwrap();

    // Force both leases already-expired and started_at an exact tie.
    sqlx::query(
        "UPDATE tasks SET started_at = now(), lease_expires_at = now() - interval '1 minute' \
         WHERE id = ANY($1)",
    )
    .bind([first, second].as_slice())
    .execute(&pool)
    .await
    .unwrap();

    let (lower, higher) = if first < second {
        (first, second)
    } else {
        (second, first)
    };

    let reapable = list_reapable_tasks(&pool, 100).await.unwrap();
    let order: Vec<Uuid> = reapable.iter().map(|t| t.id).collect();
    assert_eq!(
        order,
        vec![lower, higher],
        "a started_at tie must break on id ascending"
    );
}
