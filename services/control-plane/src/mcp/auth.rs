use crate::{
    AppState,
    jwt::{AuthError, Caller},
};
use axum::{
    extract::State,
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

/// OIDC auth middleware for MCP.
pub async fn mcp_auth(
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

    let permissions = claims.permissions(&state.permissions_claim);
    let caller = Caller {
        claims,
        permissions,
    };

    // Also keep the header for debugging or logging if needed
    req.headers_mut().insert(
        "x-lb-mcp-caller",
        HeaderValue::from_str(&caller.claims.sub).unwrap_or(HeaderValue::from_static("unknown")),
    );

    // Inject caller into request for downstream MCP handlers via request extensions
    req.extensions_mut().insert(caller);

    next.run(req).await
}
