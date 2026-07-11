//! Index snapshot sweeper (RFC-0002 / ADR-0052).
//!
//! Every default-branch push writes a full new `(repository_id, commit_sha)` snapshot into pgvector
//! (`code_chunks`) and Neo4j, and nothing reaps the old ones — reviews only ever read the *latest*
//! (`latest_indexed_commit`, ADR-0050). Left alone, a busy repo accumulates a full duplicate index per
//! push in both stores. This sweeper — run periodically by the dispatcher, like the task reaper — keeps
//! only the in-use snapshots per repo and prunes the rest from both stores.
//!
//! Keep-set per repo = the latest indexed commit (what retrieval pins to) ∪ every commit a non-terminal
//! task pins (an in-flight review, or an INDEX task mid-write). A recency grace inside
//! [`db::prune_code_chunks`] additionally spares a just-finished index whose task hasn't flipped to a
//! terminal status yet. Best-effort + idempotent: a per-repo failure is logged and the rest continue.

use sqlx::PgPool;

use crate::{db, http::metrics, integrations::neo4j};

/// One sweep cycle: prune stale snapshots for every repo that holds more than one. A steady-state repo
/// (one snapshot) isn't returned by [`db::repos_with_stale_snapshots`], so this is cheap when there's
/// nothing to do.
pub async fn sweep_once(pool: &PgPool, neo4j: Option<&neo4rs::Graph>) -> anyhow::Result<()> {
    let repos = db::repos_with_stale_snapshots(pool).await?;
    if repos.is_empty() {
        return Ok(());
    }
    tracing::debug!(
        count = repos.len(),
        "index sweeper: repos with stale snapshots"
    );
    for repository_id in repos {
        if let Err(error) = sweep_repo(pool, neo4j, repository_id).await {
            metrics::index_prune_outcome("error");
            tracing::error!(
                %error, repository_id,
                "index sweeper: prune failed for repo; continuing"
            );
        }
    }
    Ok(())
}

/// Prune one repo. The keep-set never excludes a snapshot retrieval or an in-flight run depends on;
/// if it somehow resolves empty the prune helpers no-op, so we never wipe a live index.
async fn sweep_repo(
    pool: &PgPool,
    neo4j: Option<&neo4rs::Graph>,
    repository_id: i64,
) -> anyhow::Result<()> {
    // Never prune a repo mid-index: an `index` task is the only thing WRITING a new snapshot, it
    // carries a NULL head_sha (so `in_use_commits` can't protect it), and the Neo4j graph has no
    // recency grace. Defer to the next cycle once it completes — deferring GC is harmless.
    if db::has_active_index_task(pool, repository_id).await? {
        tracing::debug!(
            repository_id,
            "index sweeper: active index task; skipping prune this cycle"
        );
        return Ok(());
    }
    let mut keep = db::in_use_commits(pool, repository_id).await?;
    if let Some(latest) = db::latest_indexed_commit(pool, repository_id).await?
        && !keep.contains(&latest)
    {
        keep.push(latest);
    }
    if keep.is_empty() {
        return Ok(());
    }

    let chunks = db::prune_code_chunks(pool, repository_id, &keep).await?;
    let graph_nodes = match neo4j {
        Some(graph) => neo4j::prune_graph(graph, repository_id, &keep).await?,
        None => 0,
    };

    if chunks > 0 || graph_nodes > 0 {
        metrics::index_prune_outcome("pruned");
        metrics::index_prune_deleted(chunks, graph_nodes);
        tracing::info!(
            repository_id,
            kept = keep.len(),
            chunks_deleted = chunks,
            graph_nodes_deleted = graph_nodes,
            "index sweeper: pruned stale snapshots"
        );
    } else {
        // >1 snapshot but all in-use or inside the recency grace — nothing to do this cycle.
        metrics::index_prune_outcome("clean");
    }
    Ok(())
}
