pub mod auth;
pub mod handler;
pub mod tools;

use crate::AppState;
use axum::Router;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use std::sync::Arc;

pub async fn run(state: AppState) -> anyhow::Result<()> {
    // The MCP role requires a database and OIDC config, similar to A2A.
    if state.db.is_none() {
        anyhow::bail!("the mcp role requires DATABASE_URL");
    }
    if state.jwt.is_none() {
        anyhow::bail!("the mcp role requires OIDC_ISSUER (no anonymous access)");
    }

    crate::spawn_metrics_server(state.metrics.clone());

    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    
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
