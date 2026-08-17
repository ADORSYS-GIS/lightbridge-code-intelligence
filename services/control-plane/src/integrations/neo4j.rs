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
        txn.run(
            query(
                "MERGE (s:Symbol {repo_id: $repo, commit: $commit, node_id: $id}) \
                 SET s.label = $label, s.source_file = $file, s.start_line = $line",
            )
            .param("repo", repository_id)
            .param("commit", commit_sha)
            .param("id", n.node_id.as_str())
            .param("label", n.label.as_str())
            .param("file", n.source_file.as_str())
            .param("line", n.start_line),
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

/// One edge returned by a graph-view query (ADR-0113 / #615).
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RelHit {
    pub source: String,
    pub target: String,
    pub relation: String,
}

/// Any one symbol's `node_id` for a repo snapshot, used as the default seed when the caller doesn't
/// pick one (ADR-0113 / #615 — symbol picking is a later UI decision, not blocking a first render).
/// Deterministic (`ORDER BY node_id`), not "most interesting" — good enough to prove the view works.
pub async fn any_symbol_id(
    graph: &Graph,
    repository_id: i64,
    commit_sha: &str,
) -> anyhow::Result<Option<String>> {
    use anyhow::Context;
    let mut rows = graph
        .execute(
            query(
                "MATCH (s:Symbol {repo_id: $repo, commit: $commit}) \
                 RETURN s.node_id AS node_id ORDER BY s.node_id LIMIT 1",
            )
            .param("repo", repository_id)
            .param("commit", commit_sha),
        )
        .await
        .context("any_symbol_id query")?;
    match rows.next().await.context("any_symbol_id row")? {
        Some(row) => Ok(row.get("node_id").ok()),
        None => Ok(None),
    }
}

/// Neighborhood around `seed_id`, up to `hops` `REL` traversals in either direction, within one repo
/// snapshot (ADR-0113 / #615 — the reduction strategy for a repo too large to render whole; #515's
/// spike measured real repos at a p90 of ~9,126 nodes, far past what a raw full-graph render can
/// carry). Bounded by `limit` on the edge count, not the node count — the caller (the HTTP handler)
/// picks the ceiling.
///
/// `hops` is interpolated into the query text rather than bound as a parameter: Cypher's
/// variable-length path syntax (`*1..N`) takes a literal bound, not a parameter, in stock Neo4j.
/// Safe here because `hops` is clamped by the caller to a small integer range before this is ever
/// called — never raw user input reaching the query string.
pub async fn graph_neighborhood(
    graph: &Graph,
    repository_id: i64,
    commit_sha: &str,
    seed_id: &str,
    hops: i64,
    limit: i64,
) -> anyhow::Result<(Vec<SymbolHit>, Vec<RelHit>)> {
    use anyhow::Context;
    let cypher = format!(
        "MATCH (seed:Symbol {{repo_id: $repo, commit: $commit, node_id: $id}}) \
         MATCH path = (seed)-[:REL*1..{hops}]-(neighbor:Symbol {{repo_id: $repo, commit: $commit}}) \
         UNWIND relationships(path) AS r \
         WITH DISTINCT startNode(r) AS a, endNode(r) AS b, r.relation AS relation \
         RETURN a.node_id AS src_id, a.label AS src_label, a.source_file AS src_file, a.start_line AS src_line, \
                b.node_id AS dst_id, b.label AS dst_label, b.source_file AS dst_file, b.start_line AS dst_line, \
                relation \
         LIMIT $limit"
    );
    let mut rows = graph
        .execute(
            query(&cypher)
                .param("repo", repository_id)
                .param("commit", commit_sha)
                .param("id", seed_id)
                .param("limit", limit),
        )
        .await
        .context("graph_neighborhood query")?;

    let mut nodes: std::collections::HashMap<String, SymbolHit> = std::collections::HashMap::new();
    let mut edges = Vec::new();
    while let Some(row) = rows.next().await.context("graph_neighborhood row")? {
        for (id_col, label_col, file_col, line_col) in [
            ("src_id", "src_label", "src_file", "src_line"),
            ("dst_id", "dst_label", "dst_file", "dst_line"),
        ] {
            let node_id: String = row.get(id_col).context("graph_neighborhood node id")?;
            nodes.entry(node_id.clone()).or_insert_with(|| SymbolHit {
                node_id,
                label: row.get(label_col).unwrap_or_default(),
                source_file: row.get(file_col).unwrap_or_default(),
                start_line: row.get(line_col).unwrap_or(0),
            });
        }
        edges.push(RelHit {
            source: row
                .get("src_id")
                .context("graph_neighborhood edge source")?,
            target: row
                .get("dst_id")
                .context("graph_neighborhood edge target")?,
            relation: row.get("relation").unwrap_or_default(),
        });
    }
    Ok((nodes.into_values().collect(), edges))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            },
            GraphNode {
                node_id: "src_math_calc_bump".into(),
                label: "bump()".into(),
                source_file: "src/math.rs".into(),
                start_line: 6,
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
