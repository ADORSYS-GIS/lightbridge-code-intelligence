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
    ControlPlaneClient, EmbeddingsClient, GraphBatch, GraphEdgePayload, GraphNodePayload,
    TaskContext,
};

use super::{IndexTuning, chunker};

/// Build the structural graph with `lci-codegraph` (in-process, tree-sitter) and submit it to the
/// control plane. Returns `(nodes, edges)` submitted; an empty graph is a no-op. Best-effort: the
/// caller logs a failure without failing the whole task (the semantic index may already have landed).
/// Languages without a graph extractor yet contribute no structural facts.
///
/// `chunks` are the same chunks `index_checkout` already collected for pgvector (ADR-0114). Each
/// `lci-codegraph` symbol node is correlated against the chunk whose `[start_line, end_line]` range
/// *contains* that symbol's start line, and that chunk's already-known `content` is what gets embedded
/// for `:Symbol.embedding`. This is deliberately in-tree: `lci-codegraph` itself exposes no `end_line`
/// to slice a symbol's text independently, and adding one there would mean carrying an
/// embeddings-adjacent concern into a crate that otherwise has none. A node with no correlated chunk
/// (e.g. a symbol kind the chunker doesn't produce a chunk for) simply ships without an embedding — it
/// still gets its structural edges.
///
/// A range containment check, not an exact `start_line` match: `lci-codegraph` emits 1-based line
/// numbers, but the chunker's tree-sitter walk records `child.start_position().row`, which is 0-based
/// — confirmed live (every chunk/symbol pair for a real indexed repo was off by exactly 1), so an
/// exact-equality match missed almost everything, even for languages with full tree-sitter chunking.
/// Containment against the chunk's own `end_line` (already produced today, no `lci-codegraph` change
/// needed) absorbs that off-by-one and also correctly maps a symbol nested inside a larger chunk
/// (e.g. a method inside an `impl` block chunk) to that chunk's text.
pub async fn index_graph(
    context: &TaskContext,
    checkout: &Path,
    client: &ControlPlaneClient,
    embedder: &EmbeddingsClient,
    chunks: &[chunker::Chunk],
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

    let mut nodes: Vec<GraphNodePayload> = Vec::with_capacity(out.graph.nodes.len());
    // (node index into `nodes`, text to embed) — collected first so embedding happens in batches,
    // not one call per symbol.
    let mut embeddable: Vec<(usize, &str)> = Vec::new();
    for n in &out.graph.nodes {
        if let Some(chunk) = chunks
            .iter()
            .find(|c| chunk_contains_symbol(c, &n.source_file, n.start_line))
        {
            embeddable.push((nodes.len(), chunk.content.as_str()));
        }
        nodes.push(GraphNodePayload {
            node_id: n.node_id.clone(),
            label: n.label.clone(),
            source_file: n.source_file.clone(),
            start_line: n.start_line,
            embedding: None,
        });
    }

    let embed_batch_size = IndexTuning::from_env().embed_batch_size;
    for batch in embeddable.chunks(embed_batch_size) {
        let texts: Vec<&str> = batch.iter().map(|(_, text)| *text).collect();
        let embeddings = embedder
            .embed(&texts)
            .await
            .context("embedding symbol batch")?;
        for ((idx, _), embedding) in batch.iter().zip(embeddings) {
            nodes[*idx].embedding = Some(embedding);
        }
    }
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

/// True if `chunk` is the one whose text a symbol at `symbol_start_line` in `symbol_file` should be
/// embedded with: same file, and the symbol's (1-based, `lci-codegraph`) start line falls within the
/// chunk's (0-based, tree-sitter) `[start_line, end_line]` range.
fn chunk_contains_symbol(
    chunk: &chunker::Chunk,
    symbol_file: &str,
    symbol_start_line: i64,
) -> bool {
    chunk.file_path == symbol_file
        && i64::from(chunk.start_line) <= symbol_start_line
        && symbol_start_line <= i64::from(chunk.end_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(file: &str, start_line: i32, end_line: i32) -> chunker::Chunk {
        chunker::Chunk {
            file_path: file.to_string(),
            language: "rust".to_string(),
            chunk_type: "function".to_string(),
            symbol_name: None,
            start_line,
            end_line,
            content: "fn f() {}".to_string(),
        }
    }

    #[test]
    fn matches_the_off_by_one_case_seen_live_against_a_real_indexed_repo() {
        // authix-todo, src/to_do/mod.rs: chunker chunk start_line=8 (0-based tree-sitter row),
        // lci-codegraph symbol start_line=9 (1-based) — an exact-equality match missed this
        // every time; containment against the chunk's own end_line catches it.
        let c = chunk("src/to_do/mod.rs", 8, 18);
        assert!(chunk_contains_symbol(&c, "src/to_do/mod.rs", 9));
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
    fn matches_at_both_inclusive_boundaries() {
        let c = chunk("src/a.rs", 10, 20);
        assert!(chunk_contains_symbol(&c, "src/a.rs", 10));
        assert!(chunk_contains_symbol(&c, "src/a.rs", 20));
    }
}
