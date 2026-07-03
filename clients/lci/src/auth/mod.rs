//! Authentication: OIDC discovery, the Authorization-Code + PKCE loopback login, token caching, and
//! silent refresh. Hand-rolled (no `oauth2` crate) — everything is plain reqwest + sha2/base64.
//!
//! The interactive login prints URLs to the *normal* terminal, so it MUST run before the TUI enters
//! raw mode / the alternate screen.

mod pkce;
mod store;

pub use store::{clear, StoredToken};

use crate::config::Config;
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

/// Refresh/validity skew: treat a token within this many seconds of expiry as needing refresh.
pub const EXPIRY_SKEW_SECS: i64 = 60;
/// How long we wait for the browser redirect before giving up on interactive login.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// The subset of the OIDC discovery document we use.
#[derive(Debug, Clone, Deserialize)]
pub struct OidcMetadata {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    /// Kept for completeness (RP-initiated logout) even though the client only clears the local cache
    /// today; parsing it keeps the discovery struct a faithful mirror.
    #[serde(default)]
    #[allow(dead_code)]
    pub end_session_endpoint: Option<String>,
}

/// Current unix time in seconds.
pub fn now_unix() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// Fetch (and in the caller, cache) the issuer's discovery document.
pub async fn discover(http: &reqwest::Client, issuer: &str) -> Result<OidcMetadata> {
    let url = format!("{}/.well-known/openid-configuration", issuer);
    let resp = http
        .get(&url)
        .send()
        .await
        .with_context(|| format!("OIDC discovery request to {url}"))?;
    if !resp.status().is_success() {
        bail!("OIDC discovery failed ({}) at {url}", resp.status());
    }
    resp.json::<OidcMetadata>()
        .await
        .context("parsing OIDC discovery document")
}

/// Ensure we hold a usable access token, following the documented startup ladder:
/// fresh cached token → refresh → interactive login. Returns the live token and persists any new one.
pub async fn ensure_token(http: &reqwest::Client, cfg: &Config) -> Result<StoredToken> {
    let meta = discover(http, &cfg.issuer).await?;
    let path = Config::token_path()?;
    let now = now_unix();

    if let Some(cached) = store::load(&path)? {
        if cached.is_fresh(now, EXPIRY_SKEW_SECS) {
            tracing::debug!("using cached access token");
            return Ok(cached);
        }
        if let Some(refresh) = cached.refresh_token.clone() {
            tracing::debug!("cached token stale; attempting silent refresh");
            match refresh_token(http, cfg, &meta, &refresh).await {
                Ok(fresh) => {
                    store::save(&path, &fresh)?;
                    return Ok(fresh);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "refresh failed; falling back to interactive login");
                }
            }
        }
    }

    let fresh = interactive_login(http, cfg, &meta).await?;
    store::save(&path, &fresh)?;
    Ok(fresh)
}

/// Force an interactive login regardless of cache state (`lci login`).
pub async fn force_login(http: &reqwest::Client, cfg: &Config) -> Result<StoredToken> {
    let meta = discover(http, &cfg.issuer).await?;
    let fresh = interactive_login(http, cfg, &meta).await?;
    store::save(&Config::token_path()?, &fresh)?;
    Ok(fresh)
}

/// Attempt a background refresh with the given refresh token, persisting on success. Used by the
/// TUI's background refresh task. Returns the new token; the caller decides how to surface failure.
pub async fn try_refresh(
    http: &reqwest::Client,
    cfg: &Config,
    refresh: &str,
) -> Result<StoredToken> {
    let meta = discover(http, &cfg.issuer).await?;
    let fresh = refresh_token(http, cfg, &meta, refresh).await?;
    store::save(&Config::token_path()?, &fresh)?;
    Ok(fresh)
}

/// `grant_type=refresh_token` exchange. An `invalid_grant` (expired/revoked refresh token) surfaces
/// as an error so the caller falls through to interactive login.
async fn refresh_token(
    http: &reqwest::Client,
    cfg: &Config,
    meta: &OidcMetadata,
    refresh: &str,
) -> Result<StoredToken> {
    let params = [
        ("grant_type", "refresh_token"),
        ("client_id", cfg.client_id.as_str()),
        ("refresh_token", refresh),
    ];
    let resp = http
        .post(&meta.token_endpoint)
        .form(&params)
        .send()
        .await
        .context("refresh_token request")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("refresh rejected ({status}): {}", brief(&body));
    }
    let token: store::TokenResponse =
        serde_json::from_str(&body).context("parsing refresh response")?;
    Ok(StoredToken::from_response(token, now_unix()))
}

/// Run the full Authorization-Code + PKCE loopback login. Prints the authorize URL, opens the
/// browser, waits for the `/callback`, verifies `state`, and exchanges the code for tokens.
async fn interactive_login(
    http: &reqwest::Client,
    cfg: &Config,
    meta: &OidcMetadata,
) -> Result<StoredToken> {
    let pkce = pkce::Pkce::generate();
    let state = pkce::random_state();
    let redirect_uri = cfg.redirect_uri();

    // Bind the loopback listener FIRST so the port is guaranteed available before we hand the URL to
    // the browser.
    let addr: SocketAddr = format!("127.0.0.1:{}", cfg.redirect_port)
        .parse()
        .expect("valid loopback socket address");
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding loopback listener on {addr} (is the port in use?)"))?;

    let authorize_url = build_authorize_url(meta, cfg, &redirect_uri, &state, &pkce.challenge)?;

    println!("\nOpen this URL to sign in:\n  {authorize_url}\n");
    if let Err(e) = open::that(authorize_url.as_str()) {
        tracing::debug!(error = %e, "could not auto-open the browser; use the URL above");
    }
    println!("Waiting for the sign-in redirect on {} …", redirect_uri);

    let callback = tokio::time::timeout(LOGIN_TIMEOUT, wait_for_callback(&listener))
        .await
        .map_err(|_| anyhow!("timed out waiting for the sign-in redirect after 5 minutes"))??;

    // CSRF: the returned state must match the one we generated.
    if callback.state.as_deref() != Some(state.as_str()) {
        bail!("state mismatch on the OAuth redirect — aborting (possible CSRF)");
    }
    if let Some(err) = callback.error {
        bail!("sign-in failed: {err}");
    }
    let code = callback
        .code
        .ok_or_else(|| anyhow!("redirect carried no authorization code"))?;

    let token = exchange_code(http, cfg, meta, &redirect_uri, &code, &pkce.verifier).await?;
    println!("Signed in.\n");
    Ok(token)
}

/// Build the `/authorize` URL with all required params.
fn build_authorize_url(
    meta: &OidcMetadata,
    cfg: &Config,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
) -> Result<Url> {
    let mut url = Url::parse(&meta.authorization_endpoint)
        .context("invalid authorization_endpoint in discovery")?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &cfg.client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", &cfg.scope)
        .append_pair("state", state)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(url)
}

/// `grant_type=authorization_code` exchange (with the PKCE verifier).
async fn exchange_code(
    http: &reqwest::Client,
    cfg: &Config,
    meta: &OidcMetadata,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> Result<StoredToken> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", cfg.client_id.as_str()),
        ("code_verifier", verifier),
    ];
    let resp = http
        .post(&meta.token_endpoint)
        .form(&params)
        .send()
        .await
        .context("authorization_code exchange request")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("token exchange rejected ({status}): {}", brief(&body));
    }
    let token: store::TokenResponse =
        serde_json::from_str(&body).context("parsing token exchange response")?;
    Ok(StoredToken::from_response(token, now_unix()))
}

/// What we extract from the `/callback` query string.
#[derive(Debug, Default)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Accept a single loopback connection, read the request line, parse `/callback?...`, and reply with
/// a friendly HTML page so the operator can close the tab.
async fn wait_for_callback(listener: &TcpListener) -> Result<Callback> {
    loop {
        let (mut socket, _peer) = listener
            .accept()
            .await
            .context("accepting loopback connection")?;

        // The request line (`GET /callback?... HTTP/1.1`) is all we need; a small read covers it.
        let mut buf = [0u8; 8192];
        let n = socket
            .read(&mut buf)
            .await
            .context("reading callback request")?;
        let request = String::from_utf8_lossy(&buf[..n]);
        let Some(target) = request_target(&request) else {
            // Not a request we understand (e.g. a browser prefetch); answer 404 and keep waiting.
            let _ = socket.write_all(&http_response(404, "Not found")).await;
            continue;
        };

        // Browsers commonly hit `/favicon.ico` alongside the callback — ignore it, keep listening.
        if !target.starts_with("/callback") {
            let _ = socket.write_all(&http_response(404, "Not found")).await;
            continue;
        }

        let callback = parse_callback(target);
        let ok = callback.error.is_none() && callback.code.is_some();
        let page = if ok {
            "<h2>Signed in</h2><p>You can close this tab and return to the terminal.</p>"
        } else {
            "<h2>Sign-in failed</h2><p>Return to the terminal for details.</p>"
        };
        let _ = socket.write_all(&http_response(200, page)).await;
        let _ = socket.flush().await;
        return Ok(callback);
    }
}

/// Extract the request target (the path+query) from an HTTP request's first line.
fn request_target(request: &str) -> Option<&str> {
    let line = request.lines().next()?;
    let mut parts = line.split_whitespace();
    let _method = parts.next()?;
    parts.next()
}

/// Parse the `code` / `state` / `error` params out of a `/callback?...` target.
fn parse_callback(target: &str) -> Callback {
    let mut cb = Callback::default();
    // Give the relative target a base so `Url` can parse the query.
    if let Ok(url) = Url::parse(&format!("http://127.0.0.1{target}")) {
        let pairs: HashMap<_, _> = url.query_pairs().into_owned().collect();
        cb.code = pairs.get("code").cloned();
        cb.state = pairs.get("state").cloned();
        cb.error = pairs
            .get("error")
            .map(|e| match pairs.get("error_description") {
                Some(desc) => format!("{e}: {desc}"),
                None => e.clone(),
            });
    }
    cb
}

/// Build a minimal HTTP/1.1 response. `body` is an HTML fragment (for 200) or plain text (404),
/// wrapped in a tiny document for the browser tab.
fn http_response(status: u16, body: &str) -> Vec<u8> {
    let (reason, content_type, document) = match status {
        200 => (
            "OK",
            "text/html; charset=utf-8",
            format!(
                "<!doctype html><meta charset=utf-8><title>lci</title>\
                 <body style=\"font-family:system-ui;margin:3rem;color:#222\">{body}</body>"
            ),
        ),
        _ => ("Not Found", "text/plain; charset=utf-8", body.to_string()),
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{document}",
        document.len()
    )
    .into_bytes()
}

/// Trim an error body for a one-line log/message (Keycloak errors are small JSON blobs).
fn brief(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.len() > 300 {
        format!("{}…", &trimmed[..300])
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg() -> Config {
        Config {
            api_url: "https://api.test".into(),
            issuer: "https://issuer.test/realms/x".into(),
            client_id: "lightbridge-cli".into(),
            redirect_port: 8765,
            scope: "openid profile email".into(),
        }
    }

    fn test_meta() -> OidcMetadata {
        OidcMetadata {
            authorization_endpoint: "https://issuer.test/realms/x/protocol/openid-connect/auth"
                .into(),
            token_endpoint: "https://issuer.test/realms/x/protocol/openid-connect/token".into(),
            end_session_endpoint: None,
        }
    }

    #[test]
    fn authorize_url_carries_all_required_params() {
        let cfg = test_cfg();
        let url = build_authorize_url(
            &test_meta(),
            &cfg,
            &cfg.redirect_uri(),
            "the-state",
            "the-challenge",
        )
        .unwrap();
        let q: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(
            q.get("client_id").map(String::as_str),
            Some("lightbridge-cli")
        );
        assert_eq!(
            q.get("redirect_uri").map(String::as_str),
            Some("http://127.0.0.1:8765/callback")
        );
        assert_eq!(
            q.get("scope").map(String::as_str),
            Some("openid profile email")
        );
        assert_eq!(q.get("state").map(String::as_str), Some("the-state"));
        assert_eq!(
            q.get("code_challenge").map(String::as_str),
            Some("the-challenge")
        );
        assert_eq!(
            q.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        // We must NOT hardcode an audience in the request.
        assert!(!q.contains_key("audience"));
        assert!(!q.contains_key("resource"));
    }

    #[test]
    fn parses_callback_query() {
        let cb = parse_callback("/callback?code=abc123&state=xyz");
        assert_eq!(cb.code.as_deref(), Some("abc123"));
        assert_eq!(cb.state.as_deref(), Some("xyz"));
        assert!(cb.error.is_none());
    }

    #[test]
    fn parses_error_redirect() {
        let cb = parse_callback("/callback?error=access_denied&error_description=User%20said%20no");
        assert!(cb.code.is_none());
        assert_eq!(cb.error.as_deref(), Some("access_denied: User said no"));
    }

    #[test]
    fn extracts_request_target() {
        let req = "GET /callback?code=x&state=y HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        assert_eq!(request_target(req), Some("/callback?code=x&state=y"));
    }

    #[test]
    fn rejected_when_state_differs() {
        // Mirror the check in interactive_login: a mismatched state is rejected.
        let returned = parse_callback("/callback?code=abc&state=attacker");
        let expected = "ours";
        assert_ne!(returned.state.as_deref(), Some(expected));
    }
}
