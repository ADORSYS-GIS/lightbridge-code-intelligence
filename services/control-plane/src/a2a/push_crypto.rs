//! AEAD encryption-at-rest for the caller-supplied webhook auth token (ADR-0079 §3).
//!
//! ## Cipher choice — ChaCha20-Poly1305 via `ring::aead`
//!
//! `ring` is already this workspace's **single, pinned** rustls crypto provider (installed
//! process-wide in `main`; the workspace `Cargo.toml` pins `ring` precisely to avoid a second
//! provider panicking rustls on the first handshake). Reusing `ring::aead` therefore leans on an
//! audited, already-compiled primitive and adds **no new crypto crate and no second provider**. We
//! deliberately do **not** roll our own cipher or KDF: the key is a raw 32-byte key, the nonce is a
//! fresh 12-byte CSPRNG draw per encryption, and integrity is Poly1305's authentication tag.
//!
//! ## Wire format
//!
//! ```text
//! token_enc = nonce(12) || ciphertext || tag(16)
//! ```
//!
//! The nonce is random per encryption ([`ring::rand::SystemRandom`], a CSPRNG), so two encryptions of
//! the same plaintext differ. [`decrypt`] returns `None` on **any** failure — wrong key, truncation,
//! or tampering (the Poly1305 tag verify fails) — and never panics. The token bytes and the key are
//! never logged anywhere in this module.

use base64::Engine;
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};

/// A 32-byte ChaCha20-Poly1305 key. A newtype so key material can't be confused with arbitrary bytes
/// and is never accidentally formatted: it intentionally has **no** `Debug`/`Display`, so a stray
/// `{:?}` on it will not compile, let alone leak the key into a log line.
pub struct Key(LessSafeKey);

impl Key {
    /// Build a key from raw bytes. `None` unless the slice is exactly the algorithm's 32-byte key
    /// length — a wrong-length key is a fail-closed configuration error, not a silent truncation.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let unbound = UnboundKey::new(&CHACHA20_POLY1305, bytes).ok()?;
        Some(Self(LessSafeKey::new(unbound)))
    }

    /// Decode a standard-base64 32-byte key (the `A2A_PUSH_TOKEN_KEY` encoding). `None` on invalid
    /// base64 or a wrong decoded length, so a misconfigured key fails closed rather than half-working.
    pub fn from_base64(encoded: &str) -> Option<Self> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .ok()?;
        Self::from_bytes(&bytes)
    }
}

/// Encrypt `plaintext` under `key`, returning `nonce || ciphertext || tag`.
///
/// A fresh 12-byte nonce is drawn from the OS CSPRNG per call, so the output is non-deterministic. The
/// only failure mode is a catastrophic CSPRNG/OS-entropy failure, which is unrecoverable and panics
/// (the process cannot safely mint a nonce) — the seal itself is infallible for a valid key.
pub fn encrypt(plaintext: &[u8], key: &Key) -> Vec<u8> {
    let rng = SystemRandom::new();
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rng.fill(&mut nonce_bytes)
        .expect("CSPRNG (getrandom) failed — cannot mint a webhook-token nonce");
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);

    let mut in_out = plaintext.to_vec();
    key.0
        .seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)
        .expect("ChaCha20-Poly1305 seal is infallible for a valid key");

    let mut out = Vec::with_capacity(NONCE_LEN + in_out.len());
    out.extend_from_slice(&nonce_bytes);
    out.append(&mut in_out);
    out
}

/// Decrypt `bytes` (`nonce || ciphertext || tag`) under `key` back to the original UTF-8 string.
///
/// Returns `None` — never panics — on any failure: too short to hold a nonce + tag, a wrong key or
/// tampered/truncated ciphertext (the Poly1305 verify fails), or non-UTF-8 plaintext.
pub fn decrypt(bytes: &[u8], key: &Key) -> Option<String> {
    let tag_len = CHACHA20_POLY1305.tag_len();
    // Below nonce + tag there isn't even room for an empty authenticated message: reject as truncated.
    if bytes.len() < NONCE_LEN + tag_len {
        return None;
    }
    let (nonce_bytes, ciphertext) = bytes.split_at(NONCE_LEN);
    let nonce = Nonce::assume_unique_for_key(nonce_bytes.try_into().ok()?);

    let mut in_out = ciphertext.to_vec();
    let plaintext = key.0.open_in_place(nonce, Aad::empty(), &mut in_out).ok()?;
    String::from_utf8(plaintext.to_vec()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_a() -> Key {
        Key::from_bytes(&[7u8; 32]).unwrap()
    }

    fn key_b() -> Key {
        Key::from_bytes(&[9u8; 32]).unwrap()
    }

    #[test]
    fn round_trips_plaintext() {
        let key = key_a();
        for plaintext in [
            "",
            "s3cr3t",
            "a longer webhook bearer token with symbols !@#$%^&*()",
        ] {
            let ct = encrypt(plaintext.as_bytes(), &key);
            assert_eq!(decrypt(&ct, &key).as_deref(), Some(plaintext));
        }
    }

    #[test]
    fn two_encryptions_of_same_plaintext_differ() {
        // A fresh random nonce per call ⇒ the ciphertexts (and the prepended nonces) differ, so the
        // stored bytes never reveal that two configs carry the same token.
        let key = key_a();
        let a = encrypt(b"same-token", &key);
        let b = encrypt(b"same-token", &key);
        assert_ne!(a, b, "random nonce must make repeated encryptions differ");
        // …yet both still decrypt to the original.
        assert_eq!(decrypt(&a, &key).as_deref(), Some("same-token"));
        assert_eq!(decrypt(&b, &key).as_deref(), Some("same-token"));
    }

    #[test]
    fn wrong_key_fails_to_decrypt() {
        let ct = encrypt(b"s3cr3t", &key_a());
        assert!(
            decrypt(&ct, &key_b()).is_none(),
            "a different key must not decrypt (tag verify fails), and must not panic"
        );
    }

    #[test]
    fn truncated_ciphertext_returns_none() {
        let key = key_a();
        let ct = encrypt(b"s3cr3t", &key);
        // Every truncation — including below the nonce+tag floor — is None, never a panic.
        for cut in 0..ct.len() {
            assert!(
                decrypt(&ct[..cut], &key).is_none(),
                "truncation to {cut} bytes must be rejected"
            );
        }
    }

    #[test]
    fn tampered_ciphertext_returns_none() {
        let key = key_a();
        let mut ct = encrypt(b"s3cr3t", &key);
        // Flip a bit inside the ciphertext body (past the 12-byte nonce) — the tag verify must fail.
        let last = ct.len() - 1;
        ct[last] ^= 0x01;
        assert!(decrypt(&ct, &key).is_none(), "a flipped tag byte must fail");

        // Flip a bit inside the nonce — decrypts against the wrong nonce, tag verify fails.
        let mut ct2 = encrypt(b"s3cr3t", &key);
        ct2[0] ^= 0x01;
        assert!(
            decrypt(&ct2, &key).is_none(),
            "a corrupted nonce must fail, not panic"
        );
    }

    #[test]
    fn key_from_bytes_rejects_wrong_length() {
        assert!(
            Key::from_bytes(&[0u8; 31]).is_none(),
            "31 bytes is too short"
        );
        assert!(
            Key::from_bytes(&[0u8; 33]).is_none(),
            "33 bytes is too long"
        );
        assert!(Key::from_bytes(&[0u8; 32]).is_some(), "32 bytes is exact");
    }

    #[test]
    fn key_from_base64_round_trips_and_rejects_garbage() {
        let raw = [3u8; 32];
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        let key = Key::from_base64(&encoded).expect("valid 32-byte base64 key");
        // The decoded key actually works end-to-end.
        let ct = encrypt(b"hello", &key);
        assert_eq!(decrypt(&ct, &key).as_deref(), Some("hello"));

        // Surrounding whitespace (a trailing newline from an env/secret file) is tolerated.
        assert!(Key::from_base64(&format!("  {encoded}\n")).is_some());

        // Not base64, and valid base64 of the wrong length, both fail closed.
        assert!(Key::from_base64("not base64!!!").is_none());
        let short = base64::engine::general_purpose::STANDARD.encode([1u8; 16]);
        assert!(
            Key::from_base64(&short).is_none(),
            "16-byte key material must be rejected"
        );
    }
}
