//! Structural graph via the in-house `lci-codegraph` crate (ADR-0086), **behind a default-off flag**.
//!
//! This is the strangler seam for slice 1 (RFC-0007): the crate emits the same
//! `GraphNodePayload`/`GraphEdgePayload` shape Graphify's `graph.json` parse produces
//! ([`super::graph`]), so the control plane / Neo4j / retrieval tools cannot tell which extractor
//! produced a given graph. Graphify stays the default; setting `LCI_CODEGRAPH_GRAPH` opts a run into
//! the in-house **Rust-only** graph so we can measure it side-by-side without changing prod behavior.

use std::path::Path;

use anyhow::Context;

use lci_agent_clients::{
    ControlPlaneClient, GraphBatch, GraphEdgePayload, GraphNodePayload, TaskContext,
};

/// Env flag selecting the in-house Rust code-graph over Graphify. Off by default (ADR-0086: additive,
/// no prod behavior change until the parity cutover).
pub const ENV_CODEGRAPH_GRAPH: &str = "LCI_CODEGRAPH_GRAPH";

/// True when the operator has opted this run into the in-house graph.
#[must_use]
pub fn enabled() -> bool {
    std::env::var(ENV_CODEGRAPH_GRAPH)
        .ok()
        .is_some_and(|v| matches!(v.trim(), "1" | "true" | "rust" | "on"))
}

/// Build the structural graph with `lci-codegraph` (Rust only) and submit it to the control plane.
/// Same return + best-effort contract as [`super::graph::index_graph`]: `(nodes, edges)` submitted;
/// an empty graph is a no-op. Non-Rust languages get **no** graph on this path (Graphify is skipped),
/// which is why the flag is experimental/Rust-only until parity.
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
