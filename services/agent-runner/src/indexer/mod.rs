//! Indexing pipeline: one `lci-codegraph` walk of the checkout → structural graph → embed → submit.
//!
//! The walk parses each file once and produces both halves of the index (ADR-0086): the semantic
//! chunks that become `code_chunks` rows in pgvector, and the structural nodes/edges that become
//! `:Symbol` nodes in Neo4j. A chunk that is a definition's body carries that definition's
//! `node_id`, recorded during the same parse, which is how a chunk's vector reaches its symbol
//! (ADR-0116). Both stores are written through the internal API — the runner has no direct DB
//! access. See docs/indexing-and-storage.md.

pub mod graph;

use std::path::Path;

use anyhow::Context;

use lci_agent_clients::{
    ChunkBatch, ChunkPayload, ControlPlaneClient, EmbeddingsClient, TaskContext,
};
use lci_codegraph::IndexOutput;

/// Chunks embedded and submitted per round-trip. Larger = fewer requests (kinder to per-minute rate
/// limits) but a bigger embeddings response body, which some gateways cap.
/// `INDEX_EMBED_BATCH_SIZE` (default 32).
///
/// Chunk shape itself — window size, line and byte ceilings — is `lci-codegraph`'s to tune, under
/// its own `LCI_CODEGRAPH_*` variables.
const DEFAULT_EMBED_BATCH_SIZE: usize = 32;

/// Read `INDEX_EMBED_BATCH_SIZE`, clamped to ≥1 so a misconfiguration can't wedge the pipeline.
/// Falls back to [`DEFAULT_EMBED_BATCH_SIZE`] when unset or unparseable.
#[must_use]
pub fn embed_batch_size() -> usize {
    std::env::var("INDEX_EMBED_BATCH_SIZE")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map(|n| n.max(1))
        .unwrap_or(DEFAULT_EMBED_BATCH_SIZE)
}

/// Walk the checkout once, producing the chunks and the structural graph from a single parse.
///
/// Tree-sitter parsing is synchronous CPU work, so it runs on a blocking thread rather than stalling
/// the async runtime.
pub async fn walk(checkout: &Path) -> anyhow::Result<IndexOutput> {
    let checkout = checkout.to_path_buf();
    tokio::task::spawn_blocking(move || {
        lci_codegraph::walk_checkout_from_env(&checkout, /* build_graph */ true)
    })
    .await
    .context("codegraph walk task panicked")?
    .context("codegraph walk failed")
}

/// Embed every chunk from `out` and submit it to the control plane. Returns the number submitted.
///
/// Each chunk carries the `node_id` of the definition it is the body of, when the walk linked one;
/// the control plane uses it to attach this chunk's vector to that `:Symbol` (ADR-0116). Submit the
/// structural graph first ([`graph::index_graph`]) so those symbols exist to be matched.
pub async fn index_chunks(
    context: &TaskContext,
    out: &IndexOutput,
    client: &ControlPlaneClient,
    embedder: &EmbeddingsClient,
) -> anyhow::Result<usize> {
    let commit_sha = context
        .head_sha
        .as_deref()
        .unwrap_or(&context.default_branch)
        .to_string();

    if out.chunks.is_empty() {
        tracing::info!("no chunks produced (empty or all-binary repo)");
        return Ok(0);
    }

    let batch_size = embed_batch_size();
    let total = out.chunks.len();
    let linked = out.chunks.iter().filter(|c| c.node_id.is_some()).count();
    tracing::info!(
        chunk_count = total,
        linked_to_a_symbol = linked,
        embed_batch_size = batch_size,
        "walk complete; embedding in batches"
    );

    let mut submitted = 0usize;
    for (batch_idx, batch_chunks) in out.chunks.chunks(batch_size).enumerate() {
        // The embeddings client bounds each input to what the model accepts, so an oversized chunk
        // is its concern, not this loop's.
        let texts: Vec<&str> = batch_chunks.iter().map(|c| c.content.as_str()).collect();
        let embeddings = embedder
            .embed(&texts)
            .await
            .with_context(|| format!("embedding batch {batch_idx}"))?;

        let payloads: Vec<ChunkPayload> = batch_chunks
            .iter()
            .zip(embeddings)
            .map(|(c, embedding)| ChunkPayload {
                file_path: c.file_path.clone(),
                language: c.language.clone(),
                chunk_type: c.chunk_type.clone(),
                symbol_name: c.symbol_name.clone(),
                start_line: c.start_line,
                end_line: c.end_line,
                content: c.content.clone(),
                embedding,
                node_id: c.node_id.clone(),
            })
            .collect();

        client
            .submit_chunks(
                context.task_id,
                ChunkBatch {
                    commit_sha: commit_sha.clone(),
                    chunks: payloads,
                },
            )
            .await
            .with_context(|| format!("submitting chunk batch {batch_idx}"))?;

        submitted += batch_chunks.len();
        tracing::info!(submitted, total, "indexing progress");
    }

    Ok(submitted)
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_EMBED_BATCH_SIZE, embed_batch_size};

    /// The env var is process-global, so these cases share one test rather than racing each other.
    #[test]
    fn embed_batch_size_parses_clamps_to_one_and_falls_back() {
        let key = "INDEX_EMBED_BATCH_SIZE";
        unsafe { std::env::remove_var(key) };
        assert_eq!(
            embed_batch_size(),
            DEFAULT_EMBED_BATCH_SIZE,
            "unset → default"
        );
        unsafe { std::env::set_var(key, "12") };
        assert_eq!(embed_batch_size(), 12, "parses a value");
        unsafe { std::env::set_var(key, "0") };
        assert_eq!(embed_batch_size(), 1, "zero clamps to 1");
        unsafe { std::env::set_var(key, "not-a-number") };
        assert_eq!(
            embed_batch_size(),
            DEFAULT_EMBED_BATCH_SIZE,
            "unparseable → default"
        );
        unsafe { std::env::remove_var(key) };
    }
}
