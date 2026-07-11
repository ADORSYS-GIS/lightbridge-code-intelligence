//! HTTP clients shared by Lightbridge agent hosts.

mod control_plane;
mod embeddings;
pub mod ratelimit;

pub use control_plane::{
    ChunkBatch, ChunkHit, ChunkPayload, ControlPlaneClient, DiscoveredTool, GraphBatch,
    GraphEdgePayload, GraphNodePayload, KnowledgeToolResult, SymbolHit, TaskContext,
    TranscriptEntry,
};
pub use embeddings::EmbeddingsClient;
