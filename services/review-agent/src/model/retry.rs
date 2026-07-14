//! Retry/backoff policy (ADR-0039) and the transient-vs-deterministic error classification the
//! transport (`client`/`stream`) reports failures through.

use std::time::Duration;

use lci_agent_types::StepError;

/// Retry/backoff policy for one chat turn (ADR-0039). Retries fire **only** on transient failures
/// (connect/timeout, HTTP 429, HTTP 5xx); a 4xx other than 429 is deterministic and never retried.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Retries on a transient failure (total attempts = `max_retries + 1`).
    pub max_retries: u32,
    /// Base backoff; attempt *n* (0-indexed) sleeps roughly `base * 2^n` plus deterministic jitter.
    pub base_backoff: Duration,
    /// Ceiling on a single backoff so a high attempt count can't sleep absurdly long.
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            base_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(8),
        }
    }
}

impl RetryPolicy {
    /// Backoff for `attempt` (0 = the wait *before* the first retry). Exponential, capped at
    /// `max_backoff`, plus a small **deterministic** jitter seeded by the attempt index — so the
    /// schedule is reproducible in tests (no clock, no RNG) yet de-synchronises retries a little.
    pub(super) fn backoff(&self, attempt: u32) -> Duration {
        let factor = 1u64 << attempt.min(16); // 2^attempt, clamped so the shift can't overflow
        let base = self
            .base_backoff
            .saturating_mul(factor.min(u32::MAX as u64) as u32);
        let capped = base.min(self.max_backoff);
        // Deterministic jitter in [0, 250ms): a cheap hash of the attempt index, no SystemTime/RNG.
        let jitter_ms = (attempt as u64).wrapping_mul(2_654_435_761) % 250;
        capped.saturating_add(Duration::from_millis(jitter_ms))
    }
}

/// Why a turn failed, so the loop can decide whether a transient error is worth a retry/failover vs.
/// a deterministic one that should fail fast.
#[derive(Debug)]
pub struct ChatError {
    pub error: anyhow::Error,
    /// `true` for connect/timeout, HTTP 429, or HTTP 5xx — the loop retries/fails over on these only.
    pub transient: bool,
    /// `Retry-After` seconds parsed off a 429, when present — the loop honours it over its own backoff.
    pub retry_after: Option<Duration>,
}

impl std::fmt::Display for ChatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.error)
    }
}

/// Map a classified transport failure onto the engine's [`StepError`]. Transient failures carry the
/// `Retry-After` hint through; a deterministic failure folds the (body-bearing) error text into the
/// terminal reason so the loop's context-overflow detection still matches the gateway's message.
pub(super) fn chat_error_to_step(err: ChatError) -> StepError {
    if err.transient {
        StepError::transient(err.error, err.retry_after)
    } else {
        StepError::terminal(format!("{:#}", err.error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Backoff is exponential, capped, and jittered deterministically (no clock/RNG in tests).
    #[test]
    fn backoff_is_exponential_capped_and_deterministic() {
        let p = RetryPolicy {
            max_retries: 5,
            base_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(2),
        };
        // attempt 0 ≈ 100ms, attempt 1 ≈ 200ms, attempt 2 ≈ 400ms (plus <250ms jitter).
        assert!(p.backoff(0) >= Duration::from_millis(100));
        assert!(p.backoff(0) < Duration::from_millis(350));
        assert!(p.backoff(2) >= Duration::from_millis(400));
        // High attempt is capped near max_backoff (+ jitter), never unbounded.
        assert!(p.backoff(20) <= Duration::from_secs(2) + Duration::from_millis(250));
        // Deterministic: same input → same output.
        assert_eq!(p.backoff(3), p.backoff(3));
    }
}
