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

use super::EmbeddedChunk;

/// Build the structural graph with `lci-codegraph` (in-process, tree-sitter) and submit it to the
/// control plane. Returns `(nodes, edges)` submitted; an empty graph is a no-op. Best-effort: the
/// caller logs a failure without failing the whole task (the semantic index may already have landed).
/// Languages without a graph extractor yet contribute no structural facts.
///
/// `chunks` are what `index_checkout` already embedded for pgvector (ADR-0114). Each symbol node
/// takes `:Symbol.embedding` from the chunk whose `[start_line, end_line]` range contains that
/// symbol's start line — a range check rather than an exact match, since the two walks use different
/// line-numbering conventions, and so that a symbol nested inside a larger chunk (e.g. a method
/// inside an `impl` block) still resolves to the chunk covering it. The vector is the one computed
/// for that chunk's text, so this pass makes no embeddings call of its own (ADR-0116). A node with
/// no covering chunk ships without an embedding but still gets its structural edges.
pub async fn index_graph(
    context: &TaskContext,
    checkout: &Path,
    client: &ControlPlaneClient,
    chunks: &[EmbeddedChunk],
) -> anyhow::Result<(usize, usize)> {
    let commit_sha = context
        .head_sha
        .as_deref()
        .unwrap_or(&context.default_branch)
        .to_string();

    // The walk is synchronous CPU work (tree-sitter); keep it off the async runtime.
    let checkout_owned = checkout.to_path_buf();
    let out = tokio::task::spawn_blocking(move || {
        lci_codegraph::walk_checkout_from_env(&checkout_owned, /* build_graph */ true)
    })
    .await
    .context("codegraph walk task panicked")?
    .context("codegraph walk failed")?;

    let nodes: Vec<GraphNodePayload> = out
        .graph
        .nodes
        .iter()
        .map(|n| GraphNodePayload {
            node_id: n.node_id.clone(),
            label: n.label.clone(),
            source_file: n.source_file.clone(),
            start_line: n.start_line,
            embedding: embedding_for(chunks, &n.source_file, n.start_line),
        })
        .collect();
    let embedded_count = nodes.iter().filter(|n| n.embedding.is_some()).count();

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
        embedded = embedded_count,
        "in-house (lci-codegraph) structural graph submitted"
    );
    Ok((n, e))
}

/// The vector a symbol at `(source_file, start_line)` carries: the one embedded for the chunk whose
/// line span covers it. `None` when no chunk covers that line — the symbol is still submitted, with
/// its structural facts and no embedding.
fn embedding_for(chunks: &[EmbeddedChunk], source_file: &str, start_line: i64) -> Option<Vec<f32>> {
    chunks
        .iter()
        .find(|c| chunk_contains_symbol(c, source_file, start_line))
        .map(|c| c.embedding.clone())
}

/// True if `chunk` is the one whose vector a symbol at `symbol_start_line` in `symbol_file` should
/// carry: same file, and the symbol's start line falls within the chunk's
/// `[start_line, end_line]` range.
fn chunk_contains_symbol(chunk: &EmbeddedChunk, symbol_file: &str, symbol_start_line: i64) -> bool {
    chunk.file_path == symbol_file
        && i64::from(chunk.start_line) <= symbol_start_line
        && symbol_start_line <= i64::from(chunk.end_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(file: &str, start_line: i32, end_line: i32) -> EmbeddedChunk {
        EmbeddedChunk {
            file_path: file.to_string(),
            start_line,
            end_line,
            embedding: vec![0.1, 0.2, 0.3],
        }
    }

    #[test]
    fn matches_when_the_chunk_and_symbol_use_different_line_numbering() {
        let c = chunk("src/lib.rs", 8, 18);
        assert!(chunk_contains_symbol(&c, "src/lib.rs", 9));
    }

    #[test]
    fn matches_a_symbol_nested_inside_a_larger_chunk() {
        let c = chunk("src/lib.rs", 0, 97);
        assert!(chunk_contains_symbol(&c, "src/lib.rs", 42));
    }

    #[test]
    fn does_not_match_a_different_file() {
        let c = chunk("src/a.rs", 0, 10);
        assert!(!chunk_contains_symbol(&c, "src/b.rs", 5));
    }

    #[test]
    fn does_not_match_a_line_outside_the_chunk_range() {
        let c = chunk("src/a.rs", 10, 20);
        assert!(!chunk_contains_symbol(&c, "src/a.rs", 5));
        assert!(!chunk_contains_symbol(&c, "src/a.rs", 21));
    }

    #[test]
    fn a_symbol_carries_the_vector_of_the_chunk_covering_it() {
        let chunks = vec![
            EmbeddedChunk {
                file_path: "src/a.rs".to_string(),
                start_line: 0,
                end_line: 20,
                embedding: vec![1.0, 2.0],
            },
            EmbeddedChunk {
                file_path: "src/b.rs".to_string(),
                start_line: 0,
                end_line: 20,
                embedding: vec![3.0, 4.0],
            },
        ];
        assert_eq!(
            embedding_for(&chunks, "src/b.rs", 7),
            Some(vec![3.0, 4.0]),
            "the covering chunk's own vector, not the first chunk in the slice"
        );
    }

    #[test]
    fn a_symbol_no_chunk_covers_carries_no_vector() {
        let chunks = vec![chunk("src/a.rs", 10, 20)];
        assert_eq!(embedding_for(&chunks, "src/a.rs", 99), None);
        assert_eq!(embedding_for(&chunks, "src/other.rs", 15), None);
        assert_eq!(embedding_for(&[], "src/a.rs", 15), None);
    }

    #[test]
    fn matches_at_both_inclusive_boundaries() {
        let c = chunk("src/a.rs", 10, 20);
        assert!(chunk_contains_symbol(&c, "src/a.rs", 10));
        assert!(chunk_contains_symbol(&c, "src/a.rs", 20));
    }
}
