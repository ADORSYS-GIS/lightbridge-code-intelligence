pub mod auth;
pub mod handler;
pub mod metadata;
pub mod tools;

use crate::AppState;
use axum::Router;
use axum::routing::get;
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
    quota_from(|name| std::env::var(name).ok())
}

/// Injectable core of [`quota_from_env`] (mirrors `a2a::quota_from`) — takes a lookup function
/// instead of reading `std::env` directly, so the defaulting/clamping logic is testable without
/// mutating real process env vars.
fn quota_from(env: impl Fn(&str) -> Option<String>) -> McpQuotaConfig {
    let env_i64 =
        |name: &str, default: i64| env(name).and_then(|v| v.parse().ok()).unwrap_or(default);
    let max = env_i64("MCP_QUOTA_MAX", 20).max(1);
    let window_secs = env_i64("MCP_QUOTA_WINDOW_SECS", 3600).max(1);
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

    // Provide the service on the root path since Traefik strips the /mcp prefix. axum 0.8 no longer
    // allows `nest_service("/", ...)` (panics: "Nesting at the root is no longer supported") —
    // `fallback_service` is the documented replacement for mounting a service at the root.
    //
    // The RFC 9728 discovery document is merged OUTSIDE that auth layer — it must be readable without
    // credentials or it cannot serve its purpose, the same reason `a2a` mounts its agent card outside
    // `a2a_auth`. It is the role's only unauthenticated route; everything else stays gated.
    let protected =
        Router::new()
            .fallback_service(service)
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                auth::mcp_auth,
            ));
    let router = Router::new()
        .route(
            metadata::METADATA_PATH,
            get(metadata::protected_resource_metadata),
        )
        .merge(protected)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(addr = %addr, "mcp role listening");
    axum::serve(listener, router).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_addr_falls_back_on_unset_or_blank() {
        assert_eq!(bind_addr_from(None), DEFAULT_BIND);
        assert_eq!(bind_addr_from(Some("   ".to_string())), DEFAULT_BIND);
        assert_eq!(
            bind_addr_from(Some("0.0.0.0:9999".to_string())),
            "0.0.0.0:9999"
        );
    }

    #[test]
    fn quota_defaults_and_clamps() {
        let quota = |max: Option<&str>, window: Option<&str>| {
            quota_from(|name| match name {
                "MCP_QUOTA_MAX" => max.map(str::to_string),
                "MCP_QUOTA_WINDOW_SECS" => window.map(str::to_string),
                _ => None,
            })
        };

        let q = quota(None, None);
        assert_eq!((q.max, q.window_secs), (20, 3600));

        let q = quota(Some("0"), Some("-5"));
        assert_eq!(
            (q.max, q.window_secs),
            (1, 1),
            "a non-positive max/window is clamped to 1 (never unbounded / never a no-op window)"
        );

        let q = quota(Some("7"), Some("60"));
        assert_eq!((q.max, q.window_secs), (7, 60));
    }
}
