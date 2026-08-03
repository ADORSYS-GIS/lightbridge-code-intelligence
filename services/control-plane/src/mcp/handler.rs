use super::auth::McpCallerContext;
use crate::AppState;
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct LightbridgeMcpHandler {
    #[allow(dead_code)]
    state: AppState,
    tool_router: ToolRouter<Self>,
}

impl LightbridgeMcpHandler {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }
}

pub(crate) fn caller_from_request_context(
    context: &RequestContext<RoleServer>,
) -> std::result::Result<McpCallerContext, ErrorData> {
    let parts = context
        .extensions
        .get::<axum::http::request::Parts>()
        .ok_or_else(|| ErrorData::internal_error("missing HTTP request context", None))?;

    let caller = parts
        .extensions
        .get::<McpCallerContext>()
        .ok_or_else(|| ErrorData::internal_error("missing caller context", None))?;

    Ok(caller.clone())
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct VectorSearchArgs {
    pub platform: String,
    pub org: String,
    pub repo: String,
    pub query: String,
    pub limit: Option<usize>,
}

#[tool_router]
impl LightbridgeMcpHandler {
    #[tool(
        name = "vector_search",
        description = "Search vector index across the repository"
    )]
    async fn vector_search_tool(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(args): Parameters<VectorSearchArgs>,
    ) -> std::result::Result<String, ErrorData> {
        let caller = caller_from_request_context(&context)?;

        // Placeholder for actual vector search DB call.
        // Needs proper integration with pgvector and access checks.
        Ok(format!(
            "Vector search executed on {}/{}/{} with query: {} (caller: {})",
            args.platform, args.org, args.repo, args.query, caller.sub
        ))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for LightbridgeMcpHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("MCP interface for Lightbridge Code Intelligence")
    }
}
