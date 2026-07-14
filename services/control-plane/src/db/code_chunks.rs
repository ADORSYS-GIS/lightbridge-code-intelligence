//! Semantic index (`code_chunks`) upsert + search, and index-snapshot lifecycle: latest-indexed-commit
//! lookup, per-commit pruning (RFC-0002, ADR-0052), and the purge helpers used when a repo is
//! disabled/removed. Split out of the former monolithic `db.rs` (ADR-0086 follow-up) — pure move, no
//! behavior change.

use serde::Serialize;
use sqlx::PgPool;

/// The `commit_sha` of the repository's **most recently indexed snapshot**, or `None` if it has never
/// been indexed (ADR-0050). This is the single anchor for review reuse: the runner skips the re-index
/// when this is `Some` and pins all retrieval (`search_code_chunks`, graph) to this same commit — so
/// the skip decision and the search scope always reference a commit that *provably has chunks*.
///
/// Why "latest", not the PR head: indexing is maintained on the default branch by re-index-on-push
/// (#183), and a review's value comes from the *base* repo context plus the PR diff (already in the
/// prompt). Pinning to the PR head (the previous behaviour) meant every new head commit looked
/// "not indexed" and triggered a full re-index per PR (slow + costly), while a repo-level "any rows?"
/// check meant searches at the head returned **zero** hits — a hollow index that starved the agent
/// (run `7c15f9bb`). Anchoring to the latest indexed snapshot fixes both: real hits, no per-PR re-index.
pub async fn latest_indexed_commit(
    pool: &PgPool,
    repository_id: i64,
) -> Result<Option<String>, sqlx::Error> {
    // `id DESC` tie-breaks when two snapshots share a `created_at` (coarse clock, or rows written in one
    // transaction where `now()` is constant) — `id` is BIGSERIAL, so the most-recently-inserted snapshot
    // wins deterministically. Backed by the `(repository_id, created_at DESC, id DESC)` index (migration
    // 0018) so this is an index lookup, not a scan — it runs on every search/graph query via `task_scope`.
    sqlx::query_scalar(
        "SELECT commit_sha FROM code_chunks WHERE repository_id = $1 \
         ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(repository_id)
    .fetch_optional(pool)
    .await
}

/// Delete a repository's semantic index (all `code_chunks` rows) — part of the data purge when a repo
/// is removed/denied (Epic #75, Milestone B). Returns the number of rows deleted.
pub async fn delete_code_chunks_for_repo(
    pool: &PgPool,
    repository_id: i64,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM code_chunks WHERE repository_id = $1")
        .bind(repository_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// Delete a repository's indexing-state rows (`repo_index`) — completes the purge bookkeeping so a
/// later re-add reindexes from scratch.
pub async fn delete_repo_index_rows(pool: &PgPool, repository_id: i64) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM repo_index WHERE repository_id = $1")
        .bind(repository_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

// ── Index snapshot pruning (RFC-0002, ADR-0052) ──────────────────────────────────────────────────
// Every default-branch push writes a full new `(repository_id, commit_sha)` snapshot into
// `code_chunks` (+ Neo4j) and nothing reaps the old ones — reviews only ever read the *latest*
// (`latest_indexed_commit`, ADR-0050). The index sweeper keeps only the in-use snapshots per repo and
// prunes the rest. These helpers are the Postgres half; the Neo4j half is `neo4j::prune_graph`.

/// Repos that currently hold MORE THAN ONE distinct `commit_sha` in `code_chunks` — i.e. the only
/// repos with anything prunable. The sweeper iterates these so a steady-state repo (one snapshot)
/// costs a single grouped count, not a per-repo delete.
pub async fn repos_with_stale_snapshots(pool: &PgPool) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT repository_id FROM code_chunks \
         GROUP BY repository_id HAVING count(DISTINCT commit_sha) > 1",
    )
    .fetch_all(pool)
    .await
}

/// Commits a still-running task pins, so the sweeper never prunes a snapshot out from under an
/// in-flight run. Non-terminal tasks that carry a `head_sha` (e.g. a review pinned to a PR head);
/// terminal statuses are excluded. `head_sha IS NOT NULL` since a null can't match a `commit_sha`.
/// NOTE: an `index` task carries a NULL `head_sha` (see [`create_index_task`]) so it is NOT covered
/// here — that case is handled by [`has_active_index_task`] (the sweeper skips a repo mid-index).
pub async fn in_use_commits(pool: &PgPool, repository_id: i64) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT DISTINCT head_sha FROM tasks \
         WHERE repository_id = $1 AND head_sha IS NOT NULL \
           AND status NOT IN ('succeeded', 'failed', 'timed_out', 'cancelled')",
    )
    .bind(repository_id)
    .fetch_all(pool)
    .await
}

/// Is an `index` task still in flight for this repo? An index task is the only thing that *writes* a
/// new snapshot, and it carries a NULL `head_sha` (see [`create_index_task`]) so [`in_use_commits`]
/// can't protect the commit it is mid-writing — and the Neo4j graph has NO recency grace (unlike
/// `code_chunks`). So the sweeper skips a repo entirely while an index runs and defers the prune one
/// cycle; deferring GC is harmless. Active = any non-terminal status.
pub async fn has_active_index_task(pool: &PgPool, repository_id: i64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS ( \
           SELECT 1 FROM tasks \
           WHERE repository_id = $1 AND command_text = 'index' \
             AND status NOT IN ('succeeded', 'failed', 'timed_out', 'cancelled'))",
    )
    .bind(repository_id)
    .fetch_one(pool)
    .await
}

/// Delete every `code_chunks` row for `repository_id` whose `commit_sha` is NOT in `keep`, except rows
/// indexed within the last 10 minutes (a recency grace: a just-finished index whose task hasn't yet
/// flipped to a terminal status, belt-and-suspenders on top of `in_use_commits`). Returns rows deleted.
/// No-op (returns 0) when `keep` is empty — never blindly delete a repo's whole index here.
pub async fn prune_code_chunks(
    pool: &PgPool,
    repository_id: i64,
    keep: &[String],
) -> Result<u64, sqlx::Error> {
    if keep.is_empty() {
        return Ok(0);
    }
    let result = sqlx::query(
        "DELETE FROM code_chunks \
         WHERE repository_id = $1 \
           AND commit_sha <> ALL($2) \
           AND created_at < now() - interval '10 minutes'",
    )
    .bind(repository_id)
    .bind(keep)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Disabled repositories that still have index data (leftover `code_chunks` or `repo_index` rows) —
/// the purge reconciler re-purges these so a cleanup lost to a control-plane restart still completes.
/// (Neo4j leftovers accompany `code_chunks`, so this also catches graph data to purge.)
pub async fn list_disabled_repos_needing_purge(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT r.id FROM repositories r \
         WHERE r.status = 'disabled' \
           AND (EXISTS (SELECT 1 FROM code_chunks c WHERE c.repository_id = r.id) \
                OR EXISTS (SELECT 1 FROM repo_index ri WHERE ri.repository_id = r.id)) \
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// A semantic chunk submitted by the indexer runner (epic #5, slice 2).
pub struct CodeChunk {
    pub file_path: String,
    pub language: String,
    pub chunk_type: String,
    pub symbol_name: Option<String>,
    pub start_line: i32,
    pub end_line: i32,
    pub content: String,
    /// Embedding vector. Its length must match the `code_chunks.embedding` column — 4096 for
    /// `qwen3-embedding-8b` (migration 0005), configurable per deployment (ADR-0018).
    pub embedding: Vec<f32>,
}

/// Upsert a batch of code chunks for a repository snapshot. The embedding is passed as a Postgres
/// vector literal so no extra crate is needed; `$N::vector` casts the text on the server side.
/// Runs in a single transaction; returns the number of rows inserted or updated.
pub async fn upsert_code_chunks(
    pool: &PgPool,
    repository_id: i64,
    commit_sha: &str,
    chunks: &[CodeChunk],
) -> anyhow::Result<usize> {
    use anyhow::Context;
    let mut tx = pool.begin().await.context("begin upsert transaction")?;
    let mut count = 0usize;
    for chunk in chunks {
        let emb = vector_literal(&chunk.embedding);
        sqlx::query(
            "INSERT INTO code_chunks \
             (repository_id, commit_sha, file_path, language, chunk_type, symbol_name, \
              start_line, end_line, content, embedding) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::vector) \
             ON CONFLICT (repository_id, commit_sha, file_path, start_line, end_line) \
             DO UPDATE SET \
               language    = EXCLUDED.language, \
               chunk_type  = EXCLUDED.chunk_type, \
               symbol_name = EXCLUDED.symbol_name, \
               content     = EXCLUDED.content, \
               embedding   = EXCLUDED.embedding",
        )
        .bind(repository_id)
        .bind(commit_sha)
        .bind(&chunk.file_path)
        .bind(&chunk.language)
        .bind(&chunk.chunk_type)
        .bind(&chunk.symbol_name)
        .bind(chunk.start_line)
        .bind(chunk.end_line)
        .bind(&chunk.content)
        .bind(&emb)
        .execute(&mut *tx)
        .await
        .context("upsert code_chunks row")?;
        count += 1;
    }
    tx.commit().await.context("commit upsert transaction")?;
    Ok(count)
}

/// Render a float slice as a pgvector text literal `[f0,f1,…]` in one pre-allocated buffer
/// (`$N::vector` casts it server-side, so no extra crate is needed).
fn vector_literal(v: &[f32]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(v.len() * 12 + 2);
    s.push('[');
    for (i, f) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "{f}");
    }
    s.push(']');
    s
}

/// One semantic-search hit (a `code_chunks` row + its similarity score). Serialized straight to the
/// retrieval API the vector MCP server calls.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct CodeChunkHit {
    pub file_path: String,
    pub language: String,
    pub chunk_type: String,
    pub symbol_name: Option<String>,
    pub start_line: i32,
    pub end_line: i32,
    pub content: String,
    /// Cosine similarity in `[0,1]` (`1 - cosine_distance`); higher is closer.
    pub score: f64,
}

/// Semantic search: the `limit` nearest chunks to `query_embedding` within one repo snapshot,
/// by cosine distance (an exact scan — the 4096-dim column exceeds pgvector's ANN limit, so
/// migration 0005 carries no index). Scoped by `(repository_id, commit_sha)` so
/// a task only ever sees its own repo's index — the caller never picks the scope (trust boundary).
pub async fn search_code_chunks(
    pool: &PgPool,
    repository_id: i64,
    commit_sha: &str,
    query_embedding: &[f32],
    limit: i64,
) -> Result<Vec<CodeChunkHit>, sqlx::Error> {
    let emb = vector_literal(query_embedding);
    sqlx::query_as::<_, CodeChunkHit>(
        "SELECT file_path, language, chunk_type, symbol_name, start_line, end_line, content, \
                1.0 - (embedding <=> $1::vector) AS score \
         FROM code_chunks \
         WHERE repository_id = $2 AND commit_sha = $3 \
         ORDER BY embedding <=> $1::vector \
         LIMIT $4",
    )
    .bind(&emb)
    .bind(repository_id)
    .bind(commit_sha)
    .bind(limit)
    .fetch_all(pool)
    .await
}
