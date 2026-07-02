//! Repository data lifecycle (Epic #75, Milestone B).
//!
//! When a repository is removed from the installation or denied by an admin, its indexed data must be
//! purged so we don't retain code for repos nobody opted into: the semantic index (`code_chunks`),
//! the structural graph (Neo4j), the indexing-state bookkeeping (`repo_index`), and any in-flight
//! tasks (cancelled so the reaper stops their Jobs). The repository row itself is kept (status
//! `disabled`) for audit/history.

use std::sync::Arc;

use sqlx::PgPool;

use crate::AppState;

/// Purge all index data for a repository. Best-effort, but ordered for self-healing: the Neo4j
/// graph goes first and a graph failure keeps the Postgres markers (`code_chunks`/`repo_index`) in
/// place, so the reconcile backstop — which probes only Postgres — re-lists the repo and retries.
/// Safe to run repeatedly (every delete is idempotent).
pub async fn purge_repository_data(
    pool: &PgPool,
    neo4j: Option<&neo4rs::Graph>,
    repository_id: i64,
) {
    // Guard against a re-approve/re-register racing ahead of this spawned purge: only purge if the
    // repo is still `disabled` (a missing row is fine — nothing to keep). Otherwise we'd wipe a
    // freshly re-approved repo's new tasks/index.
    match crate::db::repository_status(pool, repository_id).await {
        Ok(Some(status)) if status != "disabled" => {
            tracing::warn!(
                repository_id,
                status,
                "purge aborted: repository is no longer disabled (re-approved/re-added)"
            );
            return;
        }
        Err(error) => {
            tracing::error!(%error, repository_id, "purge aborted: status check failed");
            return;
        }
        _ => {} // disabled, or row gone → proceed
    }
    let cancelled = match crate::db::cancel_active_tasks_for_repo(pool, repository_id).await {
        Ok(ids) => ids.len(),
        Err(error) => {
            tracing::warn!(%error, repository_id, "purge: cancel tasks failed");
            0
        }
    };
    // Neo4j first, Postgres markers last: `list_disabled_repos_needing_purge` probes only Postgres
    // (`code_chunks`/`repo_index`), so a purge that dies after the markers are gone — the detached
    // task aborted at an await point on shutdown, or the graph delete erroring — would leave graph
    // data orphaned FOREVER (the backstop can't see it). Deleting the graph before the markers, and
    // keeping the markers when the graph delete fails, makes any interrupted purge self-heal: the
    // leftover Postgres rows get the repo re-listed and re-purged (every delete is idempotent).
    let nodes = match neo4j {
        Some(graph) => {
            match crate::integrations::neo4j::delete_repo_graph(graph, repository_id).await {
                Ok(nodes) => nodes,
                Err(error) => {
                    tracing::warn!(%error, repository_id, "purge: delete graph failed; keeping Postgres markers so the backstop retries");
                    return;
                }
            }
        }
        None => 0,
    };
    let chunks = crate::db::delete_code_chunks_for_repo(pool, repository_id)
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(%error, repository_id, "purge: delete code_chunks failed");
            0
        });
    let _ = crate::db::delete_repo_index_rows(pool, repository_id)
        .await
        .map_err(|error| tracing::warn!(%error, repository_id, "purge: delete repo_index failed"));
    tracing::info!(
        repository_id,
        cancelled_tasks = cancelled,
        deleted_chunks = chunks,
        deleted_graph_nodes = nodes,
        "purged repository index data (repo removed/denied)"
    );
}

/// How many leftover repos the reconciler purges per cycle (bounds one tick's work).
const PURGE_RECONCILE_BATCH: i64 = 20;

/// Durable backstop for the data purge: re-purge any `disabled` repo that still has index data.
/// [`spawn_purge`] is prompt but best-effort — a control-plane restart mid-purge loses the spawned
/// task. Run on the dispatcher's periodic loop, this guarantees the purge eventually completes. It's
/// the right home for this (not a per-repo k8s Job): purge writes to Postgres + Neo4j directly, and
/// only the control plane holds those credentials (ADR-0020/0002). Idempotent.
pub async fn reconcile_purges(pool: &PgPool, neo4j: Option<&neo4rs::Graph>) {
    let repos =
        match crate::db::list_disabled_repos_needing_purge(pool, PURGE_RECONCILE_BATCH).await {
            Ok(ids) => ids,
            Err(error) => {
                tracing::warn!(%error, "purge reconcile: listing disabled repos failed");
                return;
            }
        };
    if repos.is_empty() {
        return;
    }
    tracing::info!(
        count = repos.len(),
        "purge reconcile: re-purging repos with leftover data"
    );
    for repository_id in repos {
        purge_repository_data(pool, neo4j, repository_id).await;
    }
}

/// Spawn [`purge_repository_data`] so destructive cleanup never blocks a webhook (its ~10s budget) or
/// an admin request. No-op without a database.
pub fn spawn_purge(state: &AppState, repository_id: i64) {
    let Some(pool) = state.db.clone() else {
        return;
    };
    let neo4j: Option<Arc<neo4rs::Graph>> = state.neo4j.clone();
    tokio::spawn(async move {
        purge_repository_data(&pool, neo4j.as_deref(), repository_id).await;
    });
}
