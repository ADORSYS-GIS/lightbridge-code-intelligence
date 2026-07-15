//! The one-pass walk (ADR-0086). Walks a checkout and produces both the semantic chunks and the
//! structural graph over **one** tree-sitter parse per file, honouring the repo `.gitignore`
//! (composed with the operator ignore layer), and routing PDFs through bounded text extraction.
//!
//! This is the crate's top-level entry point; the agent-plane's `index` mode (and today's
//! `agent-runner`, behind a flag) drives it and maps the results onto the internal-API payloads.

use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ignore::WalkBuilder;

use crate::chunk::{self, Chunk, MAX_FILE_BYTES};
use crate::graph::{self, FileSymbols, Graph};
use crate::ignore_list::{IgnoreConfig, IgnoreList};
use crate::lang;
use crate::pdf::{self, PdfOutcome};
use crate::tuning::IndexTuning;

/// Options for [`walk_checkout`]. `bon` builder (≥3 fields, ADR-0083 idiom).
#[derive(Debug, Clone, bon::Builder)]
pub struct WalkOptions {
    /// Chunking/window tuning (carried over verbatim, ADR-0086).
    #[builder(default)]
    pub tuning: IndexTuning,
    /// Honour the repo's own `.gitignore` (and nested/parent ignore files). Default true — this is
    /// what makes the operator layer *compose with* rather than *replace* the repo ignores.
    #[builder(default = true)]
    pub respect_gitignore: bool,
    /// Build the in-house structural graph (Rust, Python, TS/JS+TSX, Java). Default `false` so the crate is
    /// behavior-neutral until a host opts in (ADR-0086 migration shape).
    #[builder(default = false)]
    pub build_graph: bool,
    /// Extract text from PDFs and index it. Default true.
    #[builder(default = true)]
    pub extract_pdfs: bool,
    /// Operator-configurable extra ignore globs, layered on top of the built-in defaults.
    #[builder(default)]
    pub extra_ignore_globs: Vec<String>,
}

/// Counters for one walk. Logged so a too-broad ignore glob (or a PDF-heavy repo) is diagnosable.
#[derive(Debug, Default, Clone, Copy)]
pub struct WalkStats {
    pub files_chunked: usize,
    pub paths_ignored: usize,
    pub pdfs_extracted: usize,
    pub pdfs_skipped: usize,
}

/// The output of a walk: chunks to embed and the resolved structural graph.
#[derive(Debug, Default)]
pub struct WalkOutput {
    pub chunks: Vec<Chunk>,
    pub graph: Graph,
    pub stats: WalkStats,
}

/// Walk `root`, producing chunks + (optionally) the structural graph. Synchronous CPU work — callers
/// on an async runtime should wrap this in `spawn_blocking` (as `agent-runner` does).
pub fn walk_checkout(root: &Path, options: &WalkOptions) -> anyhow::Result<WalkOutput> {
    let ignore_list = Arc::new(IgnoreList::build(
        &IgnoreConfig::builder()
            .root(root)
            .extra_globs(options.extra_ignore_globs.clone())
            .build(),
    )?);
    let ignored_count = Arc::new(AtomicUsize::new(0));

    // Prune the operator-ignored paths (dirs + files) via `filter_entry` so we never descend into a
    // `node_modules`, while `git_ignore` handles the repo's own rules — the two compose.
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false) // don't blanket-skip dotfiles; the ignore lists decide
        .git_ignore(options.respect_gitignore)
        .git_global(false) // a machine-global gitignore must not affect indexing determinism
        .git_exclude(options.respect_gitignore)
        .parents(options.respect_gitignore)
        .require_git(false); // honour `.gitignore` even in a non-git fixture dir

    {
        let ignore_list = Arc::clone(&ignore_list);
        let ignored_count = Arc::clone(&ignored_count);
        builder.filter_entry(move |entry| {
            let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
            if ignore_list.is_ignored(entry.path(), is_dir) {
                tracing::debug!(path = %entry.path().display(), "codegraph: skipping ignored path");
                ignored_count.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            true
        });
    }

    let mut chunks: Vec<Chunk> = Vec::new();
    let mut file_symbols: Vec<FileSymbols> = Vec::new();
    let mut stats = WalkStats::default();

    for result in builder.build() {
        let entry = match result {
            Ok(e) => e,
            Err(error) => {
                tracing::warn!(%error, "codegraph: walk entry error");
                continue;
            }
        };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        let rel_path = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        // ── PDFs: bounded text extraction → windowed chunks ──────────────────────────────────────
        if options.extract_pdfs && lang::is_pdf(path) {
            match pdf::extract_from_path(path) {
                PdfOutcome::Text(text) => {
                    let file_chunks = chunk::chunk_text(&rel_path, &text, "pdf", options.tuning);
                    if !file_chunks.is_empty() {
                        chunks.extend(file_chunks);
                        stats.files_chunked += 1;
                    }
                    stats.pdfs_extracted += 1;
                }
                PdfOutcome::TooLarge => {
                    tracing::debug!(path = %rel_path, "codegraph: PDF over byte cap, skipped");
                    stats.pdfs_skipped += 1;
                }
                PdfOutcome::Failed(reason) => {
                    tracing::debug!(path = %rel_path, %reason, "codegraph: PDF extraction failed, skipped");
                    stats.pdfs_skipped += 1;
                }
            }
            continue;
        }

        // ── Source / text files ─────────────────────────────────────────────────────────────────
        let Some(language) = lang::from_path(path) else {
            continue;
        };
        // Bound the read at the I/O level, not via `metadata().len()`: the file could grow (or be a
        // pipe/special file) between the stat and the read (TOCTOU), so cap the bytes we actually
        // pull. Read at most MAX_FILE_BYTES + 1 — if we hit the extra byte the file is over cap.
        let mut source = String::new();
        let read = std::fs::File::open(path).and_then(|f| {
            f.take(MAX_FILE_BYTES as u64 + 1)
                .read_to_string(&mut source)
        });
        if read.is_err() {
            continue; // binary, unreadable, or non-UTF8
        }
        if source.len() > MAX_FILE_BYTES {
            continue; // over the byte cap
        }

        let file_chunks = if options.build_graph && lang::has_graph(language) {
            // Parse ONCE and feed both the chunker and the graph builder (ADR-0086 "parse once").
            if let Some(tree) = lang::parse(&source, language) {
                file_symbols.push(graph::extract_file(&tree, &rel_path, &source, language));
                let mut cs = chunk::chunk_tree(&tree, &rel_path, &source, language, options.tuning);
                if cs.is_empty() {
                    cs = chunk::chunk_text(&rel_path, &source, language, options.tuning);
                }
                cs
            } else {
                chunk::chunk_file(&rel_path, &source, language, options.tuning)
            }
        } else {
            chunk::chunk_file(&rel_path, &source, language, options.tuning)
        };

        if !file_chunks.is_empty() {
            chunks.extend(file_chunks);
            stats.files_chunked += 1;
        }
    }

    stats.paths_ignored = ignored_count.load(Ordering::Relaxed);
    let graph = if options.build_graph {
        graph::resolve(file_symbols)
    } else {
        Graph::default()
    };

    tracing::info!(
        files_chunked = stats.files_chunked,
        chunks = chunks.len(),
        graph_nodes = graph.nodes.len(),
        graph_edges = graph.edges.len(),
        paths_ignored = stats.paths_ignored,
        pdfs_extracted = stats.pdfs_extracted,
        pdfs_skipped = stats.pdfs_skipped,
        "codegraph: walk complete"
    );

    Ok(WalkOutput {
        chunks,
        graph,
        stats,
    })
}

/// Convenience: walk reading tuning + operator ignore globs from the environment
/// (`INDEX_*` and `LCI_CODEGRAPH_IGNORE_GLOBS`) — used by hosts that want env-driven config.
pub fn walk_checkout_from_env(root: &Path, build_graph: bool) -> anyhow::Result<WalkOutput> {
    let options = WalkOptions::builder()
        .tuning(IndexTuning::from_env())
        .build_graph(build_graph)
        .extra_ignore_globs(read_env_globs())
        .build();
    walk_checkout(root, &options)
}

fn read_env_globs() -> Vec<String> {
    std::env::var(crate::ignore_list::ENV_EXTRA_GLOBS)
        .ok()
        .map(|raw| {
            raw.split(['\n', ','])
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn walk_chunks_and_builds_graph_and_composes_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/a.rs", "fn caller() { target(); }\n");
        write(root, "src/b.rs", "fn target() {}\n");
        // Operator-default junk dir: must be skipped even though not in .gitignore.
        write(root, "target/generated.rs", "fn should_not_index() {}\n");
        // Repo .gitignore: composes with the operator layer.
        write(root, ".gitignore", "ignored_by_repo/\n");
        write(root, "ignored_by_repo/secret.rs", "fn secret() {}\n");

        let options = WalkOptions::builder().build_graph(true).build();
        let out = walk_checkout(root, &options).unwrap();

        // Cross-file edge built.
        assert!(
            out.graph.edges.iter().any(|e| e.relation == "calls"),
            "expected a cross-file calls edge, edges = {:?}",
            out.graph.edges
        );
        // Neither the operator-default `target/` nor the repo-.gitignored dir was indexed.
        assert!(
            !out.chunks
                .iter()
                .any(|c| c.file_path.contains("generated.rs")),
            "target/ must be skipped by the operator defaults"
        );
        assert!(
            !out.chunks.iter().any(|c| c.file_path.contains("secret.rs")),
            "repo .gitignore must compose and skip ignored_by_repo/"
        );
        assert!(
            out.stats.paths_ignored >= 1,
            "at least the target/ dir was pruned"
        );
    }

    #[test]
    fn walk_builds_cross_file_graph_for_python_and_ts() {
        // Proves the full walk path — extension detection, `has_graph` gating, the parse-once branch —
        // wires up for the new languages, not just the direct `extract_file` unit tests.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "svc/a.py", "def caller():\n    target()\n");
        write(root, "svc/b.py", "def target():\n    pass\n");
        write(
            root,
            "web/a.ts",
            "import { help } from './b';\nfunction run() { help(); }\n",
        );
        write(root, "web/b.ts", "export function help() {}\n");

        let options = WalkOptions::builder().build_graph(true).build();
        let out = walk_checkout(root, &options).unwrap();

        let cross_file = |lang_dir: &str| {
            out.graph.edges.iter().any(|e| {
                e.relation == "calls"
                    && out
                        .graph
                        .nodes
                        .iter()
                        .find(|n| n.node_id == e.source)
                        .is_some_and(|n| n.source_file.starts_with(lang_dir))
            })
        };
        assert!(
            cross_file("svc/"),
            "python cross-file call edge; edges = {:?}",
            out.graph.edges
        );
        assert!(
            cross_file("web/"),
            "ts cross-file call edge; edges = {:?}",
            out.graph.edges
        );
    }

    #[test]
    fn graph_off_by_default_is_behavior_neutral() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "src/a.rs",
            "fn caller() { target(); }\nfn target() {}\n",
        );
        let out = walk_checkout(root, &WalkOptions::builder().build()).unwrap();
        assert!(out.graph.nodes.is_empty(), "graph off by default");
        assert!(!out.chunks.is_empty(), "chunks still produced");
    }
}
