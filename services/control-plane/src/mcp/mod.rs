use axum::{
    Router,
    extract::State,
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{AppState, jwt::AuthError};

/// OIDC auth middleware for `/mcp` routes.
async fn mcp_auth(
    State(state): State<AppState>,
    mut req: axum::extract::Request,
    next: Next,
) -> Response {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    let token = match token {
        Some(t) => t,
        None => return (StatusCode::UNAUTHORIZED, "Missing Bearer token").into_response(),
    };

    let jwt = match state.jwt.as_ref() {
        Some(j) => j,
        None => {
            return (StatusCode::SERVICE_UNAVAILABLE, "OIDC validation disabled").into_response();
        }
    };

    let claims = match jwt.validate(token).await {
        Ok(c) => c,
        Err(err @ AuthError::JwksUnavailable) => return err.into_response(),
        Err(_) => return (StatusCode::UNAUTHORIZED, "invalid token").into_response(),
    };

    // Inject caller into request for downstream MCP handlers
    req.headers_mut().insert(
        "x-lb-mcp-caller",
        HeaderValue::from_str(&claims.sub).unwrap_or(HeaderValue::from_static("unknown")),
    );

    next.run(req).await
}

#[derive(Clone)]
struct McpServer {
    state: AppState,
    tool_router: ToolRouter<Self>,
}

impl McpServer {
    fn new(state: AppState) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct VectorSearchArgs {
    platform: String,
    org: String,
    repo: String,
    query: String,
    limit: Option<usize>,
}

#[tool_router]
impl McpServer {
    #[tool(description = "Search vector index across the repository")]
    async fn vector_search(
        &self,
        Parameters(args): Parameters<VectorSearchArgs>,
    ) -> Result<String, rmcp::ErrorData> {
        // Placeholder for actual vector search DB call
        Ok(format!(
            "Vector search executed on {}/{}/{} with query: {}",
            args.platform, args.org, args.repo, args.query
        ))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }
}

pub fn mcp_router(state: AppState) -> Router {
    let service: StreamableHttpService<McpServer, LocalSessionManager> = StreamableHttpService::new(
        {
            let state = state.clone();
            move || Ok(McpServer::new(state.clone()))
        },
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default().disable_allowed_hosts(), // Allow Traefik/Ingress external hosts
    );

    Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            mcp_auth,
        ))
        .with_state(state)
}
