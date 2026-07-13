//! Indexing pipeline: walk the checkout → chunk by language → embed → submit to control plane.
//!
//! Slice 2 of epic #5. Produces `code_chunks` rows in the control-plane's Postgres (via the
//! internal API — the runner has no direct DB access). See docs/indexing-and-storage.md.

pub mod chunker;
// In-house structural graph via the `lci-codegraph` crate (ADR-0086) — the sole graph engine,
// in-process (tree-sitter). Replaced the retired Python Graphify CLI (ADR-0019).
pub mod graph;
pub mod language;

use std::path::Path;

use anyhow::Context;

use lci_agent_clients::{
    ChunkBatch, ChunkPayload, ControlPlaneClient, EmbeddingsClient, TaskContext,
};

/// Operator-tunable indexer knobs (ADR-0010 / epic #5). Read from the environment once per run and
/// clamped to ≥1 so a misconfiguration can't wedge the pipeline. These were hardcoded, which made a
/// downstream limit (e.g. an AI-gateway capping the batched-embedding *response* size) impossible to
/// work around without a rebuild — the reason this struct exists.
#[derive(Clone, Copy, Debug)]
pub struct IndexTuning {
    /// Chunks embedded + submitted per round-trip. Larger = fewer requests (kinder to per-minute
    /// rate limits) but a bigger embeddings response body, which some gateways cap.
    /// `INDEX_EMBED_BATCH_SIZE` (default 32).
    pub embed_batch_size: usize,
    /// Max lines a structured (tree-sitter) chunk may span before it is split into windows.
    /// `INDEX_MAX_CHUNK_LINES` (default 150).
    pub max_chunk_lines: usize,
    /// Windowed-fallback window size, in lines. `INDEX_WINDOW_SIZE` (default 100).
    pub window_size: usize,
    /// Windowed-fallback step, in lines (overlap = `window_size - window_step`). `INDEX_WINDOW_STEP`
    /// (default 50).
    pub window_step: usize,
}

impl Default for IndexTuning {
    fn default() -> Self {
        Self {
            embed_batch_size: 32,
            max_chunk_lines: 150,
            window_size: 100,
            window_step: 50,
        }
    }
}

impl IndexTuning {
    /// Read the knobs from the environment, falling back to [`Default`] and clamping each to ≥1.
    #[must_use]
    pub fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            embed_batch_size: env_usize("INDEX_EMBED_BATCH_SIZE", defaults.embed_batch_size),
            max_chunk_lines: env_usize("INDEX_MAX_CHUNK_LINES", defaults.max_chunk_lines),
            window_size: env_usize("INDEX_WINDOW_SIZE", defaults.window_size),
            window_step: env_usize("INDEX_WINDOW_STEP", defaults.window_step),
        }
    }
}

/// Parse a `usize` env var, clamping to ≥1; falls back to `default` when unset or unparseable.
fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map(|n| n.max(1))
        .unwrap_or(default)
}

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

    let tuning = IndexTuning::from_env();
    let chunks = collect_chunks(checkout, tuning)
        .await
        .context("collecting chunks")?;
    if chunks.is_empty() {
        tracing::info!("no chunks produced (empty or all-binary repo)");
        return Ok(0);
    }
    tracing::info!(
        chunk_count = chunks.len(),
        embed_batch_size = tuning.embed_batch_size,
        "chunking complete; embedding in batches"
    );

    let mut submitted = 0usize;
    let total = chunks.len();

    for (batch_idx, batch_chunks) in chunks.chunks(tuning.embed_batch_size).enumerate() {
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
async fn collect_chunks(root: &Path, tuning: IndexTuning) -> anyhow::Result<Vec<chunker::Chunk>> {
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

                let file_chunks = chunker::chunk_file(&rel_path, &source, lang, tuning);
                all_chunks.extend(file_chunks);
            }
        }

        all_chunks
    })
    .await
    .context("chunk collection task panicked")
}

#[cfg(test)]
mod tests {
    use super::{IndexTuning, env_usize};

    #[test]
    fn defaults_match_the_historical_hardcoded_values() {
        let tuning = IndexTuning::default();
        assert_eq!(tuning.embed_batch_size, 32);
        assert_eq!(tuning.max_chunk_lines, 150);
        assert_eq!(tuning.window_size, 100);
        assert_eq!(tuning.window_step, 50);
    }

    #[test]
    fn env_usize_parses_clamps_to_one_and_falls_back() {
        // A test-unique key so this never races another test's environment.
        let key = "LCI_TEST_INDEX_ENV_USIZE";
        unsafe { std::env::remove_var(key) };
        assert_eq!(env_usize(key, 7), 7, "unset → default");
        unsafe { std::env::set_var(key, "12") };
        assert_eq!(env_usize(key, 7), 12, "parses a value");
        unsafe { std::env::set_var(key, "0") };
        assert_eq!(env_usize(key, 7), 1, "zero clamps to 1");
        unsafe { std::env::set_var(key, "not-a-number") };
        assert_eq!(env_usize(key, 7), 7, "unparseable → default");
        unsafe { std::env::remove_var(key) };
    }
}
