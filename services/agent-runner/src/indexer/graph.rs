//! Structural graph via the in-house `lci-codegraph` crate (ADR-0086) — the sole graph engine.
//!
//! The runner extracts the structural code graph **in-process** with `lci-codegraph` (tree-sitter,
//! no subprocess): it emits `GraphNodePayload`/`GraphEdgePayload` and hands them to the control
//! plane, which owns the Neo4j write. This replaced the Python **Graphify** CLI (ADR-0019), which is
//! gone; there is no fallback and no flag. Languages without a graph extractor yet simply produce no
//! structural facts (the semantic pgvector index still covers them, since the same walk chunks them).

use anyhow::Context;

use lci_agent_clients::{
    ControlPlaneClient, DEFAULT_GRAPH_PAGE_SIZE, GraphEdgePayload, GraphNodePayload, TaskContext,
};
use lci_codegraph::IndexOutput;

/// Read `GRAPH_SUBMIT_PAGE_SIZE`, clamped to ≥1. Falls back to [`DEFAULT_GRAPH_PAGE_SIZE`] when
/// unset or unparseable.
#[must_use]
pub fn graph_page_size() -> usize {
    std::env::var("GRAPH_SUBMIT_PAGE_SIZE")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map(|n| n.max(1))
        .unwrap_or(DEFAULT_GRAPH_PAGE_SIZE)
}

/// Whether a failed page sequence may discard the commit's snapshot.
///
/// Pages commit individually, so an interrupted sequence leaves the ones that already landed.
/// Removing them is right only when this run created them: a snapshot that predates the run is a
/// complete graph for this commit, and replacing it is this run's job where destroying it is not.
/// An unknown prior state (`None`) is treated as pre-existing — leaving a graph in place is
/// recoverable on the next run, deleting one is not.
#[must_use]
fn may_discard(symbols_before: Option<u64>) -> bool {
    matches!(symbols_before, Some(0))
}

/// Submit the structural half of `out` — nodes and edges, no vectors — to the control plane.
/// Returns `(nodes, edges)` submitted; an empty graph is a no-op. Best-effort: the caller logs a
/// failure without failing the whole task. Languages without a graph extractor yet contribute no
/// structural facts.
///
/// A symbol's `:Symbol.embedding` arrives separately, on the chunk that is its body, keyed by
/// `node_id` (ADR-0116). Run this **before** [`super::index_chunks`] so a symbol exists by the time
/// its vector is offered; a vector whose symbol is absent is dropped rather than creating a node
/// with no structural facts.
pub async fn index_graph(
    context: &TaskContext,
    out: &IndexOutput,
    client: &ControlPlaneClient,
) -> anyhow::Result<(usize, usize)> {
    let commit_sha = context
        .head_sha
        .as_deref()
        .unwrap_or(&context.default_branch)
        .to_string();

    if out.graph.nodes.is_empty() {
        tracing::info!("codegraph produced no nodes; skipping graph submit");
        return Ok((0, 0));
    }

    let nodes: Vec<GraphNodePayload> = out
        .graph
        .nodes
        .iter()
        .map(|n| GraphNodePayload {
            node_id: n.node_id.clone(),
            label: n.label.clone(),
            source_file: n.source_file.clone(),
            start_line: n.start_line,
        })
        .collect();
    let edges: Vec<GraphEdgePayload> = out
        .graph
        .edges
        .iter()
        .map(|e| GraphEdgePayload {
            source: e.source.clone(),
            target: e.target.clone(),
            relation: e.relation.clone(),
        })
        .collect();

    let (n, e) = (nodes.len(), edges.len());
    let page_size = graph_page_size();

    // Read before the first page lands; afterwards the count reflects this run's own writes.
    let symbols_before = match client.graph_node_count(context.task_id).await {
        Ok(count) => Some(count),
        Err(error) => {
            tracing::warn!(
                error = %format!("{error:#}"),
                "graph snapshot lookup failed; a failed submit will leave the snapshot in place"
            );
            None
        }
    };
    if let Err(error) = client
        .submit_graph_paged(context.task_id, &commit_sha, &nodes, &edges, page_size)
        .await
    {
        // Discarding leaves the commit un-indexed, which readers handle, rather than a subset that
        // reads as a complete graph — a missing edge is indistinguishable from a symbol that
        // genuinely has no callers. That trade only applies to a snapshot this run created.
        if may_discard(symbols_before) {
            if let Err(discard) = client.discard_graph(context.task_id).await {
                tracing::warn!(
                    error = %format!("{discard:#}"),
                    "discarding the partial graph failed; the snapshot may hold an incomplete graph"
                );
            }
        } else {
            tracing::warn!(
                symbols_before = ?symbols_before,
                "graph submit failed over a snapshot that predates this run; leaving it in place"
            );
        }
        return Err(error).context("submitting codegraph structural graph");
    }
    tracing::info!(
        nodes = n,
        edges = e,
        page_size,
        "in-house (lci-codegraph) structural graph submitted"
    );
    Ok((n, e))
}

#[cfg(test)]
mod tests {
    use super::may_discard;

    #[test]
    fn only_a_snapshot_this_run_created_may_be_discarded() {
        assert!(
            may_discard(Some(0)),
            "the commit had no graph, so every page came from this run"
        );
        assert!(
            !may_discard(Some(1)),
            "a snapshot that predates this run is a complete graph; replacing it is this run's job"
        );
        assert!(
            !may_discard(Some(5_154)),
            "a populated snapshot is never this run's to delete"
        );
        assert!(
            !may_discard(None),
            "an unknown prior state leaves the snapshot alone: a graph left in place is recoverable \
             on the next run, one deleted is not"
        );
    }
}
