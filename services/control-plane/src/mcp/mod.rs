pub mod auth;
pub mod handler;
pub mod tools;

use crate::AppState;
use axum::Router;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use std::sync::Arc;

const DEFAULT_BIND: &str = "0.0.0.0:8080";

fn bind_addr_from(value: Option<String>) -> String {
    match value {
        Some(v) if !v.trim().is_empty() => v,
        _ => DEFAULT_BIND.to_string(),
    }
}

fn bind_addr() -> String {
    bind_addr_from(std::env::var("MCP_BIND").ok())
}

pub async fn run(state: AppState) -> anyhow::Result<()> {
    // The MCP role requires a database and OIDC config.
    if state.db.is_none() {
        anyhow::bail!("the mcp role requires DATABASE_URL");
    }
    if state.jwt.is_none() {
        anyhow::bail!("the mcp role requires OIDC_ISSUER (no anonymous access)");
    }

    crate::spawn_metrics_server(state.metrics.clone());

    let addr = bind_addr();

    // Wire up the RMCP Streamable HTTP service with our ServerHandler.
    let service: StreamableHttpService<handler::LightbridgeMcpHandler, LocalSessionManager> =
        StreamableHttpService::new(
            {
                let state = state.clone();
                move || Ok(handler::LightbridgeMcpHandler::new(state.clone()))
            },
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default().disable_allowed_hosts(), // Allow Traefik/Ingress external hosts
        );

    // Provide the service on the root path since Traefik strips the /mcp prefix.
    let router = Router::new()
        .nest_service("/", service)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::mcp_auth,
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(addr = %addr, "mcp role listening");
    axum::serve(listener, router).await?;
    Ok(())
}
