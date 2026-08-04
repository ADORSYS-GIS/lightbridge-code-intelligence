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

#[derive(Clone, Debug)]
pub struct McpQuotaConfig {
    pub max: i64,
    pub window_secs: i64,
}

fn quota_from_env() -> McpQuotaConfig {
    let max = std::env::var("MCP_QUOTA_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20)
        .max(1);
    let window_secs = std::env::var("MCP_QUOTA_WINDOW_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3600)
        .max(1);
    McpQuotaConfig { max, window_secs }
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

    let quota = quota_from_env();

    // Wire up the RMCP Streamable HTTP service with our ServerHandler.
    let service: StreamableHttpService<handler::LightbridgeMcpHandler, LocalSessionManager> =
        StreamableHttpService::new(
            {
                let state = state.clone();
                let quota = quota.clone();
                move || {
                    Ok(handler::LightbridgeMcpHandler::new(
                        state.clone(),
                        quota.clone(),
                    ))
                }
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
