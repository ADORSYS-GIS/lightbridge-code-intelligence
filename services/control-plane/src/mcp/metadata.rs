//! OAuth 2.0 Protected Resource Metadata (RFC 9728) for the `mcp` role.
//!
//! MCP has no in-band credential exchange: a client arrives with a bearer token or it doesn't. Without
//! this document, a 401 from the transport is a dead end — the client has no way to learn WHICH
//! authorization server issues tokens for this resource, so an operator has to hand-configure one
//! out-of-band. RFC 9728 closes that loop: the 401 carries a `WWW-Authenticate` challenge pointing at
//! `/.well-known/oauth-protected-resource`, and this document names the authorization server.
//!
//! This is the ONLY unauthenticated route the role serves — mounted outside `auth::mcp_auth`, the same
//! way `a2a` mounts its public agent card outside `a2a_auth` ([`crate::a2a::build_router`]). By spec
//! it must be reachable without credentials; that is the entire point of a discovery document.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::AppState;

/// The well-known path this document is served at (RFC 9728 §3). Also the suffix appended to
/// `MCP_PUBLIC_URL` to build the `resource_metadata` URL advertised in `WWW-Authenticate`.
pub const METADATA_PATH: &str = "/.well-known/oauth-protected-resource";

/// RFC 9728 §2. Only the fields this deployment can state truthfully are emitted: every other member
/// is optional, and advertising a capability the role does not implement would be worse than silence.
#[derive(Debug, Serialize)]
pub struct ProtectedResourceMetadata {
    /// The resource identifier — the externally reachable base URL of this MCP surface.
    pub resource: String,
    /// Issuers whose tokens this resource accepts. Exactly one: the realm `mcp_auth` validates against.
    pub authorization_servers: Vec<String>,
    /// The permission strings the tools enforce (ADR-0023). Advertised so a client can request a
    /// token carrying what it will actually need, rather than discovering the gap on a 403.
    pub scopes_supported: Vec<String>,
    /// This resource only ever reads `Authorization: Bearer` (RFC 9728 §2, `bearer_methods_supported`).
    pub bearer_methods_supported: Vec<String>,
}

/// Build the document from the running configuration. `resource` comes from `MCP_PUBLIC_URL` (the
/// pod cannot infer it: Traefik strips the `/mcp` prefix before the request arrives, so the path this
/// surface is published under is invisible from inside). The issuer comes from the live
/// [`crate::jwt::JwtValidator`] rather than a second env read, so it cannot drift from what tokens are
/// actually validated against.
pub fn build(public_url: &str, issuer: &str) -> ProtectedResourceMetadata {
    ProtectedResourceMetadata {
        resource: public_url.trim_end_matches('/').to_string(),
        authorization_servers: vec![issuer.to_string()],
        scopes_supported: vec![
            "repo:read".to_string(),
            "review:read".to_string(),
            "review:trigger".to_string(),
        ],
        bearer_methods_supported: vec!["header".to_string()],
    }
}

/// `GET /.well-known/oauth-protected-resource`.
///
/// 404s when `MCP_PUBLIC_URL` is unset: the `resource` member is REQUIRED by RFC 9728 and must be the
/// real external URL, so a guessed or half-filled document is worse than none — a client would key its
/// token cache to a resource identifier that doesn't match the one it was issued for. Absent the
/// config the role simply behaves as it did before this endpoint existed.
pub async fn protected_resource_metadata(State(state): State<AppState>) -> Response {
    let (Some(public_url), Some(jwt)) = (state.mcp_public_url.as_deref(), state.jwt.as_ref())
    else {
        return (
            StatusCode::NOT_FOUND,
            "protected-resource metadata is unavailable (MCP_PUBLIC_URL unset)",
        )
            .into_response();
    };
    Json(build(public_url, jwt.issuer())).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_names_the_issuer_and_trims_a_trailing_slash() {
        let doc = build(
            "https://code-intelligence-api.example/mcp/",
            "https://auth.example/realms/lightbridge",
        );
        // A trailing slash would make the advertised resource identifier differ by a byte from the
        // one a client derives from its own configured URL, which is enough to break a token cache.
        assert_eq!(doc.resource, "https://code-intelligence-api.example/mcp");
        assert_eq!(
            doc.authorization_servers,
            vec!["https://auth.example/realms/lightbridge".to_string()]
        );
        assert_eq!(doc.bearer_methods_supported, vec!["header".to_string()]);
    }

    #[test]
    fn advertised_scopes_match_what_the_tools_enforce() {
        let doc = build("https://x.example/mcp", "https://auth.example/realms/x");
        // These are the exact strings `handler.rs` passes to `Caller::require`; if a tool's gate
        // changes, this document has to change with it or clients request the wrong token.
        assert_eq!(
            doc.scopes_supported,
            vec![
                "repo:read".to_string(),
                "review:read".to_string(),
                "review:trigger".to_string(),
            ]
        );
    }
}
