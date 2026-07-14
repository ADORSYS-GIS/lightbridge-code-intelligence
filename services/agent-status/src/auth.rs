//! Constant-time bearer-token authorization for the status server's `GET /status` route.
//!
//! Split out of [`crate::server`] so the timing-sensitive comparison is its own reviewable unit,
//! separate from routing/handler wiring — the property it protects (never leaking the secret via a
//! timing side-channel) is the whole reason this file exists.

use axum::http::{HeaderMap, header};

/// Whether the request carries `Authorization: Bearer <token>` matching the configured token.
pub(crate) fn authorized(headers: &HeaderMap, expected: &str) -> bool {
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
