//! OAuth2 resource-server JWT validation.
//!
//! The control plane is a pure **resource server**: it does not issue tokens or store credentials.
//! It validates RS256 access tokens minted by an external OIDC provider (Keycloak in dev) against
//! the provider's published JWKS, checking issuer / audience / expiry. Identity is read from the
//! validated claims — there is no local user store (see ADR-0014).

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::AppState;

/// Verified identity claims extracted from an OIDC access token. Only the fields the control plane
/// uses are deserialized; issuer/audience/expiry are validated by [`JwtValidator`] separately.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Stable subject identifier (the user's id in the IdP).
    pub sub: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub preferred_username: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// Expiry (unix seconds) — validated by jsonwebtoken.
    pub exp: usize,
    /// All other claims, captured so the **permissions** list can be read from a *configurable* claim
    /// path (ADR-0023) — including nested paths the IdP/gateway chooses.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Claims {
    /// A human-readable identity for audit (`approved_by`): username, else email, else subject.
    pub fn identity(&self) -> &str {
        self.preferred_username
            .as_deref()
            .or(self.email.as_deref())
            .unwrap_or(&self.sub)
    }

    /// The caller's permissions, read from the dotted `claim_path` (ADR-0023). The claim value is a
    /// JSON array of strings; anything else (missing claim, wrong type) yields an empty set →
    /// fail-closed authorization.
    pub fn permissions(&self, claim_path: &str) -> std::collections::HashSet<String> {
        // Navigate the dotted path through the flattened extra claims.
        let mut node: Option<&serde_json::Value> = None;
        for (i, segment) in claim_path.split('.').enumerate() {
            node = if i == 0 {
                self.extra.get(segment)
            } else {
                node.and_then(|n| n.get(segment))
            };
            if node.is_none() {
                return std::collections::HashSet::new();
            }
        }
        node.and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Resource-server token-validation config. Built from env; absent `OIDC_ISSUER` disables auth and
/// makes protected routes fail closed.
#[derive(Debug, Clone)]
pub struct OidcConfig {
    pub issuer: String,
    pub audience: String,
    pub jwks_uri: String,
}

impl OidcConfig {
    /// `OIDC_ISSUER` (required to enable auth), `OIDC_AUDIENCE` (default `account`, Keycloak's
    /// default token audience), and a JWKS URI derived from the Keycloak convention
    /// `{issuer}/protocol/openid-connect/certs` unless `OIDC_JWKS_URI` overrides it.
    pub fn from_env() -> Option<Self> {
        let issuer = std::env::var("OIDC_ISSUER")
            .ok()?
            .trim_end_matches('/')
            .to_string();
        let audience = std::env::var("OIDC_AUDIENCE").unwrap_or_else(|_| "account".to_string());
        let jwks_uri = std::env::var("OIDC_JWKS_URI")
            .unwrap_or_else(|_| format!("{issuer}/protocol/openid-connect/certs"));
        Some(Self {
            issuer,
            audience,
            jwks_uri,
        })
    }
}

/// Why a request was not authenticated.
#[derive(Debug)]
pub enum AuthError {
    /// No `Authorization: Bearer …` header.
    MissingToken,
    /// Token failed signature / issuer / audience / expiry validation, or its `kid` is unknown.
    InvalidToken,
    /// JWKS could not be fetched from the IdP.
    JwksUnavailable,
    /// `OIDC_ISSUER` is not configured — the resource server cannot validate anything.
    Disabled,
    /// Authenticated, but the caller lacks the required permission.
    Forbidden,
}

impl IntoResponse for AuthError {
    fn into_response(self) -> axum::response::Response {
        let (status, msg) = match self {
            AuthError::MissingToken => (StatusCode::UNAUTHORIZED, "missing bearer token"),
            AuthError::InvalidToken => (StatusCode::UNAUTHORIZED, "invalid token"),
            AuthError::JwksUnavailable => (StatusCode::SERVICE_UNAVAILABLE, "jwks unavailable"),
            AuthError::Disabled => (StatusCode::SERVICE_UNAVAILABLE, "oidc not configured"),
            AuthError::Forbidden => (StatusCode::FORBIDDEN, "missing required permission"),
        };
        (status, msg).into_response()
    }
}

/// Validates RS256 JWTs against a cached JWKS, refreshing on an unknown `kid` (key rotation).
pub struct JwtValidator {
    config: OidcConfig,
    http: reqwest::Client,
    keys: RwLock<HashMap<String, DecodingKey>>,
}

impl JwtValidator {
    pub fn new(config: OidcConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
            keys: RwLock::new(HashMap::new()),
        }
    }

    fn keys_from_jwks(jwks: &JwkSet) -> HashMap<String, DecodingKey> {
        let mut map = HashMap::new();
        for jwk in &jwks.keys {
            if let (Some(kid), Ok(key)) = (jwk.common.key_id.clone(), DecodingKey::from_jwk(jwk)) {
                map.insert(kid, key);
            }
        }
        map
    }

    async fn refresh(&self) -> Result<(), AuthError> {
        let jwks: JwkSet = self
            .http
            .get(&self.config.jwks_uri)
            .send()
            .await
            .map_err(|_| AuthError::JwksUnavailable)?
            .json()
            .await
            .map_err(|_| AuthError::JwksUnavailable)?;
        *self.keys.write().await = Self::keys_from_jwks(&jwks);
        Ok(())
    }

    /// Ensure the JWKS has been fetched at least once. Used by readiness so a pod that cannot reach
    /// the IdP is not handed traffic it would only 503.
    pub async fn warm(&self) -> Result<(), AuthError> {
        if self.keys.read().await.is_empty() {
            self.refresh().await?;
        }
        Ok(())
    }

    async fn decoding_key(&self, kid: &str) -> Result<DecodingKey, AuthError> {
        if let Some(key) = self.keys.read().await.get(kid).cloned() {
            return Ok(key);
        }
        // Unknown kid → refresh once (handles rotation). A refresh *failure* is a transient JWKS
        // outage (IdP unreachable / bad response) → `JwksUnavailable` (retryable, 503), which must NOT
        // be collapsed into `InvalidToken` (a non-retryable 401). Only a kid that is still absent after
        // a *successful* refresh is a genuine bad token.
        self.refresh().await?;
        self.keys
            .read()
            .await
            .get(kid)
            .cloned()
            .ok_or(AuthError::InvalidToken)
    }

    pub async fn validate(&self, token: &str) -> Result<Claims, AuthError> {
        let header = decode_header(token).map_err(|_| AuthError::InvalidToken)?;
        let kid = header.kid.ok_or(AuthError::InvalidToken)?;
        let key = self.decoding_key(&kid).await?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[&self.config.issuer]);
        validation.set_audience(&[&self.config.audience]);

        decode::<Claims>(token, &key, &validation)
            .map(|data| data.claims)
            .map_err(|_| AuthError::InvalidToken)
    }

    /// Test seam: build with a static JWKS (no network).
    #[cfg(test)]
    pub fn from_static_jwks(config: OidcConfig, jwks_json: &str) -> Self {
        let jwks: JwkSet = serde_json::from_str(jwks_json).expect("valid test jwks");
        Self {
            config,
            http: reqwest::Client::new(),
            keys: RwLock::new(Self::keys_from_jwks(&jwks)),
        }
    }
}

/// Extractor that authenticates a request from its `Authorization: Bearer` token. Reject responses
/// are 401 (bad/missing token) or 503 (auth disabled / JWKS down).
impl FromRequestParts<AppState> for Claims {
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AuthError> {
        let validator = state.jwt.as_ref().ok_or(AuthError::Disabled)?;
        let token = bearer_token(parts).ok_or(AuthError::MissingToken)?;
        validator.validate(&token).await
    }
}

/// Extractor that authenticates a request AND carries the caller's **permissions** (ADR-0023), read
/// from the configured claim (`state.permissions_claim`, from `PERMISSIONS_CLAIM`, default
/// `permissions`). Handlers call [`Caller::require`] to authorize a specific capability.
pub struct Caller {
    pub claims: Claims,
    pub permissions: std::collections::HashSet<String>,
}

impl Caller {
    /// `Ok(())` if the caller holds `permission`, else `Forbidden` (403). Fail-closed: a token without
    /// the permissions claim has an empty set and is denied.
    pub fn require(&self, permission: &str) -> Result<(), AuthError> {
        if self.permissions.contains(permission) {
            Ok(())
        } else {
            Err(AuthError::Forbidden)
        }
    }
}

impl FromRequestParts<AppState> for Caller {
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AuthError> {
        let claims = Claims::from_request_parts(parts, state).await?;
        let permissions = claims.permissions(&state.permissions_claim);
        Ok(Caller {
            claims,
            permissions,
        })
    }
}

fn bearer_token(parts: &Parts) -> Option<String> {
    parts
        .headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::to_string)
}

/// Protected route: the caller's verified identity + effective permissions (ADR-0023). The web reads
/// this to show/hide capability-gated affordances.
pub async fn me(caller: Caller) -> Json<serde_json::Value> {
    let mut perms: Vec<String> = caller.permissions.into_iter().collect();
    perms.sort();
    Json(serde_json::json!({ "claims": caller.claims, "permissions": perms }))
}

/// Convenience for building the validator from env (None when `OIDC_ISSUER` is unset).
pub fn from_env() -> Option<Arc<JwtValidator>> {
    OidcConfig::from_env().map(|config| Arc::new(JwtValidator::new(config)))
}

/// Cross-module test seam: a static-JWKS validator plus a matching token minter, so other modules
/// (e.g. the `a2a` auth layer) can drive the real validation path in tests without a live IdP. The
/// keypair is generated once at runtime — no private key is committed.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    pub const ISSUER: &str = "https://idp.test/realms/lightbridge";
    pub const AUDIENCE: &str = "lightbridge-api";

    /// `(PKCS#8 private PEM, JWKS JSON)`, generated once per process.
    fn keys() -> &'static (String, String) {
        use base64::Engine as _;
        use rsa::pkcs8::EncodePrivateKey as _;
        use rsa::traits::PublicKeyParts as _;

        static KEYS: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();
        KEYS.get_or_init(|| {
            let private = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).expect("gen rsa");
            let pem = private
                .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
                .expect("pkcs8 pem")
                .to_string();
            let public = private.to_public_key();
            let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
            let n = b64.encode(public.n().to_bytes_be());
            let e = b64.encode(public.e().to_bytes_be());
            let jwks = format!(
                r#"{{"keys":[{{"kty":"RSA","n":"{n}","e":"{e}","kid":"a2a-test","alg":"RS256","use":"sig"}}]}}"#
            );
            (pem, jwks)
        })
    }

    /// A validator that trusts [`keys`]'s JWKS (no network).
    pub fn validator() -> JwtValidator {
        JwtValidator::from_static_jwks(
            OidcConfig {
                issuer: ISSUER.to_string(),
                audience: AUDIENCE.to_string(),
                jwks_uri: "http://unused.invalid".to_string(),
            },
            &keys().1,
        )
    }

    /// Mint an RS256 token for `sub` carrying `permissions` (under the default `permissions` claim).
    pub fn mint(sub: &str, permissions: &[&str]) -> String {
        let exp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as usize
            + 3600;
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("a2a-test".to_string());
        let claims = json!({
            "sub": sub,
            "iss": ISSUER,
            "aud": AUDIENCE,
            "exp": exp,
            "permissions": permissions,
        });
        let key = EncodingKey::from_rsa_pem(keys().0.as_bytes()).expect("test signing key");
        encode(&header, &claims, &key).expect("sign token")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::json;

    const ISSUER: &str = "https://idp.test/realms/lightbridge";
    const AUDIENCE: &str = "lightbridge-api";

    /// Generate an RSA test keypair once per run: returns `(PKCS#8 private PEM, JWKS JSON)`. Done at
    /// runtime so no private key material is committed to source.
    fn test_keys() -> &'static (String, String) {
        use base64::Engine as _;
        use rsa::pkcs8::EncodePrivateKey as _;
        use rsa::traits::PublicKeyParts as _;

        static KEYS: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();
        KEYS.get_or_init(|| {
            let private = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).expect("gen rsa");
            let pem = private
                .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
                .expect("pkcs8 pem")
                .to_string();
            let public = private.to_public_key();
            let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
            let n = b64.encode(public.n().to_bytes_be());
            let e = b64.encode(public.e().to_bytes_be());
            let jwks = format!(
                r#"{{"keys":[{{"kty":"RSA","n":"{n}","e":"{e}","kid":"test-key-1","alg":"RS256","use":"sig"}}]}}"#
            );
            (pem, jwks)
        })
    }

    fn validator() -> JwtValidator {
        JwtValidator::from_static_jwks(
            OidcConfig {
                issuer: ISSUER.to_string(),
                audience: AUDIENCE.to_string(),
                jwks_uri: "http://unused.invalid".to_string(),
            },
            &test_keys().1,
        )
    }

    fn now() -> usize {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as usize
    }

    /// Mint an RS256 token signed by the test key, overriding fields via the closure.
    fn mint(kid: &str, iss: &str, aud: &str, exp: usize) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.to_string());
        let claims = json!({
            "sub": "user-123",
            "email": "dev@lightbridge.test",
            "preferred_username": "dev",
            "name": "Dev User",
            "iss": iss,
            "aud": aud,
            "exp": exp,
        });
        let key = EncodingKey::from_rsa_pem(test_keys().0.as_bytes()).expect("test signing key");
        encode(&header, &claims, &key).expect("sign token")
    }

    fn claims_with(extra: serde_json::Value) -> Claims {
        Claims {
            sub: "u1".into(),
            email: Some("a@b.c".into()),
            preferred_username: Some("alice".into()),
            name: None,
            exp: 0,
            extra: extra.as_object().cloned().unwrap_or_default(),
        }
    }

    #[test]
    fn permissions_read_from_configured_claim() {
        // Top-level claim.
        let c = claims_with(json!({ "permissions": ["repo:read", "repo:approve"] }));
        let perms = c.permissions("permissions");
        assert!(perms.contains("repo:approve") && perms.contains("repo:read"));
        assert!(!perms.contains("repo:deny"));
        assert_eq!(c.identity(), "alice", "username preferred for audit");

        // Nested dotted path.
        let nested = claims_with(json!({ "code_intelligence": { "permissions": ["task:logs"] } }));
        assert!(nested
            .permissions("code_intelligence.permissions")
            .contains("task:logs"));

        // Missing / wrong-typed claim → empty set (fail-closed).
        assert!(claims_with(json!({})).permissions("permissions").is_empty());
        assert!(claims_with(json!({ "permissions": "not-an-array" }))
            .permissions("permissions")
            .is_empty());
    }

    #[tokio::test]
    async fn valid_token_yields_claims() {
        let token = mint("test-key-1", ISSUER, AUDIENCE, now() + 3600);
        let claims = validator().validate(&token).await.expect("valid");
        assert_eq!(claims.sub, "user-123");
        assert_eq!(claims.email.as_deref(), Some("dev@lightbridge.test"));
    }

    #[tokio::test]
    async fn wrong_issuer_is_rejected() {
        let token = mint("test-key-1", "https://evil.test", AUDIENCE, now() + 3600);
        assert!(matches!(
            validator().validate(&token).await,
            Err(AuthError::InvalidToken)
        ));
    }

    #[tokio::test]
    async fn wrong_audience_is_rejected() {
        let token = mint("test-key-1", ISSUER, "some-other-api", now() + 3600);
        assert!(matches!(
            validator().validate(&token).await,
            Err(AuthError::InvalidToken)
        ));
    }

    #[tokio::test]
    async fn expired_token_is_rejected() {
        // Past jsonwebtoken's default 60s exp leeway.
        let token = mint("test-key-1", ISSUER, AUDIENCE, now() - 3600);
        assert!(matches!(
            validator().validate(&token).await,
            Err(AuthError::InvalidToken)
        ));
    }

    #[tokio::test]
    async fn unknown_kid_with_unreachable_idp_is_jwks_unavailable() {
        // kid not in the static JWKS → a refresh is attempted; the IdP is unreachable
        // (unused.invalid), so this is a transient JWKS outage, NOT a bad token. It must surface as
        // `JwksUnavailable` (→ retryable 503), not `InvalidToken` (→ non-retryable 401): callers should
        // back off and retry rather than treat the token as permanently invalid.
        let token = mint("rotated-key", ISSUER, AUDIENCE, now() + 3600);
        assert!(matches!(
            validator().validate(&token).await,
            Err(AuthError::JwksUnavailable)
        ));
    }

    #[tokio::test]
    async fn tampered_signature_is_rejected() {
        let mut token = mint("test-key-1", ISSUER, AUDIENCE, now() + 3600);
        // Flip the FIRST character of the signature segment. Unlike the last
        // base64url char (which encodes only 2 significant bits of the final
        // signature byte and so often round-trips to identical bytes), the
        // first char encodes a full 6 bits, guaranteeing the decoded signature
        // actually changes.
        let sig_start = token.rfind('.').expect("JWT has three segments") + 1;
        let first = token.as_bytes()[sig_start] as char;
        let replacement = if first == 'A' { "B" } else { "A" };
        token.replace_range(sig_start..sig_start + 1, replacement);
        assert!(matches!(
            validator().validate(&token).await,
            Err(AuthError::InvalidToken)
        ));
    }
}
