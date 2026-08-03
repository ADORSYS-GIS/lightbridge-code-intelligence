use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
};

use crate::{AppState, jwt::AuthError};

/// The repository context extracted from the URL path.
#[derive(Clone, Debug)]
pub struct RepoContext {
    pub platform: String,
    pub org: String,
    pub repo: String,
}

impl RepoContext {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.org, self.repo)
    }
}

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
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::to_string);

    let Some(token) = token else {
        return (StatusCode::UNAUTHORIZED, "missing bearer token").into_response();
    };

    let jwt = match state.jwt.as_ref() {
        Some(j) => j,
        None => {
            return (StatusCode::SERVICE_UNAVAILABLE, "OIDC validation disabled").into_response();
        }
    };

    let claims = match jwt.validate(&token).await {
        Ok(claims) => claims,
        Err(err @ AuthError::JwksUnavailable) => return err.into_response(),
        Err(_) => return (StatusCode::UNAUTHORIZED, "invalid token").into_response(),
    };

    let caller_hv = match HeaderValue::from_str(&claims.sub) {
        Ok(v) => v,
        Err(_) => return (StatusCode::UNAUTHORIZED, "unrepresentable identity").into_response(),
    };

    req.headers_mut().insert("x-lb-mcp-caller", caller_hv);

    next.run(req).await
}

async fn mcp_sse_handler(
    Path((_platform, _org, _repo)): Path<(String, String, String)>,
    State(_state): State<AppState>,
) -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "SSE not yet implemented")
}

async fn mcp_message_handler(
    Path((_platform, _org, _repo)): Path<(String, String, String)>,
    State(_state): State<AppState>,
    _body: String,
) -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        "Message routing not yet implemented",
    )
}

pub fn mcp_router(state: AppState) -> Router {
    let sse = get(mcp_sse_handler);
    let message = post(mcp_message_handler);

    Router::new()
        .route("/mcp/:platform/:org/:repo", sse)
        .route("/mcp/:platform/:org/:repo/message", message)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            mcp_auth,
        ))
        .with_state(state)
}
