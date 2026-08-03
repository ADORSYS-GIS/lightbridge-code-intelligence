use crate::AppState;
use super::auth::McpCallerContext;
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo},
    service::RequestContext,
    tool_handler,
};

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

#[tool_handler(router = self.tool_router)]
impl ServerHandler for LightbridgeMcpHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "MCP interface for Lightbridge Code Intelligence",
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.tool_router.list_all()))
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<rmcp::model::CallToolResponse, ErrorData> {
        let tool = request.name.clone();
        
        let caller = caller_from_request_context(&context)?;
        
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        let result = self.tool_router.call(tcc).await;
        
        let outcome = match &result {
            Ok(rmcp::model::CallToolResponse::Complete(call_result))
                if call_result.is_error.unwrap_or(false) =>
            {
                "error"
            }
            Ok(_) => "ok",
            Err(_) => "error",
        };
        
        tracing::info!(tool = %tool, subject = %caller.sub, outcome, "mcp tool invoked");
        result
    }
}
