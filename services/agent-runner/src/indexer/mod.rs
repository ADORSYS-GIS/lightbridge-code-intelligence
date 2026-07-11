//! Indexing pipeline: walk the checkout → chunk by language → embed → submit to control plane.
//!
//! Slice 2 of epic #5. Produces `code_chunks` rows in the control-plane's Postgres (via the
//! internal API — the runner has no direct DB access). See docs/indexing-and-storage.md.

pub mod chunker;
pub mod graph;
pub mod language;

use std::path::Path;

use anyhow::Context;

use lci_agent_clients::{
    ChunkBatch, ChunkPayload, ControlPlaneClient, EmbeddingsClient, TaskContext,
};

/// How many chunks we embed and submit in one round-trip. Balances request size vs latency.
/// Most embedding APIs accept up to 2048 items; 32 is a safe default that keeps batches small
/// enough to stay well under typical token-per-minute rate limits.
const EMBED_BATCH_SIZE: usize = 32;

/// Index the checkout directory and submit all chunks to the control plane.
/// Returns the total number of chunks submitted.
pub async fn index_checkout(
    context: &TaskContext,
    checkout: &Path,
    client: &ControlPlaneClient,
    embedder: &EmbeddingsClient,
) -> anyhow::Result<usize> {
    let commit_sha = context
        .head_sha
        .as_deref()
        .unwrap_or(&context.default_branch)
        .to_string();

    let chunks = collect_chunks(checkout)
        .await
        .context("collecting chunks")?;
    if chunks.is_empty() {
        tracing::info!("no chunks produced (empty or all-binary repo)");
        return Ok(0);
    }
    tracing::info!(
        chunk_count = chunks.len(),
        "chunking complete; embedding in batches of {EMBED_BATCH_SIZE}"
    );

    let mut submitted = 0usize;
    let total = chunks.len();

    for (batch_idx, batch_chunks) in chunks.chunks(EMBED_BATCH_SIZE).enumerate() {
        let texts: Vec<&str> = batch_chunks.iter().map(|c| c.content.as_str()).collect();
        let embeddings = embedder
            .embed(&texts)
            .await
            .with_context(|| format!("embedding batch {batch_idx}"))?;

        let payloads: Vec<ChunkPayload> = batch_chunks
            .iter()
            .zip(embeddings)
            .map(|(c, emb)| ChunkPayload {
                file_path: c.file_path.clone(),
                language: c.language.clone(),
                chunk_type: c.chunk_type.clone(),
                symbol_name: c.symbol_name.clone(),
                start_line: c.start_line,
                end_line: c.end_line,
                content: c.content.clone(),
                embedding: emb,
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

/// Walk the checkout directory and produce chunks for every indexable file.
async fn collect_chunks(root: &Path) -> anyhow::Result<Vec<chunker::Chunk>> {
    // Run the file walk + tree-sitter parsing on a blocking thread so we don't stall the async
    // runtime (tree-sitter is synchronous CPU work).
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut all_chunks = Vec::new();
        let mut stack = vec![root.clone()];

        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(dir = %dir.display(), error = %e, "cannot read directory");
                    continue;
                }
            };

            for entry in entries.flatten() {
                let path = entry.path();
                let ft = match entry.file_type() {
                    Ok(ft) => ft,
                    Err(_) => continue,
                };

                if ft.is_dir() {
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    // Skip well-known non-code directories (including Python venvs and build dirs).
                    if matches!(
                        name,
                        ".git"
                            | "node_modules"
                            | "target"
                            | ".next"
                            | "dist"
                            | ".venv"
                            | "venv"
                            | "__pycache__"
                            | "build"
                    ) {
                        continue;
                    }
                    stack.push(path);
                    continue;
                }

                if !ft.is_file() {
                    continue;
                }

                let Some(lang) = language::from_path(&path) else {
                    continue;
                };

                // Use forward slashes regardless of OS so DB paths are platform-consistent.
                let rel_path = path
                    .strip_prefix(&root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");

                // Guard large files before allocating memory for them.
                const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;
                if path.metadata().map(|m| m.len()).unwrap_or(0) > MAX_FILE_BYTES {
                    continue;
                }

                let source = match std::fs::read_to_string(&path) {
                    Ok(s) => s,
                    Err(_) => continue, // binary or unreadable
                };

                let file_chunks = chunker::chunk_file(&rel_path, &source, lang);
                all_chunks.extend(file_chunks);
            }
        }

        all_chunks
    })
    .await
    .context("chunk collection task panicked")
}
