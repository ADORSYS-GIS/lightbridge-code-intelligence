//! The `a2a` role (RFC-0006 Phase 1, ticket #299): an A2A v1.0.1 server surface exposing the
//! `review` skill over polling (`SendMessage` / `GetTask` / `CancelTask`), backed by the existing
//! Postgres task queue.
//!
//! ## Shape
//!
//! A fourth ingress face on the control plane (webhook, admin, internal, **A2A**) — not new
//! execution (ADR-0029 holds; a skill is a named entry point to existing behaviour). It:
//!
//! - serves the public agent card at `/.well-known/agent-card.json` ([`card`]);
//! - serves JSON-RPC (`POST /`) + REST (`/message:send`, `/tasks/{id}`, …) via the SDK bindings,
//!   behind an OIDC auth layer ([`a2a_auth`]) that validates the bearer once and injects the
//!   verified caller identity for the [`handler`];
//! - creates deep-tier review tasks through the SAME path as the webhook handler
//!   ([`crate::db::create_task`]), enforcing the repo-approval gate and a per-identity quota, and
//!   answering an unapproved/unauthorized/over-quota submission with `TASK_STATE_REJECTED`;
//! - holds **no forge credentials** — egress stays on the reconciler/Restate path.
//!
//! The role's k8s Deployment/Ingress is an ai-helm follow-up; this module adds the role + logic.

mod card;
mod handler;
mod mapping;
mod store;

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Router;

use crate::jwt::JwtValidator;
use crate::AppState;

pub use handler::{A2aHandler, QuotaConfig};

/// Internal request header carrying the authenticated caller's stable identity (OIDC `sub`). Set by
/// [`a2a_auth`] after token validation and read by the handler via `ServiceParams`. Any inbound copy
/// is stripped first, so a client cannot spoof it.
pub(crate) const HDR_CALLER: &str = "x-lb-a2a-caller";
/// Internal request header carrying the caller's comma-joined permissions (ADR-0023). Same
/// strip-then-inject discipline as [`HDR_CALLER`].
pub(crate) const HDR_PERMS: &str = "x-lb-a2a-perms";

/// Max A2A request body. The peers are hostile-by-assumption (R9); a review submission is a small
/// JSON document, so a tight cap bounds the JSON-parse attack surface.
const MAX_BODY_BYTES: usize = 256 * 1024;

/// Default bind address for the A2A HTTP surface. Overridable via `A2A_BIND`.
const DEFAULT_BIND: &str = "0.0.0.0:8080";

/// Shared state for the auth layer: the token validator + the configured permissions claim path.
#[derive(Clone)]
struct A2aAuthState {
    jwt: Arc<JwtValidator>,
    permissions_claim: Arc<String>,
}

/// Resolve the per-identity deep-run quota from env (RFC-0006 R4): `A2A_QUOTA_MAX` submissions per
/// `A2A_QUOTA_WINDOW_SECS`. Defaults: 20 per hour. A non-positive max is clamped to 1 so the role
/// can never be configured into an unbounded state.
fn quota_from_env() -> QuotaConfig {
    let env_i64 = |name: &str, default: i64| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let max = env_i64("A2A_QUOTA_MAX", 20).max(1);
    let window_secs = env_i64("A2A_QUOTA_WINDOW_SECS", 3600).max(1);
    QuotaConfig { max, window_secs }
}

/// The externally reachable base URL advertised in the card's `supportedInterfaces` (`A2A_BASE_URL`).
/// The real Ingress host is an ai-helm concern; this is a config value, not a binding decision.
fn base_url_from_env() -> String {
    std::env::var("A2A_BASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "http://localhost:8080".to_string())
}

fn bind_addr() -> String {
    match std::env::var("A2A_BIND") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => DEFAULT_BIND.to_string(),
    }
}

/// Build the full A2A router: the public card, plus the OIDC-protected JSON-RPC + REST bindings.
///
/// Factored out of [`run`] so it is assemblable in tests without binding a socket. The auth layer
/// covers only the protected bindings — the agent card is public discovery (spec).
fn build_router(state: AppState, jwt: Arc<JwtValidator>) -> Router {
    let pool = state
        .db
        .clone()
        .expect("build_router requires a database (checked by run)");

    let handler = Arc::new(A2aHandler::new(pool, quota_from_env()));

    let issuer = std::env::var("OIDC_ISSUER").ok();
    let card = card::build_agent_card(
        &base_url_from_env(),
        &card::oidc_discovery_url(issuer.as_deref()),
    );
    let card_router =
        a2a_server::agent_card::agent_card_router(Arc::new(a2a_server::StaticAgentCard::new(card)));

    let auth_state = A2aAuthState {
        jwt,
        permissions_claim: state.permissions_claim.clone(),
    };
    let protected = a2a_server::rest::rest_router(handler.clone())
        .merge(a2a_server::jsonrpc::jsonrpc_router(handler))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::middleware::from_fn_with_state(auth_state, a2a_auth));

    Router::new().merge(card_router).merge(protected)
}

/// OIDC auth layer for the protected A2A bindings. Validates the `Authorization: Bearer` token once,
/// then injects the verified caller id + permissions as internal headers for the handler. Any inbound
/// copy of those internal headers is stripped first (anti-spoof). Authentication failures are 401
/// (missing/invalid token) or 503 (disabled) — authorization (per-skill permission) is enforced
/// downstream and surfaces as a `TASK_STATE_REJECTED` task, not a transport error (RFC-0006).
async fn a2a_auth(State(auth): State<A2aAuthState>, mut req: Request, next: Next) -> Response {
    // Strip any spoofed internal headers before we consider the request.
    let headers = req.headers_mut();
    headers.remove(HDR_CALLER);
    headers.remove(HDR_PERMS);

    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::to_string);
    let Some(token) = token else {
        return (StatusCode::UNAUTHORIZED, "missing bearer token").into_response();
    };

    let claims = match auth.jwt.validate(&token).await {
        Ok(claims) => claims,
        Err(_) => return (StatusCode::UNAUTHORIZED, "invalid token").into_response(),
    };

    let perms = claims.permissions(&auth.permissions_claim);
    let perms_joined = {
        let mut v: Vec<&str> = perms.iter().map(String::as_str).collect();
        v.sort_unstable();
        v.join(",")
    };
    let (Ok(caller_hv), Ok(perms_hv)) = (
        HeaderValue::from_str(&claims.sub),
        HeaderValue::from_str(&perms_joined),
    ) else {
        return (StatusCode::UNAUTHORIZED, "unrepresentable identity").into_response();
    };
    let headers = req.headers_mut();
    headers.insert(HDR_CALLER, caller_hv);
    headers.insert(HDR_PERMS, perms_hv);

    next.run(req).await
}

/// The `a2a` role entrypoint. Requires a database (the task queue) and OIDC (there is no anonymous
/// access). Serves the A2A surface on `A2A_BIND` and a metrics-only listener on `METRICS_ADDR`, like
/// the other headless roles.
pub async fn run(state: AppState) -> anyhow::Result<()> {
    if state.db.is_none() {
        anyhow::bail!("the a2a role requires DATABASE_URL (it is the task queue)");
    }
    let jwt = state.jwt.clone().ok_or_else(|| {
        anyhow::anyhow!("the a2a role requires OIDC_ISSUER (no anonymous access)")
    })?;

    crate::spawn_metrics_server(state.metrics.clone());

    let addr = bind_addr();
    let router = build_router(state, jwt);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(addr = %addr, "a2a role listening (A2A v1.0.1: review skill, polling)");
    axum::serve(listener, router).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_defaults_and_clamps() {
        std::env::remove_var("A2A_QUOTA_MAX");
        std::env::remove_var("A2A_QUOTA_WINDOW_SECS");
        let q = quota_from_env();
        assert_eq!(q.max, 20);
        assert_eq!(q.window_secs, 3600);

        std::env::set_var("A2A_QUOTA_MAX", "0");
        std::env::set_var("A2A_QUOTA_WINDOW_SECS", "-5");
        let q = quota_from_env();
        assert_eq!(
            q.max, 1,
            "a non-positive max is clamped to 1 (never unbounded)"
        );
        assert_eq!(q.window_secs, 1);

        std::env::set_var("A2A_QUOTA_MAX", "7");
        std::env::set_var("A2A_QUOTA_WINDOW_SECS", "60");
        let q = quota_from_env();
        assert_eq!((q.max, q.window_secs), (7, 60));

        std::env::remove_var("A2A_QUOTA_MAX");
        std::env::remove_var("A2A_QUOTA_WINDOW_SECS");
    }

    /// End-to-end over a real socket: the agent card is public, and the protected bindings are gated
    /// by the auth layer (401 without a valid bearer, and a spoofed identity header never grants
    /// access — it is stripped before validation). Uses an empty-JWKS validator so every token is
    /// invalid; the JWT validation itself is covered in `jwt.rs`.
    #[tokio::test]
    async fn card_is_public_and_bindings_require_auth() {
        use crate::jwt::{JwtValidator, OidcConfig};
        use axum::routing::get;

        let jwt = Arc::new(JwtValidator::from_static_jwks(
            OidcConfig {
                issuer: "https://idp.test/realms/lightbridge".to_string(),
                audience: "lightbridge-api".to_string(),
                jwks_uri: "http://unused.invalid".to_string(),
            },
            r#"{"keys":[]}"#,
        ));
        let auth_state = A2aAuthState {
            jwt,
            permissions_claim: Arc::new("permissions".to_string()),
        };
        let card = card::build_agent_card("http://localhost:8080", "http://kc/.well-known/x");
        let card_router = a2a_server::agent_card::agent_card_router(Arc::new(
            a2a_server::StaticAgentCard::new(card),
        ));
        let protected = Router::new()
            .route("/probe", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(auth_state, a2a_auth));
        let app = Router::new().merge(card_router).merge(protected);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let base = format!("http://{addr}");
        let client = reqwest::Client::new();

        // Public discovery: no auth, 200, and the review skill is advertised.
        let card = client
            .get(format!("{base}/.well-known/agent-card.json"))
            .send()
            .await
            .unwrap();
        assert_eq!(card.status(), 200);
        assert!(card.text().await.unwrap().contains("\"review\""));

        // Protected: no bearer → 401.
        let no_auth = client.get(format!("{base}/probe")).send().await.unwrap();
        assert_eq!(no_auth.status(), 401);

        // Spoofed identity header without a valid bearer → still 401 (the header is stripped and the
        // bad token rejected); a client can never inject its own identity.
        let spoof = client
            .get(format!("{base}/probe"))
            .header(HDR_CALLER, "admin")
            .header(header::AUTHORIZATION, "Bearer not-a-real-token")
            .send()
            .await
            .unwrap();
        assert_eq!(spoof.status(), 401);
    }

    /// The auth layer's success path: a valid bearer passes, and the downstream sees the identity +
    /// permissions the *token* carried — NOT a spoofed `x-lb-a2a-caller` header the client also sent
    /// (that is stripped before validation). This is the anti-spoof guarantee with a real token.
    #[tokio::test]
    async fn valid_token_injects_verified_identity_and_ignores_spoof() {
        use axum::extract::Request;
        use axum::routing::get;

        let auth_state = A2aAuthState {
            jwt: Arc::new(crate::jwt::test_support::validator()),
            permissions_claim: Arc::new("permissions".to_string()),
        };
        // The downstream echoes back what the middleware injected.
        async fn echo(req: Request) -> String {
            let caller = req
                .headers()
                .get(HDR_CALLER)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<none>")
                .to_string();
            let perms = req
                .headers()
                .get(HDR_PERMS)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<none>")
                .to_string();
            format!("{caller}|{perms}")
        }
        let app = Router::new()
            .route("/echo", get(echo))
            .layer(axum::middleware::from_fn_with_state(auth_state, a2a_auth));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let token = crate::jwt::test_support::mint("svc-account-9", &["a2a:review", "other:perm"]);
        let resp = reqwest::Client::new()
            .get(format!("http://{addr}/echo"))
            // A blatant spoof attempt: the client claims to be `admin` with `a2a:admin`.
            .header(HDR_CALLER, "admin")
            .header(HDR_PERMS, "a2a:admin")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        // Downstream sees the token's subject + permissions, never the spoofed values.
        assert_eq!(resp.text().await.unwrap(), "svc-account-9|a2a:review,other:perm");
    }

    #[test]
    fn bind_and_base_url_fallbacks() {
        std::env::remove_var("A2A_BIND");
        assert_eq!(bind_addr(), DEFAULT_BIND);
        std::env::set_var("A2A_BIND", "   ");
        assert_eq!(bind_addr(), DEFAULT_BIND);
        std::env::set_var("A2A_BIND", "127.0.0.1:18080");
        assert_eq!(bind_addr(), "127.0.0.1:18080");
        std::env::remove_var("A2A_BIND");

        std::env::remove_var("A2A_BASE_URL");
        assert!(base_url_from_env().starts_with("http"));
    }
}
