//! Ingest submission: indexed code chunks (for pgvector search) and the structural code graph (for
//! Neo4j via lci-codegraph).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::ControlPlaneClient;

/// One code chunk to submit to the control plane (mirrors `internal.rs::ChunkInput`).
#[derive(Debug, Serialize)]
pub struct ChunkPayload {
    pub file_path: String,
    pub language: String,
    pub chunk_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_name: Option<String>,
    pub start_line: i32,
    pub end_line: i32,
    pub content: String,
    pub embedding: Vec<f32>,
    /// The graph node this chunk is the body of, when `lci-codegraph` linked one during the parse
    /// that produced both. The control plane attaches this chunk's vector to that `:Symbol`
    /// (ADR-0116). `None` for a chunk that is not a definition — a windowed slice, a text file, or a
    /// definition the graph pass did not emit a node for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
}

/// One already-stored chunk's position (mirrors `internal.rs::IndexedChunkKey`).
#[derive(Debug, Deserialize)]
struct IndexedChunkKey {
    file_path: String,
    start_line: i32,
    end_line: i32,
}

/// Body of `GET /internal/tasks/{id}/chunks/indexed`.
#[derive(Debug, Deserialize)]
struct IndexedChunkKeys {
    keys: Vec<IndexedChunkKey>,
}

/// Body for `POST /internal/tasks/{id}/chunks`.
#[derive(Debug, Serialize)]
pub struct ChunkBatch {
    pub commit_sha: String,
    pub chunks: Vec<ChunkPayload>,
}

/// One structural-graph node (mirrors `internal.rs::GraphNodeInput`). Structural facts only — a
/// symbol's vector reaches `:Symbol.embedding` on the chunk that is its body, keyed by `node_id`
/// (ADR-0116), so this payload never carries one.
#[derive(Debug, Clone, Serialize)]
pub struct GraphNodePayload {
    pub node_id: String,
    pub label: String,
    pub source_file: String,
    pub start_line: i64,
}

/// One directed edge (`contains` / `method` / `calls` / …).
#[derive(Debug, Clone, Serialize)]
pub struct GraphEdgePayload {
    pub source: String,
    pub target: String,
    pub relation: String,
}

/// Nodes or edges carried by one `submit_graph` request.
///
/// Bounds a request's duration by a constant rather than by repository size: the control plane
/// writes a batch in a single Neo4j transaction, so an unbounded submit grows with the repository
/// until it outlives the client's request timeout.
pub const DEFAULT_GRAPH_PAGE_SIZE: usize = 2_000;

/// Body for `POST /internal/tasks/{id}/graph`.
#[derive(Debug, Serialize)]
pub struct GraphBatch {
    pub commit_sha: String,
    pub nodes: Vec<GraphNodePayload>,
    pub edges: Vec<GraphEdgePayload>,
}

impl ControlPlaneClient {
    /// `GET /internal/tasks/{id}/chunks/indexed` — the chunks already stored for this task's
    /// snapshot, as `(file_path, start_line, end_line)`.
    ///
    /// An index re-running over a commit it has partially indexed can embed only what is missing.
    /// Empty for a snapshot that has never been indexed.
    pub async fn indexed_chunk_keys(
        &self,
        task_id: Uuid,
    ) -> anyhow::Result<HashSet<(String, i32, i32)>> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/chunks/indexed", self.base_url);
        let body: IndexedChunkKeys = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .context("requesting indexed chunk keys")?
            .error_for_status()
            .context("control plane rejected the indexed-chunk lookup")?
            .json()
            .await
            .context("parsing indexed chunk keys")?;
        Ok(body
            .keys
            .into_iter()
            .map(|k| (k.file_path, k.start_line, k.end_line))
            .collect())
    }

    /// `POST /internal/tasks/{id}/chunks` — submit a batch of indexed code chunks.
    pub async fn submit_chunks(&self, task_id: Uuid, batch: ChunkBatch) -> anyhow::Result<()> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/chunks", self.base_url);
        self.http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&batch)
            .send()
            .await
            .context("submitting chunks")?
            .error_for_status()
            .context("control plane rejected chunk batch")?;
        Ok(())
    }

    /// Submit a structural graph as a sequence of bounded pages.
    ///
    /// Every node page is sent before any edge page: an edge is written by matching both of its
    /// endpoints, and a match that finds nothing is a silent no-op, so an edge that arrives ahead of
    /// its endpoints is dropped without error.
    pub async fn submit_graph_paged(
        &self,
        task_id: Uuid,
        commit_sha: &str,
        nodes: &[GraphNodePayload],
        edges: &[GraphEdgePayload],
        page_size: usize,
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        let page_size = page_size.max(1);

        for (page, chunk) in nodes.chunks(page_size).enumerate() {
            self.submit_graph(
                task_id,
                GraphBatch {
                    commit_sha: commit_sha.to_string(),
                    nodes: chunk.to_vec(),
                    edges: Vec::new(),
                },
            )
            .await
            .with_context(|| format!("submitting graph node page {page}"))?;
        }

        for (page, chunk) in edges.chunks(page_size).enumerate() {
            self.submit_graph(
                task_id,
                GraphBatch {
                    commit_sha: commit_sha.to_string(),
                    nodes: Vec::new(),
                    edges: chunk.to_vec(),
                },
            )
            .await
            .with_context(|| format!("submitting graph edge page {page}"))?;
        }
        Ok(())
    }

    /// `DELETE /internal/tasks/{id}/graph` — discard this task's commit snapshot from the graph.
    ///
    /// Returns the snapshot to "not indexed" after a page sequence stops partway, so readers see an
    /// absent graph rather than a subset of one.
    pub async fn discard_graph(&self, task_id: Uuid) -> anyhow::Result<()> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/graph", self.base_url);
        self.http
            .delete(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .context("discarding graph")?
            .error_for_status()
            .context("control plane rejected the graph discard")?;
        Ok(())
    }

    /// `POST /internal/tasks/{id}/graph` — submit the structural code graph (lci-codegraph → Neo4j).
    pub async fn submit_graph(&self, task_id: Uuid, batch: GraphBatch) -> anyhow::Result<()> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/graph", self.base_url);
        self.http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&batch)
            .send()
            .await
            .context("submitting graph")?
            .error_for_status()
            .context("control plane rejected graph batch")?;
        Ok(())
    }
}
