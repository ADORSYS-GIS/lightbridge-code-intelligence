use std::sync::Arc;

use lci_agent_tools::{
    BoxFuture, ReadKind, RegistryError, ReplaySafety, RuntimeCaps, Tool, ToolCx, ToolKind,
    ToolRegistry,
};
use lci_agent_types::{ToolCallReq, ToolOutcome, ToolSpec};
use serde::Deserialize;

use super::{ReviewServices, clamp_limit, parse, render};

pub const VECTOR_SEMANTIC_SEARCH: &str = "lightbridge_vector_semantic_search";

#[derive(Deserialize)]
struct Args {
    query: String,
    #[serde(default)]
    limit: Option<i64>,
}

pub fn spec() -> ToolSpec {
    ToolSpec::function(
        VECTOR_SEMANTIC_SEARCH,
        "Semantic search over the repository's indexed code by meaning (pgvector). Returns the most similar code chunks with file path, line range, and score.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Natural-language or code query." },
                "limit": { "type": "integer", "description": "Maximum number of results (default 10, max 100)." }
            },
            "required": ["query"]
        }),
    )
}

pub struct VectorTool {
    spec: ToolSpec,
    services: ReviewServices,
}

pub(crate) fn register(
    registry: &mut ToolRegistry,
    services: &ReviewServices,
    caps: RuntimeCaps,
) -> Result<(), RegistryError> {
    registry.register(
        Arc::new(VectorTool {
            spec: spec(),
            services: services.clone(),
        }),
        caps,
    )
}

impl Tool for VectorTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly(ReadKind::Retrieval)
    }
    fn replay(&self) -> ReplaySafety {
        ReplaySafety::ReadOnly
    }

    fn call<'a>(&'a self, cx: &'a ToolCx<'a>, call: &'a ToolCallReq) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let args = match parse::<Args>(&call.function.arguments) {
                Ok(args) => args,
                Err(error) => return ToolOutcome::Continue(error.to_string()),
            };
            let result = async {
                let mut vectors = self.services.embedder.embed(&[&args.query]).await?;
                let embedding = vectors
                    .pop()
                    .ok_or_else(|| anyhow::anyhow!("embeddings API returned no vector"))?;
                self.services
                    .client
                    .search(cx.task_id, &embedding, clamp_limit(args.limit))
                    .await
            }
            .await;
            ToolOutcome::Continue(render(VECTOR_SEMANTIC_SEARCH, result))
        })
    }
}
