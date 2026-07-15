//! Per-task runner tokens (issue #243 / ADR-0092, hardening the ADR-0017 bootstrap contract).
//!
//! Every agent Job used to carry one shared, long-lived bearer (`AGENT_RUNNER_TOKEN`) — the same
//! value, valid against every task, for as long as the process ran. A single leaked Job env therefore
//! yielded a credential good for the internal API across every task, indefinitely.
//!
//! [`RunnerTokenSigner`] mints a fresh HS256 JWT per task instead, scoped by a `tid` claim and an
//! `exp` no longer than the Job's own `activeDeadlineSeconds`. It is self-verifying — the control
//! plane checks the signature and expiry without a DB round trip or a new table — and it is the SAME
//! signing key on both sides of the trust boundary: `KubeLauncher::launch` mints with it
//! ([`RunnerTokenSigner::mint`]) and `RunnerAuth` verifies with it
//! ([`RunnerTokenSigner::verify`]). The key itself (`RUNNER_TOKEN_SIGNING_KEY`) is never injected into
//! a Job — only the per-task tokens it mints are.

use std::time::{SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Grace period added on top of a Job's `activeDeadlineSeconds` when minting its token, so a callback
/// already in flight right at the deadline boundary isn't rejected as expired a few seconds before
/// Kubernetes would have killed the Job anyway.
const EXPIRY_GRACE_SECONDS: u64 = 300;

/// The claims embedded in a per-task runner token: which task it authenticates callbacks for, and
/// when it stops doing so. Nothing else — the internal API re-derives everything else about the task
/// from the database (trust boundary, ADR-0002), never from the token.
#[derive(Debug, Serialize, Deserialize)]
struct RunnerClaims {
    /// The task this token is scoped to.
    tid: Uuid,
    /// Expiry (unix seconds) — validated by `jsonwebtoken` on decode.
    exp: u64,
}

/// Mints and verifies per-task runner tokens with one HMAC-SHA256 key. `KubeLauncher` (the dispatch
/// side, `integrations/k8s.rs`) holds a signer to mint; `AppState` (the internal-API side,
/// `http/internal.rs`) holds one to verify. Both read the same process-local
/// `RUNNER_TOKEN_SIGNING_KEY` — it never leaves the control-plane process.
#[derive(Clone)]
pub struct RunnerTokenSigner {
    encoding_key: EncodingKey,
    decoding_key: DecodingKey,
}

impl RunnerTokenSigner {
    /// Build from `RUNNER_TOKEN_SIGNING_KEY`. `None` when unset or blank — the dispatcher then mints
    /// nothing (Jobs get an empty `AGENT_RUNNER_TOKEN`) and the internal API fails closed (503),
    /// mirroring the old "`AGENT_RUNNER_TOKEN` absent" degrade.
    pub fn from_env() -> Option<Self> {
        let secret = std::env::var("RUNNER_TOKEN_SIGNING_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())?;
        Some(Self::new(secret.as_bytes()))
    }

    pub(crate) fn new(secret: &[u8]) -> Self {
        Self {
            encoding_key: EncodingKey::from_secret(secret),
            decoding_key: DecodingKey::from_secret(secret),
        }
    }

    /// Mint a token scoped to `task_id`, valid for `job_deadline_seconds` from now plus a grace
    /// window — the same hard runtime cap the Job itself is bound by (`activeDeadlineSeconds`), so
    /// the token outlives the Job's last possible callback but is worthless once the Job is gone.
    pub fn mint(&self, task_id: Uuid, job_deadline_seconds: i64) -> anyhow::Result<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let ttl = job_deadline_seconds.max(1) as u64 + EXPIRY_GRACE_SECONDS;
        let claims = RunnerClaims {
            tid: task_id,
            exp: now + ttl,
        };
        Ok(encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &self.encoding_key,
        )?)
    }

    /// Verify a presented token's signature and expiry, returning the task id it is scoped to on
    /// success. The caller ([`crate::http::internal::RunnerAuth`]) is responsible for checking that id
    /// against the request's own `{id}` path parameter — this only proves the token was minted by
    /// this process and hasn't expired, not that it's valid for the task being called.
    pub fn verify(&self, token: &str) -> Result<Uuid, jsonwebtoken::errors::Error> {
        let mut validation = Validation::new(Algorithm::HS256);
        // `mint` already bakes `EXPIRY_GRACE_SECONDS` into `exp`, so the token's own claim is the
        // exact expiry we want enforced. Zero out jsonwebtoken's default 60s leeway rather than
        // stacking an extra, undocumented tolerance on top of a deliberately-sized security window.
        validation.leeway = 0;
        decode::<RunnerClaims>(token, &self.decoding_key, &validation).map(|data| data.claims.tid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mints_a_token_that_verifies_to_the_same_task_id() {
        let signer = RunnerTokenSigner::new(b"test-signing-key");
        let task_id = Uuid::new_v4();
        let token = signer.mint(task_id, 3600).expect("mint");
        assert_eq!(signer.verify(&token).expect("verify"), task_id);
    }

    #[test]
    fn two_tasks_get_distinct_tokens() {
        let signer = RunnerTokenSigner::new(b"test-signing-key");
        let a = signer.mint(Uuid::new_v4(), 3600).expect("mint a");
        let b = signer.mint(Uuid::new_v4(), 3600).expect("mint b");
        assert_ne!(a, b);
    }

    #[test]
    fn a_token_minted_with_a_different_key_does_not_verify() {
        let signer_a = RunnerTokenSigner::new(b"key-a");
        let signer_b = RunnerTokenSigner::new(b"key-b");
        let token = signer_a.mint(Uuid::new_v4(), 3600).expect("mint");
        assert!(signer_b.verify(&token).is_err());
    }

    #[test]
    fn a_stale_expired_token_does_not_verify() {
        let signer = RunnerTokenSigner::new(b"test-signing-key");
        // Well past jsonwebtoken's default 60s clock-skew leeway, so this is unambiguously expired
        // rather than borderline.
        let expired_claims = RunnerClaims {
            tid: Uuid::new_v4(),
            exp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .saturating_sub(3600),
        };
        let token = encode(
            &Header::new(Algorithm::HS256),
            &expired_claims,
            &signer.encoding_key,
        )
        .expect("encode");
        assert!(signer.verify(&token).is_err());
    }

    #[test]
    fn a_malformed_token_does_not_verify() {
        let signer = RunnerTokenSigner::new(b"test-signing-key");
        assert!(signer.verify("not-a-jwt").is_err());
    }
}
