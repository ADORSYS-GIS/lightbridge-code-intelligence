# Cloning and Indexing Pipeline

This report details how Lightbridge Code Intelligence ingests a codebase, parses it using Tree-sitter, and creates the dual semantic and structural indices used by the agent during reviews.

## 1. Execution Context

Indexing runs inside the `agent-runner` Kubernetes Job (`services/agent-runner/src/main.rs`). It is triggered either by a standalone `index` task (e.g., on a default-branch push) or as a prerequisite for a review on a previously unindexed repository. 

The runner clones the checkout, and a unified in-process walk over the files generates both indices from a **single Tree-sitter parse** per file.

## 2. Tree-sitter and Parse-Once Architecture

Lightbridge relies on `tree-sitter` for syntax-aware parsing. The crate `services/codegraph/src/lang/mod.rs` defines the supported languages.

As the checkout is walked, each file is parsed into an Abstract Syntax Tree (AST). This single `Tree` is then fed to two independent indexers concurrently:

```rust
// services/codegraph/src/walk.rs:161
let file_chunks = if options.build_graph && lang::has_graph(language) {
    // Parse ONCE and feed both the chunker and the graph builder (ADR-0086 "parse once").
    if let Some(tree) = lang::parse(&source, language) {
        // 1. Extract structural graph symbols and edges
        file_symbols.push(graph::extract_file(&tree, &rel_path, &source, language));
        
        // 2. Extract semantic chunks for vector embeddings
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
```

## 3. Semantic Indexing (pgvector)

**Chunking (`chunker.rs`):**
Tree-sitter recursively extracts named items (`function`, `impl`, `class`, etc.) into discrete chunks. Fallbacks to windowed text generation for grammar-less files.

**Embeddings and Storage (`code_chunks` table):**
The chunks are sent to an embeddings endpoint (`qwen3-embedding-8b` producing 4096 dimensions). The vectors are stored in PostgreSQL using `pgvector`:

```sql
-- Migration: 0004_code_chunks.sql
CREATE TABLE code_chunks (
    id            BIGSERIAL PRIMARY KEY,
    repository_id BIGINT  NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    commit_sha    TEXT    NOT NULL,
    file_path     TEXT    NOT NULL,
    language      TEXT    NOT NULL,
    chunk_type    TEXT    NOT NULL,
    symbol_name   TEXT,
    start_line    INT     NOT NULL,
    end_line      INT     NOT NULL,
    content       TEXT    NOT NULL,
    embedding     vector(4096) NOT NULL,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    UNIQUE (repository_id, commit_sha, file_path, start_line, end_line)
);
```
*Note: Because 4096 dimensions exceed pgvector's HNSW index limits, the system uses an exact cosine scan, scoped by the unique `(repository_id, commit_sha)` pair.*

## 4. Structural Indexing (Neo4j)

Graph extraction happens entirely in-process using the Rust `lci-codegraph` crate, replacing the old Python-based "Graphify" tool (ADR-0086).

**Extraction Process:**
1. **Definitions & Calls (`tags.rs`):** Uses the language's bundled `tags.scm` tree-sitter query to find definitions and references.
2. **Nodes and Edges (`emit.rs`):** Emits `GraphNode` (symbols) and `GraphEdge` (relations).
3. **Cross-file Resolution (`resolve.rs`):** Matches call sites to definitions across different files, favoring precision over broad matching.

## 5. Snapshot Model and Pruning

Both indices are grouped into snapshots defined by `(repository_id, commit_sha)`. A dedicated `index_sweeper` job periodically prunes stale snapshots to save space, keeping only the most recent default-branch commit and any commits currently pinned by active tasks.
