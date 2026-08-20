use std::sync::Arc;

use lci_agent_tools::{
    BoxFuture, ReadKind, RegistryError, ReplaySafety, RuntimeCaps, Tool, ToolCx, ToolKind,
    ToolRegistry,
};
use lci_agent_types::{ToolCallReq, ToolOutcome, ToolSpec};
use serde::Deserialize;

use super::{ReviewServices, clamp_limit, parse, render};

pub const GRAPH_FIND_SYMBOL: &str = "lightbridge_graph_find_symbol";
pub const GRAPH_GET_CALLERS: &str = "lightbridge_graph_get_callers";
pub const GRAPH_SEMANTIC_SEARCH: &str = "lightbridge_graph_semantic_search";

#[derive(Deserialize)]
struct FindArgs {
    term: String,
    #[serde(default)]
    limit: Option<i64>,
}
#[derive(Deserialize)]
struct CallersArgs {
    node_id: String,
    #[serde(default)]
    limit: Option<i64>,
}
#[derive(Deserialize)]
struct SemanticSearchArgs {
    query: String,
    #[serde(default)]
    limit: Option<i64>,
}

pub fn specs() -> [ToolSpec; 3] {
    let limit = serde_json::json!({"type":"integer","description":"Maximum number of results (default 10, max 100)."});
    [
        ToolSpec::function(
            GRAPH_FIND_SYMBOL,
            "Find symbols (functions, classes, methods) by name, node id, or file-path substring. Returns matching nodes with their node id, label, and location.",
            serde_json::json!({"type":"object","properties":{"term":{"type":"string","description":"Symbol name / node id / file path substring (case-insensitive)."},"limit":limit},"required":["term"]}),
        ),
        ToolSpec::function(
            GRAPH_GET_CALLERS,
            "Return the symbols that call a given symbol (reverse call graph). Pass a node id from graph_find_symbol.",
            serde_json::json!({"type":"object","properties":{"node_id":{"type":"string","description":"Node id of the target symbol (from graph_find_symbol)."},"limit":limit},"required":["node_id"]}),
        ),
        ToolSpec::function(
            GRAPH_SEMANTIC_SEARCH,
            "Find symbols by meaning, not just name — combines exact-name and semantic-similarity matching (ADR-0114). Use when you know what the code should DO but not what it's called.",
            serde_json::json!({"type":"object","properties":{"query":{"type":"string","description":"Natural-language description of the code you're looking for."},"limit":limit},"required":["query"]}),
        ),
    ]
}

struct FindTool {
    spec: ToolSpec,
    services: ReviewServices,
}
struct CallersTool {
    spec: ToolSpec,
    services: ReviewServices,
}
struct SemanticSearchTool {
    spec: ToolSpec,
    services: ReviewServices,
}

pub(crate) fn register(
    registry: &mut ToolRegistry,
    services: &ReviewServices,
    caps: RuntimeCaps,
) -> Result<(), RegistryError> {
    let [find, callers, semantic_search] = specs();
    registry.register(
        Arc::new(FindTool {
            spec: find,
            services: services.clone(),
        }),
        caps,
    )?;
    registry.register(
        Arc::new(CallersTool {
            spec: callers,
            services: services.clone(),
        }),
        caps,
    )?;
    registry.register(
        Arc::new(SemanticSearchTool {
            spec: semantic_search,
            services: services.clone(),
        }),
        caps,
    )
}

impl Tool for FindTool {
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
            match parse::<FindArgs>(&call.function.arguments) {
                Ok(args) => ToolOutcome::Continue(render(
                    GRAPH_FIND_SYMBOL,
                    self.services
                        .client
                        .graph_find_symbol(cx.task_id, &args.term, clamp_limit(args.limit))
                        .await,
                )),
                Err(error) => ToolOutcome::Continue(error.to_string()),
            }
        })
    }
}

impl Tool for CallersTool {
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
            match parse::<CallersArgs>(&call.function.arguments) {
                Ok(args) => ToolOutcome::Continue(render(
                    GRAPH_GET_CALLERS,
                    self.services
                        .client
                        .graph_get_callers(cx.task_id, &args.node_id, clamp_limit(args.limit))
                        .await,
                )),
                Err(error) => ToolOutcome::Continue(error.to_string()),
            }
        })
    }
}

impl Tool for SemanticSearchTool {
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
            match parse::<SemanticSearchArgs>(&call.function.arguments) {
                Ok(args) => {
                    // Embed then search, sequenced into one Result so `render` reports either
                    // failure the same way it already reports a graph-query failure — the model
                    // doesn't need to know embedding is a separate step.
                    let result = async {
                        let mut embeddings =
                            self.services.embedder.embed(&[args.query.as_str()]).await?;
                        let embedding = embeddings
                            .pop()
                            .ok_or_else(|| anyhow::anyhow!("embedder returned no vector"))?;
                        self.services
                            .client
                            .graph_semantic_search(
                                cx.task_id,
                                &args.query,
                                &embedding,
                                clamp_limit(args.limit),
                            )
                            .await
                    }
                    .await;
                    ToolOutcome::Continue(render(GRAPH_SEMANTIC_SEARCH, result))
                }
                Err(error) => ToolOutcome::Continue(error.to_string()),
            }
        })
    }
}
