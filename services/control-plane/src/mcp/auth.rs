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

/// A 401 carrying the RFC 9728 §5.1 `WWW-Authenticate` challenge. Without the `resource_metadata`
/// pointer a bare 401 is a dead end for an MCP client — nothing tells it which authorization server
/// to go to — so this is what makes the discovery document reachable in practice. Falls back to a
/// plain 401 when `MCP_PUBLIC_URL` is unset (no metadata document is served in that case either).
fn unauthorized(state: &AppState, detail: &'static str) -> Response {
    let challenge = state.mcp_public_url.as_deref().and_then(|base| {
        HeaderValue::from_str(&format!(
            r#"Bearer resource_metadata="{}{}""#,
            base.trim_end_matches('/'),
            super::metadata::METADATA_PATH
        ))
        .ok()
    });
    match challenge {
        Some(value) => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, value)],
            detail,
        )
            .into_response(),
        None => (StatusCode::UNAUTHORIZED, detail).into_response(),
    }
}

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
        None => return unauthorized(&state, "Missing Bearer token"),
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
        Err(_) => return unauthorized(&state, "invalid token"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jwt::{Claims, JwtValidator, OidcConfig};
    use axum::{Router, extract::Request, routing::get};
    use std::sync::Arc;

    /// A minimal `AppState` for exercising `mcp_auth` in isolation — no DB/Neo4j/platforms needed,
    /// since the middleware only ever touches `state.jwt` and `state.permissions_claim`. Mirrors
    /// `http::admin::tests::test_state`.
    fn test_state(jwt: Option<Arc<JwtValidator>>) -> AppState {
        AppState {
            github_webhook_secret: std::sync::Arc::new(String::new()),
            seen_deliveries: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
            jwt,
            db: None,
            allow_no_db: true,
            github: None,
            gitlab: None,
            bitbucket: None,
            platforms: std::collections::HashMap::new(),
            runner_token_signer: None,
            neo4j: None,
            metrics: crate::http::metrics::install(),
            review: std::sync::Arc::new(crate::config::ReviewSection::default()),
            knowledge_tools: std::sync::Arc::new(crate::config::KnowledgeToolsSection::default()),
            app_handle: std::sync::Arc::new("lightbridge-assistant".to_string()),
            permissions_claim: std::sync::Arc::new("permissions".to_string()),
            model_allowlist: std::sync::Arc::new(Vec::new()),
            mcp_public_url: None,
        }
    }

    /// `test_state` plus the public URL, so the RFC 9728 `WWW-Authenticate` challenge is emitted.
    fn test_state_with_public_url(jwt: Option<Arc<JwtValidator>>, url: &str) -> AppState {
        AppState {
            mcp_public_url: Some(Arc::new(url.to_string())),
            ..test_state(jwt)
        }
    }

    /// Echoes what the middleware put in the request's extensions, so the test can assert on the
    /// *verified* identity/permissions rather than anything a client could supply directly.
    async fn echo_caller(req: Request) -> String {
        match req.extensions().get::<Caller>() {
            Some(c) => {
                let mut perms: Vec<&str> = c.permissions.iter().map(String::as_str).collect();
                perms.sort_unstable();
                format!("{}|{}", c.claims.sub, perms.join(","))
            }
            None => "<none>".to_string(),
        }
    }

    async fn spawn(state: AppState) -> (std::net::SocketAddr, reqwest::Client) {
        let app = Router::new()
            .route("/probe", get(echo_caller))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                mcp_auth,
            ))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, reqwest::Client::new())
    }

    #[tokio::test]
    async fn missing_bearer_is_rejected() {
        let (addr, client) = spawn(test_state(Some(Arc::new(
            crate::jwt::test_support::validator(),
        ))))
        .await;
        let resp = client
            .get(format!("http://{addr}/probe"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // No MCP_PUBLIC_URL configured -> no discovery document is served, so the challenge would
        // point at a 404. Better to stay silent than to advertise a dead pointer.
        assert!(resp.headers().get(header::WWW_AUTHENTICATE).is_none());
    }

    /// RFC 9728 §5.1: the 401 is what tells a client where to look. Without this header the metadata
    /// document may as well not exist — nothing references it.
    #[tokio::test]
    async fn rejection_advertises_the_resource_metadata_when_public_url_is_set() {
        let (addr, client) = spawn(test_state_with_public_url(
            Some(Arc::new(crate::jwt::test_support::validator())),
            // Trailing slash on purpose: it must not double up in the advertised URL.
            "https://api.example/mcp/",
        ))
        .await;

        for (case, req) in [
            ("missing token", client.get(format!("http://{addr}/probe"))),
            (
                "invalid token",
                client
                    .get(format!("http://{addr}/probe"))
                    .header(header::AUTHORIZATION, "Bearer nope"),
            ),
        ] {
            let resp = req.send().await.unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{case}");
            assert_eq!(
                resp.headers()
                    .get(header::WWW_AUTHENTICATE)
                    .and_then(|v| v.to_str().ok()),
                Some(
                    r#"Bearer resource_metadata="https://api.example/mcp/.well-known/oauth-protected-resource""#
                ),
                "{case}"
            );
        }
    }

    #[tokio::test]
    async fn oidc_disabled_is_service_unavailable() {
        // `jwt: None` is the fail-closed state when OIDC_ISSUER is unset — never anonymous access.
        let (addr, client) = spawn(test_state(None)).await;
        let resp = client
            .get(format!("http://{addr}/probe"))
            .header(header::AUTHORIZATION, "Bearer whatever")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn invalid_token_is_rejected() {
        let (addr, client) = spawn(test_state(Some(Arc::new(
            crate::jwt::test_support::validator(),
        ))))
        .await;
        let resp = client
            .get(format!("http://{addr}/probe"))
            .header(header::AUTHORIZATION, "Bearer not-a-real-token")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// A JWKS fetch failure is a transient server condition, not a bad token — surfaces as a
    /// retryable 503, matching the `a2a` auth layer's identical handling of `JwksUnavailable`.
    #[tokio::test]
    async fn jwks_outage_is_service_unavailable_not_unauthorized() {
        let jwt = Arc::new(JwtValidator::from_static_jwks(
            OidcConfig {
                issuer: crate::jwt::test_support::ISSUER.to_string(),
                audience: crate::jwt::test_support::AUDIENCE.to_string(),
                jwks_uri: "http://unused.invalid".to_string(),
            },
            r#"{"keys":[]}"#,
        ));
        let (addr, client) = spawn(test_state(Some(jwt))).await;
        let token = crate::jwt::test_support::mint("svc-a", &["review:trigger"]);
        let resp = client
            .get(format!("http://{addr}/probe"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// The success path: a valid bearer's verified `sub`/permissions reach the downstream tool via
    /// request extensions — and a client-supplied `x-lb-mcp-caller` header can never override them,
    /// since the middleware always derives + (re)inserts it from the validated token.
    #[tokio::test]
    async fn valid_token_injects_verified_caller_and_ignores_spoofed_header() {
        let (addr, client) = spawn(test_state(Some(Arc::new(
            crate::jwt::test_support::validator(),
        ))))
        .await;
        let token =
            crate::jwt::test_support::mint("svc-account-9", &["review:trigger", "repo:read"]);
        let resp = client
            .get(format!("http://{addr}/probe"))
            .header("x-lb-mcp-caller", "admin")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.text().await.unwrap(),
            "svc-account-9|repo:read,review:trigger"
        );
    }

    #[test]
    fn caller_is_missing_permission_check_fails_closed() {
        // Sanity-check the primitive every `#[tool]` handler relies on: an authenticated caller
        // without the required permission is denied, not merely warned.
        let caller = Caller {
            claims: Claims {
                sub: "svc".to_string(),
                email: None,
                preferred_username: None,
                name: None,
                exp: 9_999_999_999,
                extra: serde_json::Map::new(),
            },
            permissions: ["repo:read".to_string()].into_iter().collect(),
        };
        assert!(caller.require("repo:read").is_ok());
        assert!(caller.require("review:trigger").is_err());
    }
}
