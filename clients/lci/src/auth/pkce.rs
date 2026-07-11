//! PKCE (RFC 7636) + the small random-string helpers the Authorization-Code flow needs.
//!
//! We hand-roll this rather than pulling the `oauth2` crate: the code challenge is just
//! `base64url(sha256(verifier))` and the exchange is a plain reqwest POST, so the dependency isn't
//! worth its weight.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rand::RngExt as _;
use sha2::{Digest, Sha256};

/// The unreserved character set RFC 7636 allows in a code verifier.
const VERIFIER_ALPHABET: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

/// A generated PKCE pair: the secret `verifier` (kept locally, sent only at token exchange) and the
/// `challenge` (sent in the authorize request).
#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    /// Generate a fresh pair with a 64-char verifier (within RFC 7636's 43–128 range).
    pub fn generate() -> Self {
        let verifier = random_string(64, VERIFIER_ALPHABET);
        let challenge = challenge_for(&verifier);
        Self {
            verifier,
            challenge,
        }
    }
}

/// Derive the S256 code challenge for a given verifier: `base64url(sha256(verifier))`, no padding.
/// Split out so it's unit-testable against the RFC 7636 Appendix B vector.
pub fn challenge_for(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// A URL-safe random `state` value for CSRF protection on the redirect.
pub fn random_state() -> String {
    random_string(32, VERIFIER_ALPHABET)
}

/// Draw `len` characters uniformly from `alphabet`.
fn random_string(len: usize, alphabet: &[u8]) -> String {
    let mut rng = rand::rng();
    (0..len)
        .map(|_| {
            let idx = rng.random_range(0..alphabet.len());
            alphabet[idx] as char
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_matches_rfc7636_appendix_b_vector() {
        // RFC 7636 Appendix B: this verifier must derive exactly this challenge.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(challenge_for(verifier), expected);
    }

    #[test]
    fn generated_verifier_is_in_range_and_url_safe() {
        let pkce = Pkce::generate();
        assert!((43..=128).contains(&pkce.verifier.len()));
        assert!(pkce
            .verifier
            .bytes()
            .all(|b| VERIFIER_ALPHABET.contains(&b)));
        // The challenge round-trips through the derivation.
        assert_eq!(challenge_for(&pkce.verifier), pkce.challenge);
    }

    #[test]
    fn state_is_nonempty_and_varies() {
        let a = random_state();
        let b = random_state();
        assert_eq!(a.len(), 32);
        assert_ne!(a, b, "two states should differ (probabilistically)");
    }
}
