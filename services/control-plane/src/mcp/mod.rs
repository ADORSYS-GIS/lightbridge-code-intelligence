use axum::{
    Router,
    extract::State,
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
};

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

async fn mcp_sse_handler(State(_state): State<AppState>) -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "SSE not yet implemented")
}

async fn mcp_message_handler(State(_state): State<AppState>, _body: String) -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        "Message routing not yet implemented",
    )
}

pub fn mcp_router(state: AppState) -> Router {
    let sse = get(mcp_sse_handler);
    let message = post(mcp_message_handler);

    Router::new()
        .route("/mcp", sse)
        .route("/mcp/message", message)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            mcp_auth,
        ))
        .with_state(state)
}
