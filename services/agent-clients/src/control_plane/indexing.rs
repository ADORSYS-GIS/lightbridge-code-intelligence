//! Ingest submission: indexed code chunks (for pgvector search) and the structural code graph (for
//! Neo4j via lci-codegraph).

use serde::Serialize;
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
}

/// Body for `POST /internal/tasks/{id}/chunks`.
#[derive(Debug, Serialize)]
pub struct ChunkBatch {
    pub commit_sha: String,
    pub chunks: Vec<ChunkPayload>,
}

/// One structural-graph node (mirrors `internal.rs::GraphNodeInput`).
#[derive(Debug, Serialize)]
pub struct GraphNodePayload {
    pub node_id: String,
    pub label: String,
    pub source_file: String,
    pub start_line: i64,
    /// Embedding of the symbol's definition text (ADR-0114), when a correlated chunk was found.
    /// `None` for a symbol kind the chunker doesn't produce a chunk for — it still gets structural
    /// edges, it just won't surface from a semantic search until a chunk-producing edit touches it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
}

/// One directed edge (`contains` / `method` / `calls` / …).
#[derive(Debug, Serialize)]
pub struct GraphEdgePayload {
    pub source: String,
    pub target: String,
    pub relation: String,
}

/// Body for `POST /internal/tasks/{id}/graph`.
#[derive(Debug, Serialize)]
pub struct GraphBatch {
    pub commit_sha: String,
    pub nodes: Vec<GraphNodePayload>,
    pub edges: Vec<GraphEdgePayload>,
}

impl ControlPlaneClient {
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
