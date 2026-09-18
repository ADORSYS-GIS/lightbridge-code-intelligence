//! Neo4j (Bolt) persistence for the structural code graph (ADR-0086; formerly ADR-0019).
//!
//! The agent runner builds the structural graph in-process with `lci-codegraph` (symbols +
//! `contains`/`method`/`calls` edges) and POSTs nodes+edges to the internal API; this module writes
//! them to Neo4j. The control plane owns the Neo4j credentials so the untrusted per-task Job never
//! holds them (trust boundary, ADR-0002) — the same reason chunk ingestion routes through the control
//! plane rather than direct DB access.

use std::collections::HashMap;

use neo4rs::{BoltType, Graph, query};
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
    /// A symbol's vector arrives on the chunk that is its body (ADR-0116), so a current runner
    /// leaves this `None`. Kept because `None` means "leave any existing `s.embedding` alone", which
    /// is what a structure-only submit wants, and because a runner from before ADR-0116 still sends
    /// one.
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
/// labels/relationship types, and a property keeps the write a single prepared statement. Each side
/// is one `UNWIND`-driven statement over the whole batch, not one Bolt round trip per element, so the
/// write stays fast as the structural graph grows into the thousands of nodes/edges.
pub async fn upsert_graph(
    graph: &Graph,
    repository_id: i64,
    commit_sha: &str,
    nodes: &[GraphNode],
    edges: &[GraphEdge],
) -> anyhow::Result<(usize, usize)> {
    use anyhow::Context;
    let mut txn = graph.start_txn().await.context("begin neo4j txn")?;

    if !nodes.is_empty() {
        // Each row's `embedding` is an empty list when the node has none, which is otherwise
        // meaningless for an embedding vector — a safe "no update" sentinel so a re-index that only
        // recomputed structural facts doesn't wipe an existing embedding.
        let rows: Vec<HashMap<String, BoltType>> = nodes
            .iter()
            .map(|n| {
                HashMap::from([
                    ("id".to_string(), n.node_id.as_str().into()),
                    ("label".to_string(), n.label.as_str().into()),
                    ("file".to_string(), n.source_file.as_str().into()),
                    ("line".to_string(), n.start_line.into()),
                    (
                        "embedding".to_string(),
                        n.embedding.clone().unwrap_or_default().into(),
                    ),
                ])
            })
            .collect();
        txn.run(
            query(
                "UNWIND $nodes AS n \
                 MERGE (s:Symbol {repo_id: $repo, commit: $commit, node_id: n.id}) \
                 SET s.label = n.label, s.source_file = n.file, s.start_line = n.line, \
                     s.embedding = CASE WHEN size(n.embedding) > 0 THEN n.embedding ELSE s.embedding END",
            )
            .param("repo", repository_id)
            .param("commit", commit_sha)
            .param("nodes", rows),
        )
        .await
        .context("merge symbol nodes")?;
    }

    if !edges.is_empty() {
        let rows: Vec<HashMap<String, BoltType>> = edges
            .iter()
            .map(|e| {
                HashMap::from([
                    ("src".to_string(), e.source.as_str().into()),
                    ("dst".to_string(), e.target.as_str().into()),
                    ("rel".to_string(), e.relation.as_str().into()),
                ])
            })
            .collect();
        txn.run(
            query(
                "UNWIND $edges AS e \
                 MATCH (a:Symbol {repo_id: $repo, commit: $commit, node_id: e.src}) \
                 MATCH (b:Symbol {repo_id: $repo, commit: $commit, node_id: e.dst}) \
                 MERGE (a)-[r:REL {relation: e.rel}]->(b)",
            )
            .param("repo", repository_id)
            .param("commit", commit_sha)
            .param("edges", rows),
        )
        .await
        .context("merge edges")?;
    }

    txn.commit().await.context("commit neo4j txn")?;
    Ok((nodes.len(), edges.len()))
}

/// Attach chunk vectors to the symbols they are the body of (ADR-0116).
///
/// `rows` are `(node_id, embedding)` pairs taken from the chunks the runner just submitted, each
/// linked to its definition during the walk that produced both. `MATCH`, not `MERGE`: a vector whose
/// symbol is not in the graph — because the structural submit failed, or the snapshot predates this
/// commit — is dropped rather than creating a `:Symbol` carrying an embedding and nothing else.
/// Returns the number of symbols updated, which is at most `rows.len()`.
pub async fn attach_symbol_embeddings(
    graph: &Graph,
    repository_id: i64,
    commit_sha: &str,
    rows: &[(String, Vec<f32>)],
) -> anyhow::Result<u64> {
    use anyhow::Context;
    if rows.is_empty() {
        return Ok(0);
    }
    let params: Vec<HashMap<String, BoltType>> = rows
        .iter()
        .map(|(node_id, embedding)| {
            HashMap::from([
                ("id".to_string(), node_id.as_str().into()),
                ("embedding".to_string(), embedding.clone().into()),
            ])
        })
        .collect();
    let mut result = graph
        .execute(
            query(
                "UNWIND $rows AS r \
                 MATCH (s:Symbol {repo_id: $repo, commit: $commit, node_id: r.id}) \
                 SET s.embedding = r.embedding \
                 RETURN count(s) AS updated",
            )
            .param("repo", repository_id)
            .param("commit", commit_sha)
            .param("rows", params),
        )
        .await
        .context("attach symbol embeddings")?;

    let updated = match result.next().await.context("read attach result")? {
        Some(row) => row.get::<i64>("updated").unwrap_or(0),
        None => 0,
    };
    Ok(updated.max(0) as u64)
}

/// Nodes removed per transaction when deleting graph data.
///
/// Neo4j holds a transaction's whole state on the heap until it commits, so a delete sized by its
/// input grows with the snapshot it removes. Batching keeps each transaction to a fixed size however
/// large the repository is.
const DELETE_BATCH_ROWS: u32 = 1_000;

/// Delete every `:Symbol` the `selector` matches, with its relationships, in transactions of
/// [`DELETE_BATCH_ROWS`]. Returns how many nodes matched.
///
/// `selector` is a `MATCH … WHERE …` binding `s`. `CALL { … } IN TRANSACTIONS` commits each batch on
/// its own and only runs in an auto-commit transaction, which is what `Graph::run` issues. The count
/// is read first because deleted nodes leave the query context.
async fn delete_symbols_in_batches(
    graph: &Graph,
    selector: &str,
    params: &[(&str, BoltType)],
) -> anyhow::Result<u64> {
    use anyhow::Context;

    let mut count = query(&format!("{selector} RETURN count(s) AS n"));
    for (name, value) in params {
        count = count.param(name, value.clone());
    }
    let mut rows = graph
        .execute(count)
        .await
        .context("count symbols to delete")?;
    let matched = match rows.next().await.context("read delete count")? {
        Some(row) => row.get::<i64>("n").unwrap_or(0).max(0) as u64,
        None => 0,
    };
    if matched == 0 {
        return Ok(0);
    }

    let mut delete = query(&format!(
        "{selector} CALL (s) {{ DETACH DELETE s }} IN TRANSACTIONS OF {DELETE_BATCH_ROWS} ROWS"
    ));
    for (name, value) in params {
        delete = delete.param(name, value.clone());
    }
    graph.run(delete).await.context("delete symbols")?;
    Ok(matched)
}

/// Delete one commit snapshot's graph for a repository. Returns the number of nodes deleted.
///
/// A graph arrives as a sequence of pages, each its own transaction, so a sequence that stops partway
/// leaves the pages that already committed in place. Discarding the snapshot returns the commit to
/// "not indexed", which readers already handle, rather than leaving a subset that looks whole —
/// an absent edge is indistinguishable from a symbol that genuinely has no callers.
pub async fn delete_commit_graph(
    graph: &Graph,
    repository_id: i64,
    commit_sha: &str,
) -> anyhow::Result<u64> {
    use anyhow::Context;
    delete_symbols_in_batches(
        graph,
        "MATCH (s:Symbol {repo_id: $repo, commit: $commit})",
        &[
            ("repo", repository_id.into()),
            ("commit", commit_sha.into()),
        ],
    )
    .await
    .context("delete commit graph")
}

/// Delete **all** graph data for a repository (every commit snapshot), used when a repo is removed
/// from the installation or denied (Epic #75, Milestone B). Returns the number of nodes deleted.
/// Idempotent (deletes nothing for an already-clean repo).
pub async fn delete_repo_graph(graph: &Graph, repository_id: i64) -> anyhow::Result<u64> {
    use anyhow::Context;
    // `commit IS NOT NULL` holds for every symbol; stating it lets the `(repo_id, commit)` index
    // serve a match that names only the repository.
    delete_symbols_in_batches(
        graph,
        "MATCH (s:Symbol) WHERE s.repo_id = $repo AND s.commit IS NOT NULL",
        &[("repo", repository_id.into())],
    )
    .await
    .context("delete repo graph")
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
    delete_symbols_in_batches(
        graph,
        "MATCH (s:Symbol {repo_id: $repo}) WHERE NOT s.commit IN $keep",
        &[
            ("repo", repository_id.into()),
            ("keep", keep.to_vec().into()),
        ],
    )
    .await
    .context("prune repo graph")
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
/// (ADR-0114). Idempotent, so safe to call on every startup, mirroring how Postgres migrations
/// already run unconditionally on connect.
///
/// `dimension` should match the configured embeddings model's output size (4096 is the default when
/// `embeddings.dimension` is unset) — the same value `reconcile_embedding_dimension` uses for the
/// pgvector column. Cypher schema DDL can't be parameterized, so the value is interpolated directly,
/// which is safe here since it comes from trusted server-side config, never request input.
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

    // Snapshot-level reads and deletes — discarding a commit, pruning stale commits, removing a
    // repository — select by `(repo_id, commit)` and never name a node. The identity index below
    // needs all three properties, so without this one each of them scans the whole label.
    create_index_idempotent(
        graph,
        "CREATE INDEX symbol_snapshot IF NOT EXISTS FOR (s:Symbol) ON (s.repo_id, s.commit)",
        "symbol_snapshot",
    )
    .await?;

    // Every symbol read and write addresses a node by its full identity triple — the graph upsert's
    // MERGE, both endpoint MATCHes on each edge, `find_symbol`, `get_callers`, `symbol_embedding`,
    // `prune_graph`, and the chunk-side embedding attach. A composite index applies when a query
    // supplies all three with equality, which all of them do; without one each lookup scans every
    // `:Symbol` in the database, so one repository's write cost grows with every other repository
    // indexed.
    //
    // A uniqueness constraint rather than a bare index: it creates its own backing range index, it
    // lets MERGE plan a unique-index seek, and the triple genuinely is unique — a second node
    // sharing it would be a duplicate symbol.
    //
    // Creation is rejected outright if duplicate triples already exist, which `MERGE` on that same
    // key should never produce. That is reported and stepped over rather than propagated: the
    // indexes above are already in place by this point, and an absent identity index costs write
    // latency, not correctness.
    if let Err(error) = create_index_idempotent(
        graph,
        "CREATE CONSTRAINT symbol_identity IF NOT EXISTS \
         FOR (s:Symbol) REQUIRE (s.repo_id, s.commit, s.node_id) IS UNIQUE",
        "symbol_identity",
    )
    .await
    {
        tracing::warn!(
            ?error,
            "symbol identity constraint not created; symbol lookups will scan the label"
        );
    }
    Ok(())
}

/// `CREATE ... IF NOT EXISTS` is not atomic across concurrent callers — multiple roles can open a
/// Neo4j connection at startup, and the losing caller gets a hard
/// `Neo.ClientError.Schema.EquivalentSchemaRuleAlreadyExists` rather than a silent no-op. The desired
/// end state (the index exists) is reached either way, so that specific outcome is treated as success
/// here; anything else still propagates.
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
/// `embedding`), fused by weighted reciprocal rank (WRRF) — Neo4j's documented hybrid-search pattern,
/// not an ad hoc union (ADR-0114). Scoped by `(repository_id, commit_sha)` like every other query in
/// this module. Takes an already-computed `query_embedding`; never embeds anything itself, so any
/// caller with its own embedder can reuse it.
///
/// `source_k` bounds how many candidates each branch contributes before fusion; `final_k` bounds the
/// fused result. `RRF_CONSTANT = 60.0` is Neo4j's documented default.
///
/// Uses the `db.index.vector.queryNodes()` procedure form rather than the newer
/// `SEARCH ... IN (VECTOR INDEX...)` clause syntax.
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
/// `db.index.fulltext.queryNodes`. The query is a natural-language string, not a hand-written Lucene
/// query — an unescaped `(`, `:`, or unbalanced `"` throws a Lucene parse error that fails the entire
/// call, not just the fulltext branch. Escaping treats the whole string as literal terms, which is the
/// right semantics for free text.
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

/// One directed edge between two symbols already known to a caller, as returned to an admin API
/// consumer (the frontend code graph view). Mirrors [`SymbolHit`]'s role for nodes.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RelHit {
    pub source: String,
    pub target: String,
    pub relation: String,
}

/// A symbol's own stored embedding, if it has one (ADR-0114's coverage is not 100% — a symbol with
/// no correlated chunk at index time has none). Used to power "find similar to this" without ever
/// embedding new text: the query vector is a value already sitting in the graph, not something
/// computed at request time, so the same node always produces the same search.
pub async fn symbol_embedding(
    graph: &Graph,
    repository_id: i64,
    commit_sha: &str,
    node_id: &str,
) -> anyhow::Result<Option<(String, Vec<f32>)>> {
    use anyhow::Context;
    let mut rows = graph
        .execute(
            query(
                "MATCH (s:Symbol {repo_id: $repo, commit: $commit, node_id: $node}) \
                 WHERE s.embedding IS NOT NULL \
                 RETURN s.label AS label, s.embedding AS embedding",
            )
            .param("repo", repository_id)
            .param("commit", commit_sha)
            .param("node", node_id),
        )
        .await
        .context("symbol_embedding query")?;
    match rows.next().await.context("symbol_embedding row")? {
        Some(row) => {
            let label: String = row.get("label").unwrap_or_default();
            let embedding: Vec<f32> = row.get("embedding").unwrap_or_default();
            Ok(Some((label, embedding)))
        }
        None => Ok(None),
    }
}

/// A symbol's immediate structural neighborhood: itself, up to `hops` steps out along `REL` edges
/// (either direction), and the edges among the resulting node set. Two queries rather than one —
/// Neo4j's `*min..max` relationship range can't be parameterized, so `hops` (clamped to 1..=3 by the
/// caller) is interpolated directly into the first query; it's a small, server-controlled integer,
/// never request text. The second query finds only the edges *induced* by the first query's node set,
/// so the result is a well-formed subgraph, not a node list with edges pointing outside it.
pub async fn graph_neighborhood(
    graph: &Graph,
    repository_id: i64,
    commit_sha: &str,
    node_id: &str,
    hops: i64,
    limit: i64,
) -> anyhow::Result<(Vec<SymbolHit>, Vec<RelHit>)> {
    use anyhow::Context;
    let hops = hops.clamp(1, 3);

    let mut node_rows = graph
        .execute(
            query(&format!(
                "MATCH (seed:Symbol {{repo_id: $repo, commit: $commit, node_id: $node}}) \
                 OPTIONAL MATCH (seed)-[:REL*1..{hops}]-(other:Symbol {{repo_id: $repo, commit: $commit}}) \
                 WITH seed, collect(DISTINCT other)[0..$limit] AS others \
                 UNWIND [seed] + others AS n \
                 RETURN DISTINCT n.node_id AS node_id, n.label AS label, n.source_file AS source_file, \
                        n.start_line AS start_line"
            ))
            .param("repo", repository_id)
            .param("commit", commit_sha)
            .param("node", node_id)
            .param("limit", limit),
        )
        .await
        .context("graph_neighborhood nodes query")?;

    let mut nodes = Vec::new();
    while let Some(row) = node_rows
        .next()
        .await
        .context("graph_neighborhood node row")?
    {
        if let Some(hit) = symbol_from_row(&row) {
            nodes.push(hit);
        }
    }
    let edges = edges_among(graph, repository_id, commit_sha, &nodes).await?;
    Ok((nodes, edges))
}

/// An unseeded slice of a repository's graph — the `limit` most-connected symbols by structural
/// degree, plus the edges among them. Used when the frontend has no node selected yet (first load of
/// the graph tab), so there's always something to render instead of an empty canvas. Ranking by
/// degree rather than file order means the first thing shown is the repo's most-referenced symbols,
/// not whatever happens to sort first alphabetically.
pub async fn graph_overview(
    graph: &Graph,
    repository_id: i64,
    commit_sha: &str,
    limit: i64,
) -> anyhow::Result<(Vec<SymbolHit>, Vec<RelHit>)> {
    use anyhow::Context;
    let mut rows = graph
        .execute(
            query(
                "MATCH (s:Symbol {repo_id: $repo, commit: $commit}) \
                 OPTIONAL MATCH (s)-[r:REL]-() \
                 WITH s, count(r) AS degree \
                 ORDER BY degree DESC, s.source_file, s.start_line \
                 LIMIT $limit \
                 RETURN s.node_id AS node_id, s.label AS label, s.source_file AS source_file, \
                        s.start_line AS start_line",
            )
            .param("repo", repository_id)
            .param("commit", commit_sha)
            .param("limit", limit),
        )
        .await
        .context("graph_overview query")?;

    let mut nodes = Vec::new();
    while let Some(row) = rows.next().await.context("graph_overview row")? {
        if let Some(hit) = symbol_from_row(&row) {
            nodes.push(hit);
        }
    }
    let edges = edges_among(graph, repository_id, commit_sha, &nodes).await?;
    Ok((nodes, edges))
}

/// The `REL` edges whose both endpoints are in `nodes` — the induced subgraph on an already-known
/// node set. Shared by [`graph_neighborhood`], [`graph_overview`], and semantic-search results, so a
/// search result set that happens to be structurally connected shows that connection instead of
/// rendering as isolated dots.
pub async fn edges_among(
    graph: &Graph,
    repository_id: i64,
    commit_sha: &str,
    nodes: &[SymbolHit],
) -> anyhow::Result<Vec<RelHit>> {
    use anyhow::Context;
    if nodes.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<&str> = nodes.iter().map(|n| n.node_id.as_str()).collect();
    let mut rows = graph
        .execute(
            query(
                "MATCH (a:Symbol {repo_id: $repo, commit: $commit})-[r:REL]->(b:Symbol {repo_id: $repo, commit: $commit}) \
                 WHERE a.node_id IN $ids AND b.node_id IN $ids \
                 RETURN a.node_id AS source, b.node_id AS target, r.relation AS relation",
            )
            .param("repo", repository_id)
            .param("commit", commit_sha)
            .param("ids", ids),
        )
        .await
        .context("edges_among query")?;

    let mut edges = Vec::new();
    while let Some(row) = rows.next().await.context("edges_among row")? {
        edges.push(RelHit {
            source: row.get("source").unwrap_or_default(),
            target: row.get("target").unwrap_or_default(),
            relation: row.get("relation").unwrap_or_default(),
        });
    }
    Ok(edges)
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

    /// Live proof that discarding a snapshot clears exactly that commit. The state this exists for
    /// is a page sequence that stopped partway: the pages that committed must not survive as a graph
    /// that reads as complete. Ignored by default — run with `--ignored` after
    /// `docker compose up -d neo4j`.
    /// Live proof that deletes larger than one batch remove exactly their selection. Each path —
    /// discarding a commit, pruning stale commits, removing a repository — spans several
    /// `DELETE_BATCH_ROWS` transactions here, which is the shape that must not grow with the snapshot.
    #[tokio::test]
    #[ignore = "requires a live Neo4j (docker compose up -d neo4j)"]
    async fn deletes_spanning_several_batches_remove_exactly_their_selection() {
        let uri =
            std::env::var("NEO4J_URI").unwrap_or_else(|_| "bolt://localhost:7687".to_string());
        let graph = Graph::new(&uri, "neo4j", "lightbridge")
            .await
            .expect("connect neo4j");

        let (repo, other_repo) = (6621i64, 6622i64);
        let per_commit = DELETE_BATCH_ROWS as i64 * 2 + 500;
        let nodes: Vec<GraphNode> = (0..per_commit)
            .map(|i| GraphNode {
                node_id: format!("src/f{i}.rs#1:f{i}"),
                label: format!("f{i}()"),
                source_file: format!("src/f{i}.rs"),
                start_line: 1,
                embedding: None,
            })
            .collect();
        let edges: Vec<GraphEdge> = (1..per_commit)
            .map(|i| GraphEdge {
                source: format!("src/f{i}.rs#1:f{i}"),
                target: "src/f0.rs#1:f0".to_string(),
                relation: "calls".to_string(),
            })
            .collect();
        for r in [repo, other_repo] {
            delete_repo_graph(&graph, r).await.expect("cleanup");
        }
        for commit in ["discard", "stale", "current"] {
            upsert_graph(&graph, repo, commit, &nodes, &edges)
                .await
                .expect("seed");
        }
        upsert_graph(&graph, other_repo, "current", &nodes, &[])
            .await
            .expect("seed other repo");

        let expected = per_commit as u64;
        assert_eq!(
            delete_commit_graph(&graph, repo, "discard")
                .await
                .expect("discard"),
            expected,
            "a discard reports every node of its snapshot"
        );
        assert_eq!(
            prune_graph(&graph, repo, &["current".to_string()])
                .await
                .expect("prune"),
            expected,
            "pruning removes the one stale commit left"
        );
        assert_eq!(
            count_snapshot(&graph, repo, "current").await,
            per_commit,
            "the kept commit is intact"
        );

        assert_eq!(
            delete_repo_graph(&graph, repo).await.expect("repo delete"),
            expected,
            "removing the repository takes its last snapshot"
        );
        assert_eq!(count_snapshot(&graph, repo, "current").await, 0);
        assert_eq!(
            count_snapshot(&graph, other_repo, "current").await,
            per_commit,
            "another repository is untouched"
        );

        delete_repo_graph(&graph, other_repo)
            .await
            .expect("final cleanup");
    }

    async fn count_snapshot(graph: &Graph, repo: i64, commit: &str) -> i64 {
        let mut rows = graph
            .execute(
                query("MATCH (s:Symbol {repo_id: $repo, commit: $commit}) RETURN count(s) AS n")
                    .param("repo", repo)
                    .param("commit", commit),
            )
            .await
            .expect("count");
        rows.next()
            .await
            .expect("row")
            .expect("one row")
            .get::<i64>("n")
            .expect("n")
    }

    #[tokio::test]
    #[ignore = "requires a live Neo4j (docker compose up -d neo4j)"]
    async fn discarding_a_snapshot_clears_that_commit_and_leaves_others() {
        let uri =
            std::env::var("NEO4J_URI").unwrap_or_else(|_| "bolt://localhost:7687".to_string());
        let graph = Graph::new(&uri, "neo4j", "lightbridge")
            .await
            .expect("connect neo4j");

        let repo = 6571i64;
        delete_repo_graph(&graph, repo).await.expect("cleanup");

        let nodes: Vec<GraphNode> = (0..6)
            .map(|i| GraphNode {
                node_id: format!("src/a.rs#{i}:f{i}"),
                label: format!("f{i}()"),
                source_file: "src/a.rs".into(),
                start_line: i,
                embedding: None,
            })
            .collect();
        let edges = vec![GraphEdge {
            source: "src/a.rs#0:f0".into(),
            target: "src/a.rs#1:f1".into(),
            relation: "calls".into(),
        }];

        // A sequence that stopped partway: nodes landed, edges did not.
        upsert_graph(&graph, repo, "partial", &nodes, &[])
            .await
            .expect("node pages");
        // An unrelated snapshot of the same repository, which must survive.
        upsert_graph(&graph, repo, "keep", &nodes, &edges)
            .await
            .expect("other snapshot");

        let deleted = delete_commit_graph(&graph, repo, "partial")
            .await
            .expect("discard");
        assert_eq!(deleted, 6, "every node of that snapshot");

        assert!(
            find_symbol(&graph, repo, "partial", "f0", 10)
                .await
                .expect("find")
                .is_empty(),
            "the discarded snapshot reads as absent, not as a partial graph"
        );
        assert_eq!(
            find_symbol(&graph, repo, "keep", "f0", 10)
                .await
                .expect("find")
                .len(),
            1,
            "a different commit of the same repository is untouched"
        );

        delete_repo_graph(&graph, repo)
            .await
            .expect("final cleanup");
    }

    /// Live proof that a graph written as pages matches one written whole (ADR-0116 / #656). The
    /// failure this guards is silent: an edge is written by matching both endpoints, so an edge page
    /// that lands before its nodes writes nothing and reports success. Ignored by default (no Neo4j
    /// in CI) — run with `--ignored` after `docker compose up -d neo4j`.
    #[tokio::test]
    #[ignore = "requires a live Neo4j (docker compose up -d neo4j)"]
    async fn a_paged_graph_write_matches_an_unpaged_one() {
        let uri =
            std::env::var("NEO4J_URI").unwrap_or_else(|_| "bolt://localhost:7687".to_string());
        let graph = Graph::new(&uri, "neo4j", "lightbridge")
            .await
            .expect("connect neo4j");

        let repo = 6561i64;
        delete_repo_graph(&graph, repo).await.expect("cleanup");

        let nodes: Vec<GraphNode> = (0..25)
            .map(|i| GraphNode {
                node_id: format!("src/a.rs#{i}:f{i}"),
                label: format!("f{i}()"),
                source_file: "src/a.rs".into(),
                start_line: i,
                embedding: None,
            })
            .collect();
        let edges: Vec<GraphEdge> = (0..24)
            .map(|i| GraphEdge {
                source: format!("src/a.rs#{i}:f{i}"),
                target: format!("src/a.rs#{}:f{}", i + 1, i + 1),
                relation: "calls".into(),
            })
            .collect();

        let counts = |commit: &'static str| {
            let graph = graph.clone();
            async move {
                let mut rows = graph
                    .execute(
                        query(
                            "MATCH (s:Symbol {repo_id: $r, commit: $c}) \
                             OPTIONAL MATCH (s)-[e:REL]->() \
                             RETURN count(DISTINCT s) AS nodes, count(e) AS edges",
                        )
                        .param("r", repo)
                        .param("c", commit),
                    )
                    .await
                    .expect("count query");
                let row = rows.next().await.expect("row").expect("present");
                (
                    row.get::<i64>("nodes").unwrap(),
                    row.get::<i64>("edges").unwrap(),
                )
            }
        };

        upsert_graph(&graph, repo, "whole", &nodes, &edges)
            .await
            .expect("unpaged upsert");
        let whole = counts("whole").await;
        assert_eq!(
            whole,
            (25, 24),
            "baseline: one submit writes the full graph"
        );

        // Same graph, delivered the way `submit_graph_paged` delivers it: every node page first.
        for page in nodes.chunks(7) {
            upsert_graph(&graph, repo, "paged", page, &[])
                .await
                .expect("node page");
        }
        for page in edges.chunks(7) {
            upsert_graph(&graph, repo, "paged", &[], page)
                .await
                .expect("edge page");
        }
        assert_eq!(
            counts("paged").await,
            whole,
            "a paged write must produce the same graph as an unpaged one"
        );

        delete_repo_graph(&graph, repo)
            .await
            .expect("final cleanup");
    }

    /// Live round-trip for `attach_symbol_embeddings` (ADR-0116): a chunk's vector reaches the
    /// `:Symbol` it is the body of, a vector for an unknown symbol is dropped rather than creating
    /// one, and a later structure-only `upsert_graph` leaves an attached vector intact. Ignored by
    /// default (no Neo4j in CI) — run with `--ignored` after `docker compose up -d neo4j`.
    #[tokio::test]
    #[ignore = "requires a live Neo4j (docker compose up -d neo4j)"]
    async fn attach_symbol_embeddings_writes_to_matching_symbols_only() {
        let uri =
            std::env::var("NEO4J_URI").unwrap_or_else(|_| "bolt://localhost:7687".to_string());
        let graph = Graph::new(&uri, "neo4j", "lightbridge")
            .await
            .expect("connect neo4j");

        let repo = 7117i64;
        let commit = "test-commit-attach";
        delete_repo_graph(&graph, repo).await.expect("cleanup");

        // Structure lands first, carrying no vectors — what a current runner submits.
        let nodes = vec![
            GraphNode {
                node_id: "src/auth.rs#40:validate".into(),
                label: "validate()".into(),
                source_file: "src/auth.rs".into(),
                start_line: 40,
                embedding: None,
            },
            GraphNode {
                node_id: "src/auth.rs#60:refresh".into(),
                label: "refresh()".into(),
                source_file: "src/auth.rs".into(),
                start_line: 60,
                embedding: None,
            },
        ];
        upsert_graph(&graph, repo, commit, &nodes, &[])
            .await
            .expect("upsert structure");
        assert_eq!(
            symbol_embedding(&graph, repo, commit, "src/auth.rs#40:validate")
                .await
                .expect("read"),
            None,
            "a structure-only submit leaves the symbol without a vector"
        );

        // One known symbol, one that is not in the graph.
        let rows = vec![
            ("src/auth.rs#40:validate".to_string(), vec![0.25f32; 8]),
            ("src/auth.rs#99:ghost".to_string(), vec![0.75f32; 8]),
        ];
        let updated = attach_symbol_embeddings(&graph, repo, commit, &rows)
            .await
            .expect("attach");
        assert_eq!(updated, 1, "only the symbol that exists is updated");

        let (label, stored) = symbol_embedding(&graph, repo, commit, "src/auth.rs#40:validate")
            .await
            .expect("read back")
            .expect("a vector");
        assert_eq!(label, "validate()");
        assert_eq!(stored.len(), 8);
        assert!((stored[0] - 0.25).abs() < 1e-6, "the chunk's own vector");

        // MATCH, not MERGE: the unknown node_id created nothing.
        let mut count = graph
            .execute(
                query("MATCH (s:Symbol {repo_id: $r, commit: $c}) RETURN count(s) AS n")
                    .param("r", repo)
                    .param("c", commit),
            )
            .await
            .expect("count");
        let row = count.next().await.expect("row").expect("present");
        assert_eq!(
            row.get::<i64>("n").unwrap(),
            2,
            "a vector for an absent symbol must not create one"
        );

        // A re-index that only recomputes structure must not wipe the attached vector.
        upsert_graph(&graph, repo, commit, &nodes, &[])
            .await
            .expect("re-upsert structure");
        assert!(
            symbol_embedding(&graph, repo, commit, "src/auth.rs#40:validate")
                .await
                .expect("read after re-upsert")
                .is_some(),
            "structure-only re-upsert preserves the embedding"
        );

        delete_repo_graph(&graph, repo)
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
