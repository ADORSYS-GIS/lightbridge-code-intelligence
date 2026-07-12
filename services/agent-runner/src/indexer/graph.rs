//! Structural graph via the in-house `lci-codegraph` crate (ADR-0086) — the sole graph engine.
//!
//! The runner extracts the structural code graph **in-process** with `lci-codegraph` (tree-sitter,
//! no subprocess): it emits `GraphNodePayload`/`GraphEdgePayload` and hands them to the control
//! plane, which owns the Neo4j write. This replaced the Python **Graphify** CLI (ADR-0019), which is
//! gone; there is no fallback and no flag. Languages without a graph extractor yet simply produce no
//! structural facts (the semantic pgvector index still covers them via the tree-sitter chunker).

use std::path::Path;

use anyhow::Context;

use lci_agent_clients::{
    ControlPlaneClient, GraphBatch, GraphEdgePayload, GraphNodePayload, TaskContext,
};

/// Build the structural graph with `lci-codegraph` (in-process, tree-sitter) and submit it to the
/// control plane. Returns `(nodes, edges)` submitted; an empty graph is a no-op. Best-effort: the
/// caller logs a failure without failing the whole task (the semantic index may already have landed).
/// Languages without a graph extractor yet contribute no structural facts.
pub async fn index_graph(
    context: &TaskContext,
    checkout: &Path,
    client: &ControlPlaneClient,
) -> anyhow::Result<(usize, usize)> {
    let commit_sha = context
        .head_sha
        .as_deref()
        .unwrap_or(&context.default_branch)
        .to_string();

    // The walk is synchronous CPU work (tree-sitter); keep it off the async runtime.
    let checkout = checkout.to_path_buf();
    let out = tokio::task::spawn_blocking(move || {
        lci_codegraph::walk_checkout_from_env(&checkout, /* build_graph */ true)
    })
    .await
    .context("codegraph walk task panicked")?
    .context("codegraph walk failed")?;

    let nodes: Vec<GraphNodePayload> = out
        .graph
        .nodes
        .into_iter()
        .map(|n| GraphNodePayload {
            node_id: n.node_id,
            label: n.label,
            source_file: n.source_file,
            start_line: n.start_line,
        })
        .collect();
    let edges: Vec<GraphEdgePayload> = out
        .graph
        .edges
        .into_iter()
        .map(|e| GraphEdgePayload {
            source: e.source,
            target: e.target,
            relation: e.relation,
        })
        .collect();

    if nodes.is_empty() {
        tracing::info!("codegraph produced no nodes; skipping graph submit");
        return Ok((0, 0));
    }

    let (n, e) = (nodes.len(), edges.len());
    client
        .submit_graph(
            context.task_id,
            GraphBatch {
                commit_sha,
                nodes,
                edges,
            },
        )
        .await
        .context("submitting codegraph structural graph")?;
    tracing::info!(
        nodes = n,
        edges = e,
        "in-house (lci-codegraph) structural graph submitted"
    );
    Ok((n, e))
}
