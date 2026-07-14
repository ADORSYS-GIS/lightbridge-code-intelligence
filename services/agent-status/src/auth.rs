//! Constant-time bearer-token authorization for the status server's `GET /status` route.
//!
//! Split out of [`crate::server`] so the timing-sensitive comparison is its own reviewable unit,
//! separate from routing/handler wiring — the property it protects (never leaking the secret via a
//! timing side-channel) is the whole reason this file exists.

use axum::http::{HeaderMap, header};

/// Whether the request carries `Authorization: Bearer <token>` matching the configured token.
///
/// Fails closed on a blank `expected`: an unset/misconfigured token must never be satisfiable by a
/// caller presenting an empty `Bearer` token (`Authorization: Bearer `, trimmed to `""`), which the
/// constant-time comparison below would otherwise treat as a valid match.
pub(crate) fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    if expected.is_empty() {
        return false;
    }
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .is_some_and(|presented| constant_time_eq(presented.as_bytes(), expected.as_bytes()))
}

/// Constant-time bearer-token comparison. A plain `==` on `&str` short-circuits on the first
/// differing byte, and even `subtle`'s `ConstantTimeEq` needs equal-length inputs — guarding the
/// length with `presented.len() == expected.len()` short-circuits on a mismatch and thereby leaks the
/// secret's *length* to a timing attacker. Instead hash both sides to a fixed 32-byte SHA-256 digest
/// (always equal-length, so no length branch) and `ct_eq` the digests. Byte-for-byte constant time
/// regardless of the presented token's length; a collision would require breaking SHA-256.
fn constant_time_eq(presented: &[u8], expected: &[u8]) -> bool {
    use sha2::{Digest, Sha256};
    use subtle::ConstantTimeEq as _;
    let presented = Sha256::digest(presented);
    let expected = Sha256::digest(expected);
    presented.ct_eq(&expected).into()
}

#[cfg(test)]
mod tests {
    use super::authorized;
    use axum::http::{HeaderMap, HeaderValue, header};

    fn headers_with_bearer(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        headers
    }

    #[test]
    fn blank_expected_token_never_authorizes_even_an_empty_bearer_token() {
        // A blank `expected` (unset/misconfigured secret) must fail closed, not treat an empty
        // `Authorization: Bearer ` as a match — the regression this test guards against.
        assert!(!authorized(&headers_with_bearer(""), ""));
        assert!(!authorized(&headers_with_bearer("anything"), ""));
        assert!(!authorized(&HeaderMap::new(), ""));
    }

    #[test]
    fn matching_bearer_token_authorizes() {
        assert!(authorized(&headers_with_bearer("secret"), "secret"));
    }

    #[test]
    fn mismatched_bearer_token_does_not_authorize() {
        assert!(!authorized(&headers_with_bearer("wrong"), "secret"));
    }

    #[test]
    fn missing_authorization_header_does_not_authorize() {
        assert!(!authorized(&HeaderMap::new(), "secret"));
    }
}
