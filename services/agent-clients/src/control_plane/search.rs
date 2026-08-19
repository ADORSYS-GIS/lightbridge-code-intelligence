//! Query side: semantic search over the task's pgvector index, and structural-graph queries
//! (find-symbol / get-callers) over the Neo4j graph.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::ControlPlaneClient;

/// A semantic-search hit (mirrors `db::CodeChunkHit`). Returned by [`ControlPlaneClient::search`].
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChunkHit {
    pub file_path: String,
    pub language: String,
    pub chunk_type: String,
    pub symbol_name: Option<String>,
    pub start_line: i32,
    pub end_line: i32,
    pub content: String,
    pub score: f64,
}

/// A structural-graph symbol (mirrors `neo4j::SymbolHit`). Returned by the graph queries.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SymbolHit {
    pub node_id: String,
    pub label: String,
    pub source_file: String,
    pub start_line: i64,
}

impl ControlPlaneClient {
    /// `POST /internal/tasks/{id}/search` — semantic search over the task's pgvector index. The
    /// caller passes the already-embedded query (the vector MCP embeds the text with the runner's
    /// embeddings key); the control plane scopes the search to the task's repo.
    pub async fn search(
        &self,
        task_id: Uuid,
        embedding: &[f32],
        limit: i64,
    ) -> anyhow::Result<Vec<ChunkHit>> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/search", self.base_url);
        let hits = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "embedding": embedding, "limit": limit }))
            .send()
            .await
            .context("semantic search request")?
            .error_for_status()
            .context("control plane rejected the search")?
            .json::<Vec<ChunkHit>>()
            .await
            .context("parsing search hits")?;
        Ok(hits)
    }

    /// `POST /internal/tasks/{id}/graph/query` with `op=find_symbol`.
    pub async fn graph_find_symbol(
        &self,
        task_id: Uuid,
        term: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<SymbolHit>> {
        self.graph_query(
            task_id,
            serde_json::json!({ "op": "find_symbol", "term": term, "limit": limit }),
        )
        .await
    }

    /// `POST /internal/tasks/{id}/graph/query` with `op=get_callers`.
    pub async fn graph_get_callers(
        &self,
        task_id: Uuid,
        node_id: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<SymbolHit>> {
        self.graph_query(
            task_id,
            serde_json::json!({ "op": "get_callers", "node_id": node_id, "limit": limit }),
        )
        .await
    }

    /// `POST /internal/tasks/{id}/graph/query` with `op=hybrid_search` (ADR-0114) — lexical + semantic
    /// symbol search, fused by weighted reciprocal rank. `embedding` must already be computed (this
    /// client never embeds anything itself); the caller mints it from its own `EmbeddingsClient`.
    pub async fn graph_semantic_search(
        &self,
        task_id: Uuid,
        term: &str,
        embedding: &[f32],
        limit: i64,
    ) -> anyhow::Result<Vec<SymbolHit>> {
        self.graph_query(
            task_id,
            serde_json::json!({ "op": "hybrid_search", "term": term, "embedding": embedding, "limit": limit }),
        )
        .await
    }

    async fn graph_query(
        &self,
        task_id: Uuid,
        body: serde_json::Value,
    ) -> anyhow::Result<Vec<SymbolHit>> {
        use anyhow::Context;
        let url = format!("{}/internal/tasks/{task_id}/graph/query", self.base_url);
        let hits = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .context("graph query request")?
            .error_for_status()
            .context("control plane rejected the graph query")?
            .json::<Vec<SymbolHit>>()
            .await
            .context("parsing graph hits")?;
        Ok(hits)
    }
}
