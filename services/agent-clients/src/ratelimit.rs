//! AI-gateway rate-limit response headers (Envoy AI Gateway / "eaig") + `Retry-After` parsing.
//!
//! The gateway does **token-based** rate limiting on top of Envoy Gateway's global rate-limit API: an
//! ExtProc server reads `usage.*_tokens` off each OpenAI-schema response and a `BackendTrafficPolicy`
//! deducts that cost from a budget keyed by descriptors (model + our `x-code-intelligence-*`
//! attribution headers, epic #89). When the operator enables `enable_x_ratelimit_headers:
//! DRAFT_VERSION_03` on the gateway, every response carries the IETF draft-03 headers:
//!
//! - `X-RateLimit-Limit` — the quota for the window (token-bucket max; may be suffixed with the policy,
//!   e.g. `100, 100;w=60` — we read the leading integer)
//! - `X-RateLimit-Remaining` — budget left in the current window (post-deduction of the just-finished
//!   response, so it's a "what's left for next time" signal)
//! - `X-RateLimit-Reset` — seconds until the window resets
//!
//! and a 429 additionally carries `x-envoy-ratelimited: true`.
//!
//! **These headers are advisory observability only.** The reactive `429` + `Retry-After` path stays the
//! source of truth: the gateway budget is *global* across horizontally-scaled runners (RFC-0001), so a
//! single runner's snapshot is only a partial view of the shared budget and must never be treated as
//! authoritative. We use it to log budget burn and to *soft*-warn when a run is close to the ceiling.

use std::time::Duration;

/// Snapshot of the gateway's advertised rate-limit budget, parsed from one response's headers. Every
/// field is optional because the draft-03 headers are off unless enabled on the gateway — an all-`None`
/// snapshot ([`is_empty`](Self::is_empty)) just means "the gateway told us nothing", not "no budget".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RateLimitSnapshot {
    /// `X-RateLimit-Limit` — the window's quota (token-bucket max).
    pub limit: Option<u64>,
    /// `X-RateLimit-Remaining` — budget left in the current window.
    pub remaining: Option<u64>,
    /// `X-RateLimit-Reset` — time until the current window resets.
    pub reset: Option<Duration>,
    /// `x-envoy-ratelimited: true` — set by Envoy on a rate-limited (429) response.
    pub limited: bool,
}

impl RateLimitSnapshot {
    /// Parse the rate-limit headers off a response. Unknown/garbled values are simply dropped (this is
    /// advisory telemetry — a malformed header must never fail a request).
    pub fn from_headers(headers: &reqwest::header::HeaderMap) -> Self {
        Self {
            limit: leading_u64(headers, "x-ratelimit-limit"),
            remaining: leading_u64(headers, "x-ratelimit-remaining"),
            reset: leading_u64(headers, "x-ratelimit-reset").map(Duration::from_secs),
            limited: flag(headers, "x-envoy-ratelimited"),
        }
    }

    /// True when the gateway advertised no rate-limit state at all (headers disabled or absent).
    pub fn is_empty(&self) -> bool {
        self.limit.is_none() && self.remaining.is_none() && self.reset.is_none() && !self.limited
    }

    /// Fraction of the window's budget still available, in `[0.0, 1.0]`, when both `remaining` and a
    /// non-zero `limit` are known. `None` when we can't compute it.
    pub fn fraction_remaining(&self) -> Option<f64> {
        match (self.remaining, self.limit) {
            (Some(r), Some(l)) if l > 0 => Some((r as f64 / l as f64).clamp(0.0, 1.0)),
            _ => None,
        }
    }

    /// Whether the budget is running low — at or below `threshold` (a fraction in `[0.0, 1.0]`) of the
    /// window quota. A hint to soft-throttle/log, never a hard gate (the budget is shared, see module
    /// docs). `false` when the fraction is unknown (we don't warn on missing data).
    pub fn is_low(&self, threshold: f64) -> bool {
        self.fraction_remaining().is_some_and(|f| f <= threshold)
    }
}

/// Parse `Retry-After` expressed as an integer number of seconds (the form gateways use on 429s). The
/// HTTP-date form is ignored — callers fall back to their own backoff then.
pub fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    parse_leading_u64(value).map(Duration::from_secs)
}

/// The leading-integer value of a (case-insensitive) header, or `None` if absent/unparseable.
fn leading_u64(headers: &reqwest::header::HeaderMap, name: &str) -> Option<u64> {
    parse_leading_u64(headers.get(name)?.to_str().ok()?)
}

/// Whether a header is present and equal (case-insensitively) to `true`.
fn flag(headers: &reqwest::header::HeaderMap, name: &str) -> bool {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
}

/// Leading run of ASCII digits parsed as a `u64`. Tolerates the draft-03 quota-policy suffix
/// (`100, 100;w=60` → `100`) and trailing junk; returns `None` when the value doesn't start with a
/// digit (e.g. an HTTP-date `Retry-After`).
fn parse_leading_u64(s: &str) -> Option<u64> {
    let digits: String = s
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderMap;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                reqwest::header::HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn parses_full_draft03_snapshot() {
        let snap = RateLimitSnapshot::from_headers(&headers(&[
            ("x-ratelimit-limit", "1000"),
            ("x-ratelimit-remaining", "250"),
            ("x-ratelimit-reset", "42"),
        ]));
        assert_eq!(snap.limit, Some(1000));
        assert_eq!(snap.remaining, Some(250));
        assert_eq!(snap.reset, Some(Duration::from_secs(42)));
        assert!(!snap.limited);
        assert!(!snap.is_empty());
        assert_eq!(snap.fraction_remaining(), Some(0.25));
        assert!(snap.is_low(0.25));
        assert!(!snap.is_low(0.1));
    }

    #[test]
    fn tolerates_quota_policy_suffix() {
        // Envoy emits `<value>, <quota>;w=<window>` when a policy is attached; we read the leading int.
        let snap = RateLimitSnapshot::from_headers(&headers(&[
            ("x-ratelimit-limit", "100, 100;w=60"),
            ("x-ratelimit-remaining", "7, 100;w=60"),
        ]));
        assert_eq!(snap.limit, Some(100));
        assert_eq!(snap.remaining, Some(7));
    }

    #[test]
    fn empty_when_no_headers_and_flags_x_envoy_ratelimited() {
        assert!(RateLimitSnapshot::from_headers(&headers(&[])).is_empty());
        let limited = RateLimitSnapshot::from_headers(&headers(&[("x-envoy-ratelimited", "true")]));
        assert!(limited.limited);
        assert!(!limited.is_empty());
        assert!(
            !RateLimitSnapshot::from_headers(&headers(&[("x-envoy-ratelimited", "false")])).limited
        );
    }

    #[test]
    fn fraction_handles_unknown_and_zero_limit() {
        assert_eq!(RateLimitSnapshot::default().fraction_remaining(), None);
        let zero = RateLimitSnapshot {
            limit: Some(0),
            remaining: Some(0),
            ..Default::default()
        };
        assert_eq!(zero.fraction_remaining(), None);
        assert!(!zero.is_low(0.5));
    }

    #[test]
    fn retry_after_parses_seconds_but_not_dates() {
        assert_eq!(
            retry_after(&headers(&[("retry-after", "120")])),
            Some(Duration::from_secs(120))
        );
        // HTTP-date form is intentionally unsupported → None (caller uses its own backoff).
        assert_eq!(
            retry_after(&headers(&[(
                "retry-after",
                "Wed, 21 Oct 2026 07:28:00 GMT"
            )])),
            None
        );
        assert_eq!(retry_after(&headers(&[])), None);
    }
}
