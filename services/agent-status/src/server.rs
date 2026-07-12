//! The read-only HTTP surface for the [`StatusHandle`] projection state.
//!
//! A tiny axum app the run-once host runs **alongside** the review loop when the operator opts in
//! (`LCI_STATUS_API`). It exposes:
//!
//! - `GET /status` — the [`StatusSnapshot`](crate::StatusSnapshot) as JSON. **Bearer-authenticated**
//!   (the runner token, or a dedicated read token) — a missing/wrong token is `401`, never leaking
//!   even the progress metadata.
//! - `GET /healthz` — an unauthenticated liveness probe (`{"ok":true}`), carrying no task data.
//!
//! Flag-gating lives in the caller: build a [`StatusServerConfig`] only when enabled (see
//! [`StatusServerConfig::from_env`]) and [`spawn`] the server. Unset flag → no server is started, so
//! the port is never bound and the feature is prod-neutral and dormant. Whether or not the server runs,
//! the loop behaves identically.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};

use crate::StatusHandle;

/// Default TCP port for the status server when `LCI_STATUS_PORT` is unset. Chosen away from the app's
/// data ports; a NetworkPolicy/Service to actually reach it is deploy-side and out of scope here.
pub const DEFAULT_STATUS_PORT: u16 = 8091;

/// Configuration for the read-only status server. Built by the host only when the feature is enabled.
///
/// - `bind_addr` — the IP to bind (default `0.0.0.0` so a sibling container/Service can reach it).
/// - `port` — the TCP port ([`DEFAULT_STATUS_PORT`] by default).
/// - `bearer_token` — the token `GET /status` requires; a dedicated read token, else the runner token.
#[derive(Clone, Debug, bon::Builder)]
pub struct StatusServerConfig {
    pub bind_addr: IpAddr,
    pub port: u16,
    pub bearer_token: String,
}

impl StatusServerConfig {
    /// Resolve the config from the environment, returning `None` when the feature is **off** (the
    /// default), so the caller starts no server at all:
    ///
    /// - `LCI_STATUS_API` — must be truthy (`1`/`true`/`yes`, case-insensitive) to enable; unset or
    ///   anything else ⇒ `None` (prod-neutral, dormant).
    /// - `LCI_STATUS_PORT` — the port; falls back to [`DEFAULT_STATUS_PORT`] when unset/unparseable.
    /// - `LCI_STATUS_BIND` — the bind IP; falls back to `0.0.0.0` when unset/unparseable.
    /// - `LCI_STATUS_TOKEN` — a dedicated read token; falls back to `runner_token` when unset/blank.
    #[must_use]
    pub fn from_env(runner_token: &str) -> Option<Self> {
        if !truthy(std::env::var("LCI_STATUS_API").ok().as_deref()) {
            return None;
        }
        let port = std::env::var("LCI_STATUS_PORT")
            .ok()
            .and_then(|raw| raw.trim().parse::<u16>().ok())
            .unwrap_or(DEFAULT_STATUS_PORT);
        let bind_addr = std::env::var("LCI_STATUS_BIND")
            .ok()
            .and_then(|raw| raw.trim().parse::<IpAddr>().ok())
            .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        let bearer_token = std::env::var("LCI_STATUS_TOKEN")
            .ok()
            .map(|raw| raw.trim().to_string())
            .filter(|token| !token.is_empty())
            .unwrap_or_else(|| runner_token.to_string());
        Some(
            Self::builder()
                .bind_addr(bind_addr)
                .port(port)
                .bearer_token(bearer_token)
                .build(),
        )
    }

    fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.bind_addr, self.port)
    }
}

/// Whether an env value is a truthy opt-in token.
fn truthy(value: Option<&str>) -> bool {
    matches!(
        value.map(|raw| raw.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes")
    )
}

/// Shared axum state: the projection handle + the bearer token `GET /status` requires.
#[derive(Clone)]
struct AppState {
    handle: StatusHandle,
    token: Arc<String>,
}

/// Build the router (extracted so it can be exercised in-process, without binding a port).
fn router(handle: StatusHandle, token: String) -> Router {
    Router::new()
        .route("/status", get(status_handler))
        .route("/healthz", get(health_handler))
        .with_state(AppState {
            handle,
            token: Arc::new(token),
        })
}

/// Bind and serve the status app until the process exits (or the future is dropped/aborted). Returns
/// the bind error if the port can't be claimed.
pub async fn serve(handle: StatusHandle, config: StatusServerConfig) -> std::io::Result<()> {
    let addr = config.socket_addr();
    let app = router(handle, config.bearer_token);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "live status API listening (read-only, bearer-authenticated)");
    axum::serve(listener, app).await
}

/// Spawn [`serve`] on the current runtime, logging (never propagating) a bind/serve failure — the
/// status server is best-effort and must never take down the run it observes. Returns the task handle
/// so the host can abort it on shutdown.
#[must_use]
pub fn spawn(handle: StatusHandle, config: StatusServerConfig) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = serve(handle, config).await {
            tracing::warn!(%error, "live status API failed to start (non-fatal); continuing without it");
        }
    })
}

/// `GET /status` — the JSON snapshot, gated on the bearer token.
async fn status_handler(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&headers, &state.token) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    Json(state.handle.snapshot()).into_response()
}

/// `GET /healthz` — unauthenticated liveness, no task data.
async fn health_handler() -> Response {
    Json(serde_json::json!({ "ok": true })).into_response()
}

/// Whether the request carries `Authorization: Bearer <token>` matching the configured token.
fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .is_some_and(|presented| presented == expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt; // oneshot
    use uuid::Uuid;

    fn bearer(token: &str) -> String {
        format!("Bearer {token}")
    }

    // One test, run serially, for the whole env-gating contract: process-wide env vars race across
    // parallel tests, so off-by-default and the enabled fallbacks live in a single test that owns the
    // vars start to finish.
    #[test]
    fn from_env_gates_on_the_flag_and_falls_back_to_the_runner_token() {
        unsafe {
            std::env::remove_var("LCI_STATUS_API");
            std::env::remove_var("LCI_STATUS_PORT");
            std::env::remove_var("LCI_STATUS_BIND");
            std::env::remove_var("LCI_STATUS_TOKEN");
        }
        // Off by default: no flag ⇒ no config ⇒ no server.
        assert!(StatusServerConfig::from_env("runner-token").is_none());

        // Enabled with nothing else set ⇒ default port/bind + the runner token.
        unsafe {
            std::env::set_var("LCI_STATUS_API", "true");
        }
        let config = StatusServerConfig::from_env("runner-token").expect("enabled");
        assert_eq!(config.port, DEFAULT_STATUS_PORT);
        assert_eq!(config.bearer_token, "runner-token");
        assert_eq!(config.bind_addr, IpAddr::V4(Ipv4Addr::UNSPECIFIED));

        // A dedicated read token overrides the runner token.
        unsafe {
            std::env::set_var("LCI_STATUS_TOKEN", "read-only-token");
        }
        assert_eq!(
            StatusServerConfig::from_env("runner-token")
                .expect("enabled")
                .bearer_token,
            "read-only-token"
        );
        unsafe {
            std::env::remove_var("LCI_STATUS_API");
            std::env::remove_var("LCI_STATUS_TOKEN");
        }
    }

    #[tokio::test]
    async fn status_requires_a_valid_bearer() {
        let handle = StatusHandle::new(Uuid::nil());
        let app = router(handle, "secret".to_string());

        // No header ⇒ 401.
        let unauthorized = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        // Wrong token ⇒ 401.
        let wrong = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .header(header::AUTHORIZATION, bearer("nope"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

        // Correct token ⇒ 200 + JSON snapshot.
        let ok = app
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .header(header::AUTHORIZATION, bearer("secret"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        let body = axum::body::to_bytes(ok.into_body(), 64 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["task_id"], Uuid::nil().to_string());
        assert_eq!(value["phase"], "starting");
    }

    #[tokio::test]
    async fn healthz_is_unauthenticated_and_carries_no_task_data() {
        let handle = StatusHandle::new(Uuid::nil());
        let app = router(handle, "secret".to_string());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value, serde_json::json!({ "ok": true }));
    }
}
