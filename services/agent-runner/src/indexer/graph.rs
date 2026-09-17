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
    if let Err(error) = client
        .submit_graph_paged(context.task_id, &commit_sha, &nodes, &edges, page_size)
        .await
    {
        // Pages commit individually, so a sequence that stops partway leaves the ones that already
        // landed. Discarding the snapshot leaves the commit un-indexed, which readers handle, rather
        // than a subset that reads as a complete graph — a missing edge is indistinguishable from a
        // symbol that genuinely has no callers.
        if let Err(discard) = client.discard_graph(context.task_id).await {
            tracing::warn!(
                error = %format!("{discard:#}"),
                "discarding the partial graph failed; the snapshot may hold an incomplete graph"
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
