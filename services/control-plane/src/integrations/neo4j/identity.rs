//! How `(repo_id, commit, node_id)` — the key every symbol read and write addresses a node by — is
//! indexed, and how snapshots holding more than one node per key are brought back to one.
//!
//! The preferred form is the `symbol_identity` uniqueness constraint: it backs a unique-index seek
//! and makes `MERGE` atomic. Neo4j refuses to create it while any key is already duplicated, and
//! refuses it while a plain index covers the same properties. So the key is always served by one of
//! two indexes — the constraint when the data allows it, the `symbol_identity_lookup` index when it
//! does not — and [`repair`] is what moves a database from the second to the first.

use neo4rs::{Graph, query};
use serde::Serialize;

use super::create_index_idempotent;

const CONSTRAINT: &str = "symbol_identity";
const LOOKUP_INDEX: &str = "symbol_identity_lookup";

/// Nodes processed per transaction while repairing, for the same reason deletes are batched: a
/// transaction's state stays on the heap until it commits.
const REPAIR_BATCH_ROWS: u32 = 1_000;

/// Which index currently serves the identity key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityIndex {
    /// `symbol_identity`: a unique-index seek, and `MERGE` cannot create a second node per key.
    Constraint,
    /// `symbol_identity_lookup`: the same seek without uniqueness, used while duplicates exist.
    LookupIndex,
}

/// Make sure the identity key is indexed, preferring the constraint.
///
/// Called at startup, so it never tears an existing index down: a lookup index already in place
/// means the constraint was refused before, and dropping it to retry on every restart would leave
/// the key unindexed while the replacement populates. Moving to the constraint is [`repair`]'s job,
/// once the data is known to allow it.
pub async fn declare(graph: &Graph) -> anyhow::Result<IdentityIndex> {
    if exists(graph, "CONSTRAINTS", CONSTRAINT).await? {
        return Ok(IdentityIndex::Constraint);
    }
    if exists(graph, "INDEXES", LOOKUP_INDEX).await? {
        return Ok(IdentityIndex::LookupIndex);
    }
    match create_constraint(graph).await {
        Ok(()) => Ok(IdentityIndex::Constraint),
        Err(refused) => {
            tracing::warn!(
                error = %format!("{refused:#}"),
                "symbol identity constraint refused; serving the key from a lookup index until the \
                 duplicates are repaired"
            );
            create_lookup_index(graph).await?;
            Ok(IdentityIndex::LookupIndex)
        }
    }
}

/// Replace the lookup index with the constraint, keeping the lookup index if the constraint is still
/// refused. The two cannot coexist, so the lookup index has to go first.
async fn promote(graph: &Graph) -> anyhow::Result<IdentityIndex> {
    use anyhow::Context;
    if exists(graph, "CONSTRAINTS", CONSTRAINT).await? {
        return Ok(IdentityIndex::Constraint);
    }
    graph
        .run(query(&format!("DROP INDEX {LOOKUP_INDEX} IF EXISTS")))
        .await
        .context("drop identity lookup index")?;
    match create_constraint(graph).await {
        Ok(()) => Ok(IdentityIndex::Constraint),
        Err(refused) => {
            tracing::warn!(
                error = %format!("{refused:#}"),
                "symbol identity constraint still refused after repair; restoring the lookup index"
            );
            create_lookup_index(graph).await?;
            Ok(IdentityIndex::LookupIndex)
        }
    }
}

async fn create_constraint(graph: &Graph) -> anyhow::Result<()> {
    create_index_idempotent(
        graph,
        "CREATE CONSTRAINT symbol_identity IF NOT EXISTS \
         FOR (s:Symbol) REQUIRE (s.repo_id, s.commit, s.node_id) IS UNIQUE",
        CONSTRAINT,
    )
    .await
}

async fn create_lookup_index(graph: &Graph) -> anyhow::Result<()> {
    create_index_idempotent(
        graph,
        "CREATE INDEX symbol_identity_lookup IF NOT EXISTS \
         FOR (s:Symbol) ON (s.repo_id, s.commit, s.node_id)",
        LOOKUP_INDEX,
    )
    .await
}

/// `kind` is `CONSTRAINTS` or `INDEXES` — a fixed keyword, never caller input.
async fn exists(graph: &Graph, kind: &str, name: &str) -> anyhow::Result<bool> {
    use anyhow::Context;
    let mut rows = graph
        .execute(
            query(&format!(
                "SHOW {kind} YIELD name WHERE name = $name RETURN count(*) AS n"
            ))
            .param("name", name),
        )
        .await
        .with_context(|| format!("show {kind}"))?;
    Ok(match rows.next().await.context("read schema row")? {
        Some(row) => row.get::<i64>("n").unwrap_or(0) > 0,
        None => false,
    })
}

/// One commit snapshot that holds some symbols more than once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DuplicatedSnapshot {
    pub repo_id: i64,
    pub commit: String,
    /// Keys with more than one node.
    pub duplicated_symbols: i64,
    /// Nodes beyond the first for each of those keys — what a repair removes.
    pub extra_nodes: i64,
}

/// What [`repair`] found, and — when applied — what it left serving the identity key.
#[derive(Debug, Clone, Serialize)]
pub struct RepairReport {
    pub applied: bool,
    pub snapshots: Vec<DuplicatedSnapshot>,
    pub extra_nodes: i64,
    pub identity: IdentityIndex,
}

/// Find every snapshot holding duplicated symbols and, when `apply` is set, collapse each to one
/// node per key and move the identity key onto the constraint.
///
/// Snapshots are examined one at a time rather than in one database-wide aggregation, so the memory
/// a pass needs is bounded by the largest snapshot, not by every symbol stored.
pub async fn repair(graph: &Graph, apply: bool) -> anyhow::Result<RepairReport> {
    let snapshots = duplicated_snapshots(graph).await?;
    let extra_nodes = snapshots.iter().map(|s| s.extra_nodes).sum();

    let identity = if apply {
        for snapshot in &snapshots {
            let removed = collapse_snapshot(graph, snapshot.repo_id, &snapshot.commit).await?;
            tracing::info!(
                repository_id = snapshot.repo_id,
                commit = %snapshot.commit,
                removed,
                "collapsed duplicated symbols"
            );
        }
        promote(graph).await?
    } else {
        declare(graph).await?
    };

    Ok(RepairReport {
        applied: apply,
        snapshots,
        extra_nodes,
        identity,
    })
}

async fn duplicated_snapshots(graph: &Graph) -> anyhow::Result<Vec<DuplicatedSnapshot>> {
    use anyhow::Context;
    let mut keys = Vec::new();
    let mut rows = graph
        .execute(query(
            "MATCH (s:Symbol) RETURN DISTINCT s.repo_id AS repo, s.commit AS commit",
        ))
        .await
        .context("list snapshots")?;
    while let Some(row) = rows.next().await.context("read snapshot")? {
        if let (Ok(repo), Ok(commit)) = (row.get::<i64>("repo"), row.get::<String>("commit")) {
            keys.push((repo, commit));
        }
    }

    let mut duplicated = Vec::new();
    for (repo, commit) in keys {
        let mut rows = graph
            .execute(
                query(
                    "MATCH (s:Symbol {repo_id: $repo, commit: $commit}) \
                     WITH s.node_id AS id, count(*) AS copies WHERE copies > 1 \
                     RETURN count(id) AS symbols, coalesce(sum(copies - 1), 0) AS extra",
                )
                .param("repo", repo)
                .param("commit", commit.as_str()),
            )
            .await
            .context("count duplicated symbols")?;
        let Some(row) = rows.next().await.context("read duplicate count")? else {
            continue;
        };
        let symbols = row.get::<i64>("symbols").unwrap_or(0);
        if symbols > 0 {
            duplicated.push(DuplicatedSnapshot {
                repo_id: repo,
                commit,
                duplicated_symbols: symbols,
                extra_nodes: row.get::<i64>("extra").unwrap_or(0),
            });
        }
    }
    Ok(duplicated)
}

/// Reduce one snapshot to a single node per key. Returns the nodes removed.
///
/// The survivor for each key is the copy with the lowest element id. Concurrent writers leave every
/// copy of an endpoint joined to every copy of the other, so each edge touching a surplus copy is
/// first re-created between the survivors — `MERGE` folds the fan-out back to one edge per relation
/// — and then the surplus copies are removed, handing any embedding they carry to the survivor.
async fn collapse_snapshot(graph: &Graph, repo: i64, commit: &str) -> anyhow::Result<u64> {
    use anyhow::Context;

    let mut rows = graph
        .execute(
            query(
                "MATCH (s:Symbol {repo_id: $repo, commit: $commit}) \
                 WITH s.node_id AS id, count(*) AS copies WHERE copies > 1 \
                 RETURN collect(id) AS ids",
            )
            .param("repo", repo)
            .param("commit", commit),
        )
        .await
        .context("list duplicated symbols")?;
    let ids: Vec<String> = match rows.next().await.context("read duplicated symbols")? {
        Some(row) => row.get("ids").unwrap_or_default(),
        None => Vec::new(),
    };
    if ids.is_empty() {
        return Ok(0);
    }

    graph
        .run(
            query(&format!(
                "UNWIND $ids AS id \
                 MATCH (:Symbol {{repo_id: $repo, commit: $commit, node_id: id}})-[r:REL]-() \
                 WITH DISTINCT r \
                 CALL (r) {{ \
                   WITH r, startNode(r) AS a, endNode(r) AS b \
                   MATCH (ka:Symbol {{repo_id: $repo, commit: $commit, node_id: a.node_id}}) \
                   WITH r, a, b, min(elementId(ka)) AS ka_id \
                   MATCH (kb:Symbol {{repo_id: $repo, commit: $commit, node_id: b.node_id}}) \
                   WITH r, a, b, ka_id, min(elementId(kb)) AS kb_id \
                   WHERE ka_id <> elementId(a) OR kb_id <> elementId(b) \
                   MATCH (ka:Symbol) WHERE elementId(ka) = ka_id \
                   MATCH (kb:Symbol) WHERE elementId(kb) = kb_id \
                   MERGE (ka)-[:REL {{relation: r.relation}}]->(kb) \
                   DELETE r \
                 }} IN TRANSACTIONS OF {REPAIR_BATCH_ROWS} ROWS"
            ))
            .param("repo", repo)
            .param("commit", commit)
            .param("ids", ids.clone()),
        )
        .await
        .context("move edges onto surviving symbols")?;

    let mut removed = 0u64;
    let mut rows = graph
        .execute(
            query(&format!(
                "UNWIND $ids AS id \
                 MATCH (s:Symbol {{repo_id: $repo, commit: $commit, node_id: id}}) \
                 WITH id, s ORDER BY elementId(s) \
                 WITH id, collect(s) AS copies \
                 WITH head(copies) AS keep, tail(copies) AS extras \
                 UNWIND extras AS extra \
                 CALL (keep, extra) {{ \
                   SET keep.embedding = coalesce(keep.embedding, extra.embedding) \
                   DETACH DELETE extra \
                 }} IN TRANSACTIONS OF {REPAIR_BATCH_ROWS} ROWS \
                 RETURN count(*) AS removed"
            ))
            .param("repo", repo)
            .param("commit", commit)
            .param("ids", ids),
        )
        .await
        .context("remove surplus symbols")?;
    if let Some(row) = rows.next().await.context("read removed count")? {
        removed = row.get::<i64>("removed").unwrap_or(0).max(0) as u64;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: i64 = 6641;

    async fn connect() -> Graph {
        let uri =
            std::env::var("NEO4J_URI").unwrap_or_else(|_| "bolt://localhost:7687".to_string());
        Graph::new(&uri, "neo4j", "lightbridge")
            .await
            .expect("connect neo4j")
    }

    async fn run(graph: &Graph, cypher: &str) {
        graph.run(query(cypher)).await.expect(cypher);
    }

    async fn single<T: serde::de::DeserializeOwned>(graph: &Graph, cypher: &str) -> T {
        let mut rows = graph.execute(query(cypher)).await.expect(cypher);
        let row = rows.next().await.expect("row").expect("one row");
        row.get::<T>("v").expect("v")
    }

    /// The state concurrent writers leave behind: `a` twice, `b` three times, `c` once; every copy of
    /// an endpoint joined to every copy of the other (edge `MATCH`es hit every copy), `b` calling
    /// itself, and — written before the other copy existed — an edge and the only embedding held by
    /// a copy of `a` that will not survive. A second snapshot of the same repository is clean and
    /// must not change.
    async fn seed(graph: &Graph) {
        run(
            graph,
            &format!("MATCH (s:Symbol {{repo_id: {REPO}}}) DETACH DELETE s"),
        )
        .await;
        run(
            graph,
            &format!(
                "UNWIND [['a', 2], ['b', 3], ['c', 1]] AS spec UNWIND range(1, spec[1]) AS n \
                 CREATE (:Symbol {{repo_id: {REPO}, commit: 'main', node_id: spec[0], \
                                   label: spec[0], source_file: 'f.rs', start_line: 1}})"
            ),
        )
        .await;
        run(
            graph,
            &format!(
                "MATCH (a:Symbol {{repo_id: {REPO}, commit: 'main', node_id: 'a'}}) \
                 WITH a ORDER BY elementId(a) DESC LIMIT 1 SET a.embedding = [0.5, 0.25] \
                 WITH a MATCH (c:Symbol {{repo_id: {REPO}, commit: 'main', node_id: 'c'}}) \
                 CREATE (a)-[:REL {{relation: 'contains'}}]->(c)"
            ),
        )
        .await;
        for (from, to) in [("a", "b"), ("b", "c"), ("b", "b")] {
            run(
                graph,
                &format!(
                    "MATCH (x:Symbol {{repo_id: {REPO}, commit: 'main', node_id: '{from}'}}), \
                           (y:Symbol {{repo_id: {REPO}, commit: 'main', node_id: '{to}'}}) \
                     CREATE (x)-[:REL {{relation: 'calls'}}]->(y)"
                ),
            )
            .await;
        }
        run(
            graph,
            &format!(
                "CREATE (:Symbol {{repo_id: {REPO}, commit: 'other', node_id: 'a', label: 'a', \
                                   source_file: 'f.rs', start_line: 1}}) \
                        -[:REL {{relation: 'calls'}}]-> \
                        (:Symbol {{repo_id: {REPO}, commit: 'other', node_id: 'b', label: 'b', \
                                   source_file: 'f.rs', start_line: 1}})"
            ),
        )
        .await;
    }

    async fn edges(graph: &Graph, commit: &str) -> Vec<String> {
        single(
            graph,
            &format!(
                "MATCH (a:Symbol {{repo_id: {REPO}, commit: '{commit}'}})-[r:REL]->(b) \
                 WITH a.node_id + ' -' + r.relation + '-> ' + b.node_id AS edge ORDER BY edge \
                 RETURN collect(edge) AS v"
            ),
        )
        .await
    }

    /// Live proof that a repair reduces a snapshot to one node per key without losing an edge or an
    /// embedding, touches no other snapshot, and leaves the key on the uniqueness constraint. A dry
    /// run reports the same snapshot and changes nothing; a second repair finds nothing to do.
    #[tokio::test]
    #[ignore = "requires a live Neo4j (docker compose up -d neo4j)"]
    async fn repair_collapses_duplicates_and_keeps_every_edge_and_embedding() {
        let graph = connect().await;
        let had_constraint = exists(&graph, "CONSTRAINTS", CONSTRAINT)
            .await
            .expect("show");
        run(&graph, "DROP CONSTRAINT symbol_identity IF EXISTS").await;
        run(&graph, "DROP INDEX symbol_identity_lookup IF EXISTS").await;
        seed(&graph).await;

        let dry = repair(&graph, false).await.expect("dry run");
        let found: Vec<_> = dry.snapshots.iter().filter(|s| s.repo_id == REPO).collect();
        assert_eq!(
            found,
            vec![&DuplicatedSnapshot {
                repo_id: REPO,
                commit: "main".to_string(),
                duplicated_symbols: 2,
                extra_nodes: 3,
            }],
            "only the duplicated snapshot is reported, with one surplus `a` and two surplus `b`"
        );
        assert_eq!(
            dry.identity,
            IdentityIndex::LookupIndex,
            "while duplicates exist the constraint is refused and the key is served by the lookup index"
        );
        assert_eq!(
            single::<i64>(
                &graph,
                &format!(
                    "MATCH (s:Symbol {{repo_id: {REPO}, commit: 'main'}}) RETURN count(s) AS v"
                )
            )
            .await,
            6,
            "a dry run changes nothing"
        );

        let applied = repair(&graph, true).await.expect("repair");
        assert!(applied.applied);
        assert_eq!(
            applied.identity,
            IdentityIndex::Constraint,
            "a clean database moves the key onto the constraint"
        );
        assert!(
            !exists(&graph, "INDEXES", LOOKUP_INDEX).await.expect("show"),
            "the lookup index is replaced, not kept alongside"
        );

        assert_eq!(
            single::<i64>(
                &graph,
                &format!(
                    "MATCH (s:Symbol {{repo_id: {REPO}, commit: 'main'}}) RETURN count(s) AS v"
                )
            )
            .await,
            3,
            "one node per key"
        );
        assert_eq!(
            edges(&graph, "main").await,
            vec![
                "a -calls-> b",
                "a -contains-> c",
                "b -calls-> b",
                "b -calls-> c"
            ],
            "every relation survives exactly once — including one only a surplus copy held, and \
             b's call to itself"
        );
        assert_eq!(
            single::<Vec<f64>>(
                &graph,
                &format!(
                    "MATCH (s:Symbol {{repo_id: {REPO}, commit: 'main', node_id: 'a'}}) \
                     RETURN s.embedding AS v"
                )
            )
            .await,
            vec![0.5, 0.25],
            "the embedding moves to the surviving copy"
        );
        assert_eq!(
            edges(&graph, "other").await,
            vec!["a -calls-> b"],
            "a clean snapshot of the same repository is untouched"
        );

        let again = repair(&graph, true).await.expect("repeat");
        assert!(
            again.snapshots.iter().all(|s| s.repo_id != REPO),
            "repeating a repair finds nothing left to do"
        );

        run(
            &graph,
            &format!("MATCH (s:Symbol {{repo_id: {REPO}}}) DETACH DELETE s"),
        )
        .await;
        if !had_constraint {
            run(&graph, "DROP CONSTRAINT symbol_identity IF EXISTS").await;
        }
    }
}
