//! A2A task-mapping retention sweeper (RFC-0006, ADR-0077 §S3 / #321).
//!
//! `a2a_tasks` and its `ON DELETE CASCADE` children (`a2a_task_events`, `a2a_push_configs`) are
//! append-only: every A2A submission leaves a permanent mapping row plus its whole event log, and the
//! cascade is correct but nothing ever deletes the *parent*. Left alone the tables grow without bound.
//!
//! This sweeper — run periodically by the dispatcher alongside the index (ADR-0052) and outbox
//! (ADR-0059) sweepers — deletes **terminal** mappings past a TTL, a bounded batch per tick so a large
//! backlog is drained across several ticks without ever holding a long table lock. A still-running
//! (SUBMITTED / WORKING) mapping is never touched, whatever its age (see
//! [`crate::db::sweep_terminal_a2a_tasks`] for the terminal predicate and the load-bearing safety
//! argument). Best-effort + idempotent (a `DELETE` is naturally so): a failed cycle is logged and
//! retried next tick.

use sqlx::PgPool;

use crate::{db, http::metrics};

/// One sweep cycle: delete up to `batch` terminal `a2a_tasks` mappings older than `ttl_days` (their
/// events + push configs cascade). Cheap when there is nothing to do (a bounded partial `DELETE`).
pub async fn sweep_once(pool: &PgPool, ttl_days: i64, batch: i64) -> anyhow::Result<()> {
    let deleted = db::sweep_terminal_a2a_tasks(pool, ttl_days, batch).await?;
    if deleted > 0 {
        metrics::a2a_task_sweep_deleted(deleted);
        tracing::info!(
            deleted,
            ttl_days,
            batch,
            "a2a task sweeper: reaped terminal mappings (events + push configs cascaded)"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;

    /// Seed one `a2a_tasks` mapping at a controlled age and wire state, and return its id. `state` is
    /// the SCREAMING_SNAKE snapshot; `age_days` back-dates `created_at`.
    async fn seed_task(pool: &PgPool, state: &str, age_days: i64) -> Uuid {
        let id = Uuid::now_v7();
        let task = json!({
            "id": id.to_string(),
            "contextId": "ctx-test",
            "status": { "state": state }
        });
        sqlx::query(
            "INSERT INTO a2a_tasks \
                 (a2a_task_id, context_id, caller_id, skill, state, version, task_json, created_at) \
             VALUES ($1, 'ctx-test', 'svc-a', 'review', $2, 1, $3, \
                     now() - make_interval(days => $4::int))",
        )
        .bind(id)
        .bind(state)
        .bind(&task)
        .bind(age_days)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    /// Append one event to a task; `final_` marks the terminal (log-freezing) event.
    async fn seed_event(pool: &PgPool, a2a_task_id: Uuid, seq: i64, final_: bool) {
        let payload = json!({ "statusUpdate": { "taskId": a2a_task_id.to_string(), "seq": seq } });
        sqlx::query(
            "INSERT INTO a2a_task_events (a2a_task_id, seq, kind, state, final, payload) \
             VALUES ($1, $2, 'status-update', 'TASK_STATE_WORKING', $3, $4)",
        )
        .bind(a2a_task_id)
        .bind(seq)
        .bind(final_)
        .bind(&payload)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_config(pool: &PgPool, a2a_task_id: Uuid) -> Uuid {
        let config_id = Uuid::now_v7();
        db::insert_push_config(
            pool,
            config_id,
            a2a_task_id,
            "https://93.184.216.34/hook",
            None,
            "svc-a",
        )
        .await
        .unwrap();
        config_id
    }

    async fn task_exists(pool: &PgPool, id: Uuid) -> bool {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM a2a_tasks WHERE a2a_task_id = $1)",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// A terminal mapping past the TTL is reaped, and its events + push configs cascade away with it.
    #[sqlx::test(migrations = "./migrations")]
    async fn terminal_task_past_ttl_is_deleted_and_children_cascade(pool: PgPool) {
        // Terminal via a frozen event log (final=true) AND old.
        let done = seed_task(&pool, "TASK_STATE_WORKING", 40).await;
        seed_event(&pool, done, 1, false).await;
        seed_event(&pool, done, 2, true).await; // terminal event
        let cfg = seed_config(&pool, done).await;

        let deleted = db::sweep_terminal_a2a_tasks(&pool, 30, 100).await.unwrap();
        assert_eq!(deleted, 1, "the one over-TTL terminal mapping is swept");
        assert!(!task_exists(&pool, done).await, "mapping deleted");

        // Cascades: no orphan events or configs remain.
        let events: i64 =
            sqlx::query_scalar("SELECT count(*) FROM a2a_task_events WHERE a2a_task_id = $1")
                .bind(done)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(events, 0, "events cascade away with the parent");
        assert!(
            db::get_push_config(&pool, cfg).await.unwrap().is_none(),
            "push config cascades away with the parent"
        );
    }

    /// A REJECTED gate outcome (terminal snapshot state, no event log) past the TTL is also reaped —
    /// the belt-and-braces branch for a mapping whose best-effort terminal-event append was lost.
    #[sqlx::test(migrations = "./migrations")]
    async fn terminal_snapshot_state_without_events_is_deleted(pool: PgPool) {
        let rejected = seed_task(&pool, "TASK_STATE_REJECTED", 40).await;
        let deleted = db::sweep_terminal_a2a_tasks(&pool, 30, 100).await.unwrap();
        assert_eq!(deleted, 1);
        assert!(!task_exists(&pool, rejected).await);
    }

    /// Retained: a recent terminal task (inside the TTL) and an old NON-terminal (still WORKING,
    /// no final event) task both survive — retention never reaps a live task, whatever its age.
    #[sqlx::test(migrations = "./migrations")]
    async fn recent_or_nonterminal_tasks_are_retained(pool: PgPool) {
        // Recent terminal: final event but only 2 days old → kept (inside 30d TTL).
        let recent_done = seed_task(&pool, "TASK_STATE_WORKING", 2).await;
        seed_event(&pool, recent_done, 1, true).await;

        // Old but non-terminal: 40 days old, WORKING snapshot, no final event → kept.
        let old_working = seed_task(&pool, "TASK_STATE_WORKING", 40).await;
        seed_event(&pool, old_working, 1, false).await;

        let deleted = db::sweep_terminal_a2a_tasks(&pool, 30, 100).await.unwrap();
        assert_eq!(deleted, 0, "neither is eligible");
        assert!(
            task_exists(&pool, recent_done).await,
            "recent terminal kept"
        );
        assert!(
            task_exists(&pool, old_working).await,
            "old non-terminal kept — never reap a live task"
        );
    }

    /// A non-positive TTL or batch is a **skip**, never "delete everything" (guards the `interval '0'`
    /// = `now()` footgun), and the batch bounds the delete count per call.
    #[sqlx::test(migrations = "./migrations")]
    async fn zero_ttl_skips_and_batch_bounds_the_delete(pool: PgPool) {
        for _ in 0..3 {
            let t = seed_task(&pool, "TASK_STATE_COMPLETED", 40).await;
            seed_event(&pool, t, 1, true).await;
        }

        // TTL 0 → skip entirely (would otherwise match every terminal row).
        assert_eq!(
            db::sweep_terminal_a2a_tasks(&pool, 0, 100).await.unwrap(),
            0
        );
        // Batch 0 → skip.
        assert_eq!(db::sweep_terminal_a2a_tasks(&pool, 30, 0).await.unwrap(), 0);

        // Bounded batch: only 2 of the 3 eligible rows go this call; the 3rd next call.
        assert_eq!(db::sweep_terminal_a2a_tasks(&pool, 30, 2).await.unwrap(), 2);
        assert_eq!(db::sweep_terminal_a2a_tasks(&pool, 30, 2).await.unwrap(), 1);
        assert_eq!(
            db::sweep_terminal_a2a_tasks(&pool, 30, 2).await.unwrap(),
            0,
            "nothing left; a re-run is a no-op"
        );
    }
}
