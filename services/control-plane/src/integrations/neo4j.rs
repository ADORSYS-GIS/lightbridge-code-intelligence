//! Neo4j (Bolt) persistence for the structural code graph (ADR-0086; formerly ADR-0019).
//!
//! The agent runner builds the structural graph in-process with `lci-codegraph` (symbols +
//! `contains`/`method`/`calls` edges) and POSTs nodes+edges to the internal API; this module writes
//! them to Neo4j. The control plane owns the Neo4j credentials so the untrusted per-task Job never
//! holds them (trust boundary, ADR-0002) — the same reason chunk ingestion routes through the control
//! plane rather than direct DB access.

use neo4rs::{Graph, query};
use rmcp::schemars;
use serde::Serialize;

/// One graph node submitted by the runner (from `lci-codegraph`).
#[derive(Debug, Clone)]
pub struct GraphNode {
    pub node_id: String,
    pub label: String,
    pub source_file: String,
    /// 1-based start line (as emitted by `lci-codegraph`).
    pub start_line: i64,
    /// Embedding of the symbol's definition text (ADR-0114), when the runner found a correlated
    /// chunk to embed. `None` leaves any existing `s.embedding` untouched on a re-index.
    pub embedding: Option<Vec<f32>>,
}

/// One directed edge (`contains` / `method` / `calls` / …).
#[derive(Debug, Clone)]
pub struct GraphEdge {
    pub source: String,
    pub target: String,
    pub relation: String,
}

/// Connect to Neo4j from `NEO4J_URI` / `NEO4J_USER` / `NEO4J_PASSWORD`. Returns `Ok(None)` when
/// `NEO4J_URI` is unset (the graph endpoint then fails closed with 503, like the DB-less paths).
pub async fn connect_from_env() -> anyhow::Result<Option<Graph>> {
    use anyhow::Context;
    let uri = match std::env::var("NEO4J_URI") {
        Ok(uri) if !uri.is_empty() => uri,
        _ => return Ok(None),
    };
    let user = std::env::var("NEO4J_USER").unwrap_or_else(|_| "neo4j".to_string());
    let pass = std::env::var("NEO4J_PASSWORD").unwrap_or_default();
    let graph = Graph::new(&uri, &user, &pass)
        .await
        .context("connecting to Neo4j")?;
    tracing::info!(%uri, "neo4j connected");
    Ok(Some(graph))
}

/// Upsert a repository snapshot's graph. Nodes and edges are scoped by `(repository_id, commit_sha)`
/// so re-indexing the same commit is idempotent and different commits coexist. Runs in one
/// transaction; returns `(nodes_written, edges_written)`.
///
/// Nodes are a generic `:Symbol` and edges a generic `[:REL {relation}]` — Cypher can't parameterize
/// labels/relationship types, and a property keeps the write a single prepared statement. Per-row
/// MERGE in a transaction is correct and simple; batching via `UNWIND` is a later optimization.
pub async fn upsert_graph(
    graph: &Graph,
    repository_id: i64,
    commit_sha: &str,
    nodes: &[GraphNode],
    edges: &[GraphEdge],
) -> anyhow::Result<(usize, usize)> {
    use anyhow::Context;
    let mut txn = graph.start_txn().await.context("begin neo4j txn")?;

    for n in nodes {
        // `$embedding` is sent as an empty list when `n.embedding` is `None` — Neo4j has no first-class
        // null-vs-empty-list parameter distinction that's simpler to thread through here, and an empty
        // list is otherwise meaningless for a 4096-dim embedding, so it's a safe "no update" sentinel.
        // Without this guard a re-index that only recomputed structural facts (no embedder available)
        // would silently wipe every symbol's embedding.
        txn.run(
            query(
                "MERGE (s:Symbol {repo_id: $repo, commit: $commit, node_id: $id}) \
                 SET s.label = $label, s.source_file = $file, s.start_line = $line, \
                     s.embedding = CASE WHEN size($embedding) > 0 THEN $embedding ELSE s.embedding END",
            )
            .param("repo", repository_id)
            .param("commit", commit_sha)
            .param("id", n.node_id.as_str())
            .param("label", n.label.as_str())
            .param("file", n.source_file.as_str())
            .param("line", n.start_line)
            .param("embedding", n.embedding.clone().unwrap_or_default()),
        )
        .await
        .context("merge symbol node")?;
    }

    for e in edges {
        txn.run(
            query(
                "MATCH (a:Symbol {repo_id: $repo, commit: $commit, node_id: $src}) \
                 MATCH (b:Symbol {repo_id: $repo, commit: $commit, node_id: $dst}) \
                 MERGE (a)-[r:REL {relation: $rel}]->(b)",
            )
            .param("repo", repository_id)
            .param("commit", commit_sha)
            .param("src", e.source.as_str())
            .param("dst", e.target.as_str())
            .param("rel", e.relation.as_str()),
        )
        .await
        .context("merge edge")?;
    }

    txn.commit().await.context("commit neo4j txn")?;
    Ok((nodes.len(), edges.len()))
}

/// Delete **all** graph data for a repository (every commit snapshot), used when a repo is removed
/// from the installation or denied (Epic #75, Milestone B). Returns the number of nodes deleted.
/// `DETACH DELETE` removes the nodes' relationships too. Idempotent (deletes nothing for an
/// already-clean repo).
pub async fn delete_repo_graph(graph: &Graph, repository_id: i64) -> anyhow::Result<u64> {
    use anyhow::Context;
    // Count BEFORE deleting: a `RETURN count(s)` after `DETACH DELETE s` is unreliable (the nodes are
    // gone from the query context). Collect + count first, then delete via FOREACH.
    let mut rows = graph
        .execute(
            query(
                "MATCH (s:Symbol {repo_id: $repo}) \
                 WITH collect(s) AS nodes, count(s) AS deleted \
                 FOREACH (n IN nodes | DETACH DELETE n) \
                 RETURN deleted",
            )
            .param("repo", repository_id),
        )
        .await
        .context("delete repo graph")?;
    let deleted = match rows.next().await.context("read delete count")? {
        Some(row) => row.get::<i64>("deleted").unwrap_or(0).max(0) as u64,
        None => 0,
    };
    Ok(deleted)
}

/// Prune a repo's stale structural-graph snapshots: delete every `Symbol` for `repository_id` whose
/// `commit` is NOT in `keep` (RFC-0002 / ADR-0052 — the Neo4j half of index pruning). `keep` is the
/// in-use commit set (latest indexed + in-flight) computed by the sweeper. No-op when `keep` is empty
/// (mirrors [`crate::db::prune_code_chunks`] — never wipe the whole graph here). Returns nodes deleted.
pub async fn prune_graph(
    graph: &Graph,
    repository_id: i64,
    keep: &[String],
) -> anyhow::Result<u64> {
    use anyhow::Context;
    if keep.is_empty() {
        return Ok(0);
    }
    // Count BEFORE deleting (same reason as `delete_repo_graph`: the nodes leave the query context).
    let mut rows = graph
        .execute(
            query(
                "MATCH (s:Symbol {repo_id: $repo}) WHERE NOT s.commit IN $keep \
                 WITH collect(s) AS nodes, count(s) AS deleted \
                 FOREACH (n IN nodes | DETACH DELETE n) \
                 RETURN deleted",
            )
            .param("repo", repository_id)
            .param("keep", keep.to_vec()),
        )
        .await
        .context("prune repo graph")?;
    let deleted = match rows.next().await.context("read prune count")? {
        Some(row) => row.get::<i64>("deleted").unwrap_or(0).max(0) as u64,
        None => 0,
    };
    Ok(deleted)
}

/// A symbol returned by a graph query. Serialized straight to the retrieval API the graph MCP calls.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SymbolHit {
    pub node_id: String,
    pub label: String,
    pub source_file: String,
    pub start_line: i64,
}

/// Map a result row's `s.*` projection into a [`SymbolHit`].
fn symbol_from_row(row: &neo4rs::Row) -> Option<SymbolHit> {
    Some(SymbolHit {
        node_id: row.get("node_id").ok()?,
        label: row.get("label").unwrap_or_default(),
        source_file: row.get("source_file").unwrap_or_default(),
        start_line: row.get("start_line").unwrap_or(0),
    })
}

/// Find symbols by substring of name / id / file, within one repo snapshot. Scoped by
/// `(repository_id, commit_sha)` so a task only sees its own repo (trust boundary).
pub async fn find_symbol(
    graph: &Graph,
    repository_id: i64,
    commit_sha: &str,
    term: &str,
    limit: i64,
) -> anyhow::Result<Vec<SymbolHit>> {
    use anyhow::Context;
    let mut rows = graph
        .execute(
            query(
                "MATCH (s:Symbol {repo_id: $repo, commit: $commit}) \
                 WHERE toLower(s.label) CONTAINS toLower($term) \
                    OR toLower(s.node_id) CONTAINS toLower($term) \
                    OR toLower(s.source_file) CONTAINS toLower($term) \
                 RETURN s.node_id AS node_id, s.label AS label, s.source_file AS source_file, \
                        s.start_line AS start_line \
                 LIMIT $limit",
            )
            .param("repo", repository_id)
            .param("commit", commit_sha)
            .param("term", term)
            .param("limit", limit),
        )
        .await
        .context("find_symbol query")?;

    let mut hits = Vec::new();
    while let Some(row) = rows.next().await.context("find_symbol row")? {
        if let Some(hit) = symbol_from_row(&row) {
            hits.push(hit);
        }
    }
    Ok(hits)
}

/// Return the symbols that **call** `node_id` (reverse `calls` traversal), within one repo snapshot.
pub async fn get_callers(
    graph: &Graph,
    repository_id: i64,
    commit_sha: &str,
    node_id: &str,
    limit: i64,
) -> anyhow::Result<Vec<SymbolHit>> {
    use anyhow::Context;
    let mut rows = graph
        .execute(
            query(
                "MATCH (caller:Symbol {repo_id: $repo, commit: $commit}) \
                       -[:REL {relation: 'calls'}]-> \
                       (target:Symbol {repo_id: $repo, commit: $commit, node_id: $id}) \
                 RETURN caller.node_id AS node_id, caller.label AS label, \
                        caller.source_file AS source_file, caller.start_line AS start_line \
                 LIMIT $limit",
            )
            .param("repo", repository_id)
            .param("commit", commit_sha)
            .param("id", node_id)
            .param("limit", limit),
        )
        .await
        .context("get_callers query")?;

    let mut hits = Vec::new();
    while let Some(row) = rows.next().await.context("get_callers row")? {
        if let Some(hit) = symbol_from_row(&row) {
            hits.push(hit);
        }
    }
    Ok(hits)
}

/// Create the vector and fulltext indexes hybrid search needs, if they don't already exist
/// (ADR-0114). Both `CREATE ... INDEX ... IF NOT EXISTS` forms are idempotent — verified directly
/// against a real `neo4j:5.26-community` instance — so this is safe to call on every startup,
/// mirroring how Postgres migrations already run unconditionally on connect (`db::connect_from_env`).
///
/// `dimension` must match whatever the runner's `EmbeddingsClient` actually produces — 4096
/// (`qwen3-embedding-8b`, same as `code_chunks.embedding`/pgvector) is only the *default* when no
/// `embeddings.dimension` is configured. Neo4j's `CREATE VECTOR INDEX` has no parameter substitution
/// (schema DDL is never parameterized in Cypher), so the value is interpolated directly — safe here
/// since it comes from trusted server-side config, never from a request. A mismatch between this and
/// the embeddings actually written would otherwise fail every `MERGE ... SET s.embedding` silently at
/// the vector-index level and every `db.index.vector.queryNodes` call, exactly the failure mode
/// `db::reconcile_embedding_dimension` already guards against for the pgvector column.
pub async fn ensure_indexes(graph: &Graph, dimension: i64) -> anyhow::Result<()> {
    create_index_idempotent(
        graph,
        &format!(
            "CREATE VECTOR INDEX symbol_embedding_idx IF NOT EXISTS \
             FOR (s:Symbol) ON s.embedding \
             OPTIONS {{ indexConfig: {{ \
               `vector.dimensions`: {dimension}, \
               `vector.similarity_function`: 'cosine' \
             }}}}"
        ),
        "symbol_embedding_idx",
    )
    .await?;
    create_index_idempotent(
        graph,
        "CREATE FULLTEXT INDEX symbol_label_fulltext IF NOT EXISTS \
         FOR (s:Symbol) ON EACH [s.label, s.source_file]",
        "symbol_label_fulltext",
    )
    .await?;
    Ok(())
}

/// `CREATE ... IF NOT EXISTS` is not race-safe under concurrent callers: this project runs
/// `ensure_indexes` from every role that opens a Neo4j connection (control-plane and mcp both at
/// startup, confirmed live — two callers connected two seconds apart still raced), and Neo4j's
/// existence check + create isn't atomic across sessions. The loser of the race doesn't get a silent
/// no-op; it gets a hard `Neo.ClientError.Schema.EquivalentSchemaRuleAlreadyExists` ("An equivalent
/// index already exists"). The desired end state — the index exists — is reached either way, so that
/// specific outcome is a success here, not a failure; anything else still propagates.
async fn create_index_idempotent(graph: &Graph, ddl: &str, name: &str) -> anyhow::Result<()> {
    use anyhow::Context;
    match graph.run(query(ddl)).await {
        Ok(()) => Ok(()),
        Err(error) if error.to_string().contains("already exists") => {
            tracing::debug!(%error, name, "index already exists (lost a startup race, not a failure)");
            Ok(())
        }
        Err(error) => Err(error).with_context(|| format!("create {name}")),
    }
}

/// Hybrid symbol search: lexical (fulltext on `label`/`source_file`) + semantic (vector on
/// `embedding`), fused by weighted reciprocal rank (WRRF) — Neo4j's own documented hybrid-search
/// pattern, not an ad hoc union (ADR-0114). Scoped by `(repository_id, commit_sha)` like every other
/// query in this module. Takes an already-computed `query_embedding`; this function never embeds
/// anything itself, which is what lets both the MCP tool (runner-embedded) and a future frontend
/// endpoint (control-plane-embedded) share it without duplicating embedding logic here.
///
/// `source_k` bounds how many candidates each branch contributes before fusion; `final_k` bounds the
/// fused result. `RRF_CONSTANT = 60.0` is Neo4j's own documented default, dampening the effect of
/// rank position.
///
/// Version note: uses the `db.index.vector.queryNodes()` **procedure** form, confirmed working
/// against this project's real `neo4j:5.26-community` — not the newer `SEARCH ... IN (VECTOR
/// INDEX...)` clause syntax some current Neo4j docs lead with.
pub async fn hybrid_symbol_search(
    graph: &Graph,
    repository_id: i64,
    commit_sha: &str,
    query_text: &str,
    query_embedding: &[f32],
    source_k: i64,
    final_k: i64,
) -> anyhow::Result<Vec<SymbolHit>> {
    use anyhow::Context;
    const RRF_CONSTANT: f64 = 60.0;

    let mut rows = graph
        .execute(
            query(
                "CALL () { \
                   CALL db.index.fulltext.queryNodes('symbol_label_fulltext', $query, {limit: $sourceK}) \
                   YIELD node AS s \
                   WHERE s.repo_id = $repo AND s.commit = $commit \
                   WITH collect(s) AS hits \
                   UNWIND CASE WHEN size(hits) = 0 THEN [] ELSE range(0, size(hits) - 1) END AS rankIndex \
                   RETURN hits[rankIndex] AS s, rankIndex + 1 AS sourceRank \
                   UNION ALL \
                   CALL db.index.vector.queryNodes('symbol_embedding_idx', $sourceK, $queryVector) \
                   YIELD node AS s \
                   WHERE s.repo_id = $repo AND s.commit = $commit \
                   WITH collect(s) AS hits \
                   UNWIND CASE WHEN size(hits) = 0 THEN [] ELSE range(0, size(hits) - 1) END AS rankIndex \
                   RETURN hits[rankIndex] AS s, rankIndex + 1 AS sourceRank \
                 } \
                 WITH s, sum(1.0 / ($rrfConstant + sourceRank)) AS wrrf \
                 ORDER BY wrrf DESC \
                 LIMIT $finalK \
                 RETURN s.node_id AS node_id, s.label AS label, s.source_file AS source_file, \
                        s.start_line AS start_line",
            )
            .param("repo", repository_id)
            .param("commit", commit_sha)
            .param("query", escape_lucene_query(query_text))
            .param("queryVector", query_embedding.to_vec())
            .param("sourceK", source_k)
            .param("finalK", final_k)
            .param("rrfConstant", RRF_CONSTANT),
        )
        .await
        .context("hybrid_symbol_search query")?;

    let mut hits = Vec::new();
    while let Some(row) = rows.next().await.context("hybrid_symbol_search row")? {
        if let Some(hit) = symbol_from_row(&row) {
            hits.push(hit);
        }
    }
    Ok(hits)
}

/// Escape Lucene query-syntax metacharacters in free text before it's sent to
/// `db.index.fulltext.queryNodes`. The `term`/`query` argument to `lightbridge_graph_semantic_search`
/// is a model-authored natural-language string, not a hand-written Lucene query — an unescaped `(`,
/// `:`, unbalanced `"`, etc. throws a Lucene parse error that fails the *entire* Cypher call (not just
/// the fulltext branch), even though the vector branch alone would have been perfectly able to answer.
/// Escaping every occurrence of Lucene's reserved characters makes the whole string match as literal
/// terms, which is the right semantics here anyway — these are natural-language queries, not queries a
/// caller is deliberately writing Lucene boolean/proximity syntax into.
fn escape_lucene_query(text: &str) -> String {
    const SPECIAL: &[char] = &[
        '\\', '+', '-', '!', '(', ')', ':', '^', '[', ']', '"', '{', '}', '~', '*', '?', '|', '&',
        '/',
    ];
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if SPECIAL.contains(&c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_lucene_metacharacters_a_model_query_could_plausibly_contain() {
        assert_eq!(
            escape_lucene_query("retry logic (with backoff)"),
            r"retry logic \(with backoff\)"
        );
        assert_eq!(
            escape_lucene_query("how does auth: refresh work?"),
            r"how does auth\: refresh work\?"
        );
        assert_eq!(
            escape_lucene_query(r#"a "quoted" phrase"#),
            r#"a \"quoted\" phrase"#
        );
    }

    #[test]
    fn leaves_plain_natural_language_untouched() {
        assert_eq!(
            escape_lucene_query("retry a failed payment charge with backoff"),
            "retry a failed payment charge with backoff"
        );
    }

    /// Live round-trip against a real Neo4j (the compose service). Ignored by default — CI has no
    /// Neo4j — run with `cargo test -p control-plane --ignored` after `docker compose up -d neo4j`.
    /// Proves the `neo4rs` API usage + Cypher actually execute and round-trip, scoped by commit.
    #[tokio::test]
    #[ignore = "requires a live Neo4j (docker compose up -d neo4j)"]
    async fn upsert_graph_round_trips_against_live_neo4j() {
        let uri =
            std::env::var("NEO4J_URI").unwrap_or_else(|_| "bolt://localhost:7687".to_string());
        let graph = Graph::new(&uri, "neo4j", "lightbridge")
            .await
            .expect("connect neo4j");

        let commit = "test-commit-graph";
        // Clean any prior run for this commit so the test is repeatable.
        graph
            .run(query("MATCH (s:Symbol {commit: $c}) DETACH DELETE s").param("c", commit))
            .await
            .expect("cleanup");

        let nodes = vec![
            GraphNode {
                node_id: "src_math_add".into(),
                label: "add()".into(),
                source_file: "src/math.rs".into(),
                start_line: 2,
                embedding: None,
            },
            GraphNode {
                node_id: "src_math_calc_bump".into(),
                label: "bump()".into(),
                source_file: "src/math.rs".into(),
                start_line: 6,
                embedding: None,
            },
        ];
        let edges = vec![GraphEdge {
            source: "src_math_calc_bump".into(),
            target: "src_math_add".into(),
            relation: "calls".into(),
        }];

        let (n, e) = upsert_graph(&graph, 42, commit, &nodes, &edges)
            .await
            .expect("upsert");
        assert_eq!((n, e), (2, 1));

        // Read the call edge back, scoped to this commit.
        let mut rows = graph
            .execute(
                query(
                    "MATCH (a:Symbol {commit: $c})-[r:REL {relation: 'calls'}]->(b:Symbol) \
                     RETURN a.node_id AS src, b.node_id AS dst",
                )
                .param("c", commit),
            )
            .await
            .expect("query");
        let row = rows.next().await.expect("a row").expect("row present");
        assert_eq!(row.get::<String>("src").unwrap(), "src_math_calc_bump");
        assert_eq!(row.get::<String>("dst").unwrap(), "src_math_add");

        // Idempotent: re-upserting the same commit doesn't duplicate.
        upsert_graph(&graph, 42, commit, &nodes, &edges)
            .await
            .expect("re-upsert");
        let mut count = graph
            .execute(query("MATCH (s:Symbol {commit: $c}) RETURN count(s) AS n").param("c", commit))
            .await
            .expect("count query");
        let c = count.next().await.expect("row").expect("present");
        assert_eq!(c.get::<i64>("n").unwrap(), 2, "MERGE is idempotent");

        // find_symbol: case-insensitive substring match, scoped to (repo, commit).
        let found = find_symbol(&graph, 42, commit, "add", 10)
            .await
            .expect("find");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].node_id, "src_math_add");
        assert_eq!(found[0].start_line, 2);

        // get_callers: reverse `calls` traversal — bump() calls add().
        let callers = get_callers(&graph, 42, commit, "src_math_add", 10)
            .await
            .expect("callers");
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].node_id, "src_math_calc_bump");

        // Scope isolation: the same query under a different repo id returns nothing.
        assert!(
            find_symbol(&graph, 999, commit, "add", 10)
                .await
                .expect("find other repo")
                .is_empty()
        );

        // Cleanup.
        graph
            .run(query("MATCH (s:Symbol {commit: $c}) DETACH DELETE s").param("c", commit))
            .await
            .expect("final cleanup");
    }

    /// Live round-trip for `prune_graph` (ADR-0052): two snapshots of one repo, prune to a keep-set →
    /// the kept commit's nodes survive, the rest are deleted; an empty keep-set is a no-op. Ignored by
    /// default (no Neo4j in CI) — run with `--ignored` after `docker compose up -d neo4j`.
    #[tokio::test]
    #[ignore = "requires a live Neo4j (docker compose up -d neo4j)"]
    async fn prune_graph_keeps_only_the_keep_set() {
        let uri =
            std::env::var("NEO4J_URI").unwrap_or_else(|_| "bolt://localhost:7687".to_string());
        let graph = Graph::new(&uri, "neo4j", "lightbridge")
            .await
            .expect("connect neo4j");
        let repo = 4242i64;
        // Clean any prior run for this repo.
        delete_repo_graph(&graph, repo).await.expect("cleanup");

        let node = |id: &str| GraphNode {
            node_id: id.into(),
            label: format!("{id}()"),
            source_file: "src/x.rs".into(),
            start_line: 1,
            embedding: None,
        };
        let keep_sha = "graph-keep-sha";
        let stale_sha = "graph-stale-sha";
        upsert_graph(&graph, repo, keep_sha, &[node("keep_fn")], &[])
            .await
            .expect("upsert keep");
        upsert_graph(&graph, repo, stale_sha, &[node("stale_fn")], &[])
            .await
            .expect("upsert stale");

        // Prune everything but `keep_sha` → the one stale node goes.
        let deleted = prune_graph(&graph, repo, &[keep_sha.to_string()])
            .await
            .expect("prune");
        assert_eq!(deleted, 1, "only the stale snapshot's node is pruned");
        assert_eq!(
            find_symbol(&graph, repo, stale_sha, "stale", 10)
                .await
                .expect("find stale")
                .len(),
            0,
            "stale snapshot gone"
        );
        assert_eq!(
            find_symbol(&graph, repo, keep_sha, "keep", 10)
                .await
                .expect("find keep")
                .len(),
            1,
            "kept snapshot survives"
        );

        // Empty keep-set is a no-op (never wipe a live graph).
        assert_eq!(prune_graph(&graph, repo, &[]).await.expect("noop"), 0);

        delete_repo_graph(&graph, repo)
            .await
            .expect("final cleanup");
    }
}
