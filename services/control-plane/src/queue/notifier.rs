//! The `notifier` role (RFC-0006 Phase 3, ADR-0079 §4/§5): the SSRF-guarded webhook-delivery actor.
//!
//! This is the control plane's **first outbound egress to caller-controlled, arbitrary-internet
//! URLs**, so it is the security-sensitive half of push notifications. The Phase-2 `a2a_task_events`
//! log ([ADR-0077](../../docs/adr/0077-a2a-streaming-event-log.md)) is the durable, ordered queue;
//! each `a2a_push_configs` row carries its own `delivered_seq` cursor into it. This role turns that
//! log into webhook POSTs — a *consumer* of the log, adding nothing to the `set_task_status` hot path.
//!
//! ## Delivery discipline (mirrors the dispatcher's claim/lease)
//!
//! A poll loop drains all currently-due configs, then waits a short interval. Retries are due-time
//! based (`next_attempt_at`), so — unlike the per-task streaming tail — there is no global
//! `LISTEN/NOTIFY`: the streaming notify is per-task and does not fit a global worker (a global notify
//! would be a *latency* optimization only, not a correctness need, so it is noted but not built).
//!
//! Per config, exactly once at a time (ADR-0079 P5/P8):
//! 1. [`crate::db::claim_next_push_config`] takes a config with work due under a lease
//!    (`FOR UPDATE SKIP LOCKED` + `lease_expires_at`), so no two replicas deliver the same config.
//! 2. It delivers every event past `delivered_seq` **in `seq` order, one at a time**, advancing the
//!    cursor after each success. A failure on event N blocks the config at N (backoff) and never skips
//!    it, so ordering holds (backpressure, not reorder).
//! 3. At each delivery it **re-validates and pins** the URL (DNS-rebinding/TOCTOU — §2/P2), decrypts
//!    the caller token (§3) — **failing the attempt closed** if a configured token cannot be decrypted
//!    rather than POSTing without it — and POSTs through the hardened, redirect-disabled, IP-pinned
//!    client.
//!
//! ## SSRF hardening (the load-bearing part — ADR-0079 §2)
//!
//! The actual egress lives behind the [`WebhookSender`] trait so the delivery loop (SSRF re-validate +
//! token decrypt + cursor/retry bookkeeping) is testable with a mock, while the real
//! [`ReqwestWebhookSender`] is the hardened client: **redirects disabled** (a `302 → 169.254.169.254`
//! is the canonical bypass), a total timeout, and the socket **pinned to the SSRF-validated IP** via
//! `reqwest`'s `resolve` override so the connection target is exactly the address that was checked —
//! never a fresh re-resolution that could differ.
//!
//! At-least-once, documented: a crash between a successful POST and the `delivered_seq` advance
//! re-delivers that event (a duplicate, never a loss or reorder). The POST carries a stable event id
//! `{a2a_task_id}:{seq}` (the `X-A2A-Notification-Id` header) so the receiver can dedupe.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use sqlx::PgPool;

use crate::a2a::push_crypto::{self, Key};
use crate::a2a::ssrf::{self, SsrfPolicy, ValidatedWebhook};
use crate::db;

/// HTTP header carrying the stable, per-event notification id `{a2a_task_id}:{seq}` (ADR-0079 §4/P6),
/// so an at-least-once receiver can dedupe re-delivered events.
pub const NOTIFICATION_ID_HEADER: &str = "X-A2A-Notification-Id";

/// Consecutive failed attempts before a config is dead-lettered (`state = 'disabled'`) and stops being
/// delivered (ADR-0079 §4/P7). A persistently-failing webhook is disabled, not retried forever; the
/// caller can re-create/re-enable it.
const DEFAULT_MAX_ATTEMPTS: i32 = 8;
/// Delivery lease: how long a claim holds a config before another worker may re-claim it. Kept
/// comfortably longer than a single POST's total timeout, and renewed after each successful event so a
/// multi-event catch-up never loses the lease mid-flight.
const DEFAULT_LEASE: Duration = Duration::from_secs(60);
/// Poll cadence: retries are due-time based, so a short poll is the primary driver (no global NOTIFY).
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(3);
/// Backoff = BASE × 2^(attempts−1), capped — mirrors the reaper's `requeue_backoff`.
const BACKOFF_BASE: Duration = Duration::from_secs(15);
const BACKOFF_CAP: Duration = Duration::from_secs(3600);
/// Head-of-line fairness cap (ADR-0079 P10): a single claim delivers at most this many events before
/// yielding its lease so other due configs get a turn on the single worker. One config with a huge
/// backlog therefore can't monopolize delivery — it resumes (from its cursor) on the next claim. High
/// enough that the common small catch-up completes in one claim.
const DEFAULT_MAX_EVENTS_PER_CLAIM: usize = 50;
/// Hardened HTTPS client timeouts (ADR-0079 §2): a slow/hostile receiver must not pin a worker.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(20);
/// The only port the SSRF policy accepts (`ssrf::ALLOWED_PORT` is private); pinning connects here.
const HTTPS_PORT: u16 = 443;

/// Tunable notifier loop timings + the dead-letter cutoff.
#[derive(Debug, Clone, Copy)]
pub struct NotifierConfig {
    /// Poll cadence between drains (retries are due-time based).
    pub poll_interval: Duration,
    /// Delivery lease per claimed config.
    pub lease: Duration,
    /// Consecutive-failure cutoff before dead-lettering a config.
    pub max_attempts: i32,
    /// Head-of-line fairness cap: events delivered per claim before yielding the lease (see the const).
    pub max_events_per_claim: usize,
}

impl Default for NotifierConfig {
    fn default() -> Self {
        Self {
            poll_interval: DEFAULT_POLL_INTERVAL,
            lease: DEFAULT_LEASE,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            max_events_per_claim: DEFAULT_MAX_EVENTS_PER_CLAIM,
        }
    }
}

impl NotifierConfig {
    /// Resolve from env, each unset/invalid field falling back to its default.
    pub fn from_env() -> Self {
        Self::from_env_with(|name| std::env::var(name).ok())
    }

    fn from_env_with(env: impl Fn(&str) -> Option<String>) -> Self {
        let secs = |name: &str, default: Duration| {
            env(name)
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|&s| s > 0)
                .map(Duration::from_secs)
                .unwrap_or(default)
        };
        let max_attempts = env("NOTIFIER_MAX_ATTEMPTS")
            .and_then(|v| v.parse::<i32>().ok())
            .filter(|&m| m > 0)
            .unwrap_or(DEFAULT_MAX_ATTEMPTS);
        let max_events_per_claim = env("NOTIFIER_MAX_EVENTS_PER_CLAIM")
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&m| m > 0)
            .unwrap_or(DEFAULT_MAX_EVENTS_PER_CLAIM);
        Self {
            poll_interval: secs("NOTIFIER_POLL_SECS", DEFAULT_POLL_INTERVAL),
            lease: secs("NOTIFIER_LEASE_SECS", DEFAULT_LEASE),
            max_attempts,
            max_events_per_claim,
        }
    }
}

/// Exponential-with-cap backoff before a failed config's next attempt — the reaper's curve.
fn requeue_backoff(attempts: i32) -> Duration {
    let shift = attempts.clamp(1, 16) as u32 - 1;
    let secs = BACKOFF_BASE.as_secs().saturating_mul(1u64 << shift);
    Duration::from_secs(secs.min(BACKOFF_CAP.as_secs()))
}

/// A failed webhook send — every failure is retriable (the loop backs off and eventually
/// dead-letters), so this carries only a human-readable cause, never the token or response body.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SendError(pub String);

/// The actual outbound POST, abstracted so the delivery loop is testable without real egress (a mock
/// records calls and returns programmed outcomes) while the real impl is the hardened, IP-pinned,
/// redirect-disabled client. The loop does the SSRF re-validate + token decrypt itself and hands the
/// sender an already-[`ValidatedWebhook`] (with its pinned IPs) — so the sender only connects to the
/// checked address.
#[async_trait]
pub trait WebhookSender: Send + Sync {
    /// POST `payload` to `validated.url`, pinned to a `validated.pinned_ips` address, presenting
    /// `token` as `Authorization: Bearer <token>` when `Some`, and `event_id` as the notification-id
    /// header. `Ok(())` only on a 2xx; any non-2xx / timeout / connect error is a `SendError`.
    async fn send(
        &self,
        validated: &ValidatedWebhook,
        token: Option<&str>,
        event_id: &str,
        payload: &Value,
    ) -> Result<(), SendError>;
}

/// The hardened HTTPS webhook client (ADR-0079 §2). A fresh `reqwest::Client` is built per delivery so
/// the DNS `resolve` override pins the socket to *this* config's validated IP; delivery volume is low
/// (per-config, cursor-driven), so per-send construction is not a hot path.
pub struct ReqwestWebhookSender;

impl ReqwestWebhookSender {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ReqwestWebhookSender {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the hardened, IP-pinned client for one validated webhook: redirects disabled, connect+total
/// timeouts, and — for a domain host — the connect address overridden to the SSRF-validated, pinned IP
/// on port 443 so the socket target is exactly the checked address (never a fresh re-resolution). A
/// literal-IP host needs no override (it already connects to the checked literal). SNI/cert
/// verification still use the URL host, so TLS validates the hostname while connecting to the pinned IP.
fn build_pinned_client(validated: &ValidatedWebhook) -> reqwest::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(TOTAL_TIMEOUT);
    // Pin the connect to the validated IP for a domain host (DNS-rebinding defence). `host_str` is the
    // domain for a domain URL; for a literal IP there is nothing to override (the pin is the literal).
    if let (Some(host), Some(&ip)) = (validated.url.host_str(), validated.pinned_ips.first())
        && matches!(validated.url.host(), Some(url::Host::Domain(_)))
    {
        builder = builder.resolve(host, SocketAddr::new(ip, HTTPS_PORT));
    }
    builder.build()
}

#[async_trait]
impl WebhookSender for ReqwestWebhookSender {
    async fn send(
        &self,
        validated: &ValidatedWebhook,
        token: Option<&str>,
        event_id: &str,
        payload: &Value,
    ) -> Result<(), SendError> {
        let client = build_pinned_client(validated)
            .map_err(|error| SendError(format!("build webhook client: {error}")))?;
        let mut req = client
            .post(validated.url.clone())
            .header(NOTIFICATION_ID_HEADER, event_id)
            .json(payload);
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        let resp = req
            .send()
            .await
            .map_err(|error| SendError(format!("webhook POST failed: {error}")))?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            // We deliver, we don't read: the response body is not consumed beyond the status (§2).
            Err(SendError(format!("webhook returned status {status}")))
        }
    }
}

/// Run the notifier until cancelled. Drains all due configs, then waits `poll_interval` (or a shutdown
/// signal). `owner` identifies this replica in the lease (e.g. the pod name).
pub async fn run(
    pool: PgPool,
    sender: Arc<dyn WebhookSender>,
    key: Option<Key>,
    policy: SsrfPolicy,
    owner: String,
    cfg: NotifierConfig,
) -> anyhow::Result<()> {
    tracing::info!(owner, "notifier started (A2A push delivery)");
    loop {
        drain(&pool, sender.as_ref(), key.as_ref(), &policy, &owner, &cfg).await;
        tokio::select! {
            _ = tokio::time::sleep(cfg.poll_interval) => {}
            _ = shutdown_signal() => {
                tracing::info!(owner, "notifier received shutdown signal; stopping");
                break;
            }
        }
    }
    Ok(())
}

/// Claim and deliver every currently-due config, then return so the caller can wait.
async fn drain(
    pool: &PgPool,
    sender: &dyn WebhookSender,
    key: Option<&Key>,
    policy: &SsrfPolicy,
    owner: &str,
    cfg: &NotifierConfig,
) {
    loop {
        match deliver_next_due(pool, sender, key, policy, owner, cfg).await {
            Ok(true) => continue, // claimed one; look for more
            Ok(false) => return,  // nothing due
            Err(error) => {
                tracing::error!(%error, "notifier: claim failed");
                return;
            }
        }
    }
}

/// Claim the next due config and deliver all its pending events. Returns `Ok(true)` if a config was
/// claimed (so the drain keeps going), `Ok(false)` when nothing is due. A delivery *failure* within a
/// claimed config is handled internally (backoff / dead-letter) and still returns `Ok(true)` — the
/// error path here is only a claim (DB) failure. Pulled out of [`drain`] so tests can drive one
/// claim+deliver cycle deterministically.
pub async fn deliver_next_due(
    pool: &PgPool,
    sender: &dyn WebhookSender,
    key: Option<&Key>,
    policy: &SsrfPolicy,
    owner: &str,
    cfg: &NotifierConfig,
) -> Result<bool, sqlx::Error> {
    let Some(config) = db::claim_next_push_config(pool, owner, cfg.lease).await? else {
        return Ok(false);
    };
    deliver_claimed(pool, sender, key, policy, cfg, &config).await;
    Ok(true)
}

/// Deliver every event past a claimed config's cursor, in `seq` order, one at a time. Stops at the
/// first failure (backpressure — the config is backed off / dead-lettered and re-tried later, never
/// skipping the blocked event), or releases the lease once caught up.
async fn deliver_claimed(
    pool: &PgPool,
    sender: &dyn WebhookSender,
    key: Option<&Key>,
    policy: &SsrfPolicy,
    cfg: &NotifierConfig,
    config: &db::ClaimedPushConfig,
) {
    let mut delivered_seq = config.delivered_seq;
    let mut delivered_this_claim: usize = 0;
    loop {
        let next = match db::next_push_event(pool, config.a2a_task_id, delivered_seq).await {
            Ok(Some(event)) => event,
            Ok(None) => {
                // Caught up: release the lease so the config goes idle until a new event lands.
                if let Err(error) = db::release_push_config(pool, config.config_id).await {
                    tracing::warn!(%error, config_id = %config.config_id, "notifier: release lease failed");
                }
                return;
            }
            Err(error) => {
                tracing::error!(%error, config_id = %config.config_id, "notifier: fetch next event failed; lease will expire");
                return;
            }
        };
        let (seq, payload) = next;

        match deliver_one(sender, key, policy, config, seq, &payload).await {
            Ok(()) => {
                // Advance the cursor + renew the lease so this worker keeps catching up.
                if let Err(error) =
                    db::advance_push_delivered(pool, config.config_id, seq, cfg.lease).await
                {
                    tracing::error!(%error, config_id = %config.config_id, seq, "notifier: advance cursor failed; lease will expire and re-deliver (at-least-once)");
                    return;
                }
                delivered_seq = seq;
                delivered_this_claim += 1;

                // Head-of-line fairness (ADR-0079 P10): after a bounded run of events, yield the lease so
                // other due configs get a turn on the single worker. `advance_push_delivered` already set
                // `next_attempt_at = now()`, so this config is immediately re-claimable and resumes from
                // its cursor (`delivered_seq`) — no event is skipped, ordering holds. Without this, one
                // config with a large backlog could monopolize the worker until fully caught up.
                if delivered_this_claim >= cfg.max_events_per_claim {
                    if let Err(error) = db::release_push_config(pool, config.config_id).await {
                        tracing::warn!(%error, config_id = %config.config_id, "notifier: release lease on head-of-line yield failed (lease will expire)");
                    }
                    return;
                }
            }
            Err(error) => {
                record_failure(pool, cfg, config, seq, &error).await;
                return; // block at this seq; a later cycle retries it (never skip → ordering holds)
            }
        }
    }
}

/// Attempt one event's delivery: re-validate + pin the URL (DNS-rebinding/TOCTOU — §2/P2), resolve the
/// caller token (§3), and hand the sender the validated webhook. An SSRF re-block is a failed attempt
/// (the sender is never called). A configured token that fails to resolve (wrong/rotated/absent key) is
/// **also** a failed attempt — we fail CLOSED rather than POST without the auth the caller configured
/// (that could leak a task update to a receiver that only accepts authenticated calls, or let an
/// attacker who cannot forge the token still receive the payload). Never panics, never logs the token.
async fn deliver_one(
    sender: &dyn WebhookSender,
    key: Option<&Key>,
    policy: &SsrfPolicy,
    config: &db::ClaimedPushConfig,
    seq: i64,
    payload: &Value,
) -> Result<(), SendError> {
    // Re-validate at delivery and pin the connect to the checked IP (never a fresh re-resolution).
    let validated = ssrf::validate_webhook_url(&config.url, policy, ssrf::system_resolver)
        .map_err(|error| {
            // Not sent: a URL that now resolves private/invalid is a failed attempt, not a POST.
            SendError(format!("SSRF re-validation blocked delivery: {error}"))
        })?;

    // Fail closed on a token that won't decrypt: a `?` here makes it a failed attempt (backoff →
    // dead-letter), never an unauthenticated send.
    let token = resolve_token(config, key)?;
    let event_id = format!("{}:{}", config.a2a_task_id, seq);
    sender
        .send(&validated, token.as_deref(), &event_id, payload)
        .await
}

/// Resolve the config's stored auth token for presentation as a bearer, **failing closed** (ADR-0079
/// §3). Three outcomes:
/// - `Ok(None)`  — the config carries NO token: a tokenless webhook sends none, unchanged.
/// - `Ok(Some)`  — a stored token decrypted cleanly with the role key.
/// - `Err`       — a token IS configured but cannot be presented (wrong/rotated key, or no key
///   configured). We refuse to send: delivering without the caller's auth is worse than a retryable
///   failure. Warns with the `config_id` only — the token bytes and key are never logged.
fn resolve_token(
    config: &db::ClaimedPushConfig,
    key: Option<&Key>,
) -> Result<Option<String>, SendError> {
    let Some(token_enc) = config.token_enc.as_deref() else {
        return Ok(None); // tokenless config: nothing to present, send unauthenticated as configured
    };
    match key {
        Some(key) => match push_crypto::decrypt(token_enc, key) {
            Some(token) => Ok(Some(token)),
            None => {
                tracing::warn!(
                    config_id = %config.config_id,
                    "notifier: stored webhook token failed to decrypt (wrong/rotated key?); failing delivery closed (not sending unauthenticated)"
                );
                Err(SendError(
                    "configured webhook auth token failed to decrypt".to_string(),
                ))
            }
        },
        None => {
            tracing::warn!(
                config_id = %config.config_id,
                "notifier: webhook token present but no encryption key is configured; failing delivery closed (not sending unauthenticated)"
            );
            Err(SendError(
                "configured webhook auth token cannot be decrypted (no key)".to_string(),
            ))
        }
    }
}

/// Record a failed delivery: increment the consecutive-failure counter, then schedule the next attempt
/// with exponential backoff — or dead-letter (`disabled`) once the counter reaches `max_attempts`
/// (ADR-0079 §4/P7). The lease is held across the two writes (see [`db::bump_push_attempts`]) so no
/// other worker re-delivers the same event in the gap.
async fn record_failure(
    pool: &PgPool,
    cfg: &NotifierConfig,
    config: &db::ClaimedPushConfig,
    seq: i64,
    error: &SendError,
) {
    let attempts = match db::bump_push_attempts(pool, config.config_id).await {
        Ok(attempts) => attempts,
        Err(db_error) => {
            tracing::error!(%db_error, config_id = %config.config_id, seq, "notifier: failed to record delivery failure; lease will expire and retry");
            return;
        }
    };
    let disable = attempts >= cfg.max_attempts;
    let backoff = requeue_backoff(attempts);
    if disable {
        tracing::error!(
            config_id = %config.config_id, seq, attempts, %error,
            "notifier: webhook dead-lettered after repeated failures (disabled)"
        );
    } else {
        tracing::warn!(
            config_id = %config.config_id, seq, attempts, backoff_secs = backoff.as_secs(), %error,
            "notifier: webhook delivery failed; backing off"
        );
    }
    if let Err(db_error) = db::schedule_push_retry(pool, config.config_id, backoff, disable).await {
        tracing::error!(%db_error, config_id = %config.config_id, "notifier: failed to schedule retry; lease will expire and retry");
    }
}

/// Resolves on SIGTERM (Kubernetes pod termination) or Ctrl-C, for a clean notifier shutdown between
/// delivery cycles — mirrors the dispatcher's signal handling.
#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(error) => {
            tracing::warn!(%error, "could not install SIGTERM handler");
            return std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::warn!(%error, "could not install Ctrl-C handler");
        std::future::pending::<()>().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Mutex;
    use uuid::Uuid;

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(requeue_backoff(1), Duration::from_secs(15));
        assert_eq!(requeue_backoff(2), Duration::from_secs(30));
        assert_eq!(requeue_backoff(3), Duration::from_secs(60));
        assert_eq!(requeue_backoff(99), BACKOFF_CAP);
    }

    #[test]
    fn config_from_env_defaults_and_overrides() {
        let config =
            |poll: Option<&str>, lease: Option<&str>, attempts: Option<&str>, cap: Option<&str>| {
                NotifierConfig::from_env_with(|name| match name {
                    "NOTIFIER_POLL_SECS" => poll.map(str::to_string),
                    "NOTIFIER_LEASE_SECS" => lease.map(str::to_string),
                    "NOTIFIER_MAX_ATTEMPTS" => attempts.map(str::to_string),
                    "NOTIFIER_MAX_EVENTS_PER_CLAIM" => cap.map(str::to_string),
                    _ => None,
                })
            };

        let cfg = config(None, None, None, None);
        assert_eq!(cfg.poll_interval, DEFAULT_POLL_INTERVAL);
        assert_eq!(cfg.lease, DEFAULT_LEASE);
        assert_eq!(cfg.max_attempts, DEFAULT_MAX_ATTEMPTS);
        assert_eq!(cfg.max_events_per_claim, DEFAULT_MAX_EVENTS_PER_CLAIM);

        // A zero is invalid and falls back to the default (never a busy-loop / zero-attempt state).
        let cfg = config(Some("7"), Some("0"), Some("3"), Some("5"));
        assert_eq!(cfg.poll_interval, Duration::from_secs(7));
        assert_eq!(cfg.lease, DEFAULT_LEASE);
        assert_eq!(cfg.max_attempts, 3);
        assert_eq!(cfg.max_events_per_claim, 5);
    }

    /// The hardened client builds for both a domain (pinned via `resolve`) and a literal-IP webhook.
    /// We can't assert the private resolver mapping directly, but a successful build proves the
    /// redirect-none + timeouts + pin wiring is well-formed.
    #[test]
    fn pinned_client_builds_for_domain_and_literal() {
        use std::net::{IpAddr, Ipv4Addr};
        let domain = ValidatedWebhook {
            url: url::Url::parse("https://hooks.example.com/a2a").unwrap(),
            pinned_ips: vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))],
        };
        assert!(build_pinned_client(&domain).is_ok());
        let literal = ValidatedWebhook {
            url: url::Url::parse("https://93.184.216.34/a2a").unwrap(),
            pinned_ips: vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))],
        };
        assert!(build_pinned_client(&literal).is_ok());
    }

    // ── Delivery-loop integration (needs Postgres via DATABASE_URL) ────────────────────────────────

    /// A mock sender: records every call `(event_id, token)` and fails delivery of any `seq` whose id
    /// ends with a listed suffix. Never touches the network — this is what lets the loop's ordering /
    /// cursor / retry / dead-letter / no-double-send be tested without fighting the SSRF policy.
    #[derive(Default)]
    struct MockSender {
        calls: Mutex<Vec<(String, Option<String>)>>,
        /// `seq` values to fail on (by numeric suffix of the event id).
        fail_seqs: HashSet<i64>,
    }

    impl MockSender {
        fn failing(seqs: &[i64]) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail_seqs: seqs.iter().copied().collect(),
            }
        }
        fn event_ids(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(id, _)| id.clone())
                .collect()
        }
    }

    #[async_trait]
    impl WebhookSender for MockSender {
        async fn send(
            &self,
            _validated: &ValidatedWebhook,
            token: Option<&str>,
            event_id: &str,
            _payload: &Value,
        ) -> Result<(), SendError> {
            self.calls
                .lock()
                .unwrap()
                .push((event_id.to_string(), token.map(str::to_string)));
            let seq: i64 = event_id
                .rsplit(':')
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(-1);
            if self.fail_seqs.contains(&seq) {
                Err(SendError(format!("mock failure for seq {seq}")))
            } else {
                Ok(())
            }
        }
    }

    fn test_cfg() -> NotifierConfig {
        NotifierConfig {
            poll_interval: Duration::from_millis(10),
            lease: Duration::from_secs(60),
            max_attempts: 3,
            max_events_per_claim: DEFAULT_MAX_EVENTS_PER_CLAIM,
        }
    }

    fn test_key() -> Key {
        Key::from_bytes(&[7u8; 32]).unwrap()
    }

    /// Seed an `a2a_tasks` mapping row (the FK parent for events + configs) and return its id.
    async fn seed_a2a_task(pool: &PgPool) -> Uuid {
        let id = Uuid::now_v7();
        let task = serde_json::json!({
            "id": id.to_string(),
            "contextId": "ctx-test",
            "status": { "state": "TASK_STATE_WORKING" }
        });
        sqlx::query(
            "INSERT INTO a2a_tasks (a2a_task_id, context_id, caller_id, skill, state, version, task_json) \
             VALUES ($1, 'ctx-test', 'svc-a', 'review', 'TASK_STATE_WORKING', 1, $2)",
        )
        .bind(id)
        .bind(&task)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    /// Append `n` events (seq 1..=n) to a task; the last is `final`.
    async fn seed_events(pool: &PgPool, a2a_task_id: Uuid, n: i64) {
        for seq in 1..=n {
            let payload = serde_json::json!({ "statusUpdate": { "taskId": a2a_task_id.to_string(), "seq": seq } });
            sqlx::query(
                "INSERT INTO a2a_task_events (a2a_task_id, seq, kind, state, final, payload) \
                 VALUES ($1, $2, 'status-update', 'TASK_STATE_WORKING', $3, $4)",
            )
            .bind(a2a_task_id)
            .bind(seq)
            .bind(seq == n)
            .bind(&payload)
            .execute(pool)
            .await
            .unwrap();
        }
    }

    async fn insert_config(
        pool: &PgPool,
        a2a_task_id: Uuid,
        url: &str,
        token_enc: Option<&[u8]>,
    ) -> Uuid {
        let config_id = Uuid::now_v7();
        db::insert_push_config(pool, config_id, a2a_task_id, url, token_enc, "svc-a")
            .await
            .unwrap();
        config_id
    }

    /// Read (delivered_seq, attempts, state) for assertions.
    async fn config_state(pool: &PgPool, config_id: Uuid) -> (i64, i32, String) {
        sqlx::query_as(
            "SELECT delivered_seq, attempts, state FROM a2a_push_configs WHERE config_id = $1",
        )
        .bind(config_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    const PUBLIC_URL: &str = "https://93.184.216.34/hook"; // literal public IP → no DNS at delivery

    /// Happy path: N events delivered in seq order, cursor advances to N, attempts stay 0.
    #[sqlx::test(migrations = "./migrations")]
    async fn delivers_all_events_in_order(pool: PgPool) {
        let task = seed_a2a_task(&pool).await;
        seed_events(&pool, task, 3).await;
        let config = insert_config(&pool, task, PUBLIC_URL, None).await;
        let sender = MockSender::default();
        let cfg = test_cfg();

        // One claim delivers the whole catch-up (events 1..3) while holding the lease.
        assert!(
            deliver_next_due(
                &pool,
                &sender,
                None,
                &SsrfPolicy::default(),
                "owner-a",
                &cfg
            )
            .await
            .unwrap()
        );

        assert_eq!(
            sender.event_ids(),
            vec![
                format!("{task}:1"),
                format!("{task}:2"),
                format!("{task}:3"),
            ],
            "events delivered strictly in seq order"
        );
        let (delivered, attempts, state) = config_state(&pool, config).await;
        assert_eq!((delivered, attempts, state.as_str()), (3, 0, "active"));

        // Nothing left due.
        assert!(
            !deliver_next_due(
                &pool,
                &sender,
                None,
                &SsrfPolicy::default(),
                "owner-a",
                &cfg
            )
            .await
            .unwrap()
        );
    }

    /// Regression: `advance_push_delivered` is strictly monotonic. A stale worker whose lease
    /// expired mid-delivery (after another worker re-claimed and advanced the config) cannot rewind
    /// the cursor by writing a lower seq — the `AND delivered_seq < $2` guard makes it a no-op, so no
    /// already-delivered event is re-sent out of order.
    #[sqlx::test(migrations = "./migrations")]
    async fn advance_delivered_is_monotonic(pool: PgPool) {
        let task = seed_a2a_task(&pool).await;
        let config = insert_config(&pool, task, PUBLIC_URL, None).await;
        let lease = std::time::Duration::from_secs(60);

        db::advance_push_delivered(&pool, config, 5, lease)
            .await
            .unwrap();
        assert_eq!(config_state(&pool, config).await.0, 5);

        // A stale write of a LOWER seq must not rewind the cursor.
        db::advance_push_delivered(&pool, config, 3, lease)
            .await
            .unwrap();
        assert_eq!(
            config_state(&pool, config).await.0,
            5,
            "cursor must not rewind on a stale lower-seq advance"
        );

        // A forward write still advances.
        db::advance_push_delivered(&pool, config, 7, lease)
            .await
            .unwrap();
        assert_eq!(config_state(&pool, config).await.0, 7);
    }

    /// Backpressure: a failure on event 2 blocks the config at seq 1 (does NOT skip 2); attempts
    /// increments and next_attempt_at moves out. A later success delivers 2 then 3.
    #[sqlx::test(migrations = "./migrations")]
    async fn failure_blocks_at_seq_and_does_not_skip(pool: PgPool) {
        let task = seed_a2a_task(&pool).await;
        seed_events(&pool, task, 3).await;
        let config = insert_config(&pool, task, PUBLIC_URL, None).await;
        let cfg = test_cfg();

        // Sender fails seq 2. Delivery: 1 ok (cursor→1), 2 fails (blocked, backoff), stop.
        let failing = MockSender::failing(&[2]);
        deliver_next_due(
            &pool,
            &failing,
            None,
            &SsrfPolicy::default(),
            "owner-a",
            &cfg,
        )
        .await
        .unwrap();
        assert_eq!(
            failing.event_ids(),
            vec![format!("{task}:1"), format!("{task}:2")]
        );
        let (delivered, attempts, state) = config_state(&pool, config).await;
        assert_eq!(
            (delivered, attempts, state.as_str()),
            (1, 1, "active"),
            "blocked at 1, not skipped to 2"
        );

        // The backoff pushed next_attempt_at into the future, so it is NOT due right now.
        assert!(
            !deliver_next_due(
                &pool,
                &MockSender::default(),
                None,
                &SsrfPolicy::default(),
                "owner-a",
                &cfg
            )
            .await
            .unwrap(),
            "backed-off config is not due yet"
        );

        // Simulate the backoff elapsing, then a healthy sender delivers 2 then 3 in order.
        sqlx::query("UPDATE a2a_push_configs SET next_attempt_at = now() - interval '1 second' WHERE config_id = $1")
            .bind(config)
            .execute(&pool)
            .await
            .unwrap();
        let healthy = MockSender::default();
        deliver_next_due(
            &pool,
            &healthy,
            None,
            &SsrfPolicy::default(),
            "owner-a",
            &cfg,
        )
        .await
        .unwrap();
        assert_eq!(
            healthy.event_ids(),
            vec![format!("{task}:2"), format!("{task}:3")],
            "resumes at 2, in order"
        );
        let (delivered, attempts, state) = config_state(&pool, config).await;
        assert_eq!((delivered, attempts, state.as_str()), (3, 0, "active"));
    }

    /// Dead-letter: repeated failure on the first event → after max_attempts the config is disabled
    /// and stops being claimed.
    #[sqlx::test(migrations = "./migrations")]
    async fn repeated_failure_dead_letters_the_config(pool: PgPool) {
        let task = seed_a2a_task(&pool).await;
        seed_events(&pool, task, 1).await;
        let config = insert_config(&pool, task, PUBLIC_URL, None).await;
        let cfg = test_cfg(); // max_attempts = 3
        let policy = SsrfPolicy::default();

        for expected_attempts in 1..=3 {
            let failing = MockSender::failing(&[1]);
            let claimed = deliver_next_due(&pool, &failing, None, &policy, "owner-a", &cfg)
                .await
                .unwrap();
            assert!(
                claimed,
                "attempt {expected_attempts} should still claim the active config"
            );
            assert_eq!(failing.event_ids(), vec![format!("{task}:1")]);
            // Make it due again for the next loop (bypass the backoff wait).
            sqlx::query("UPDATE a2a_push_configs SET next_attempt_at = now() - interval '1 second' WHERE config_id = $1")
                .bind(config)
                .execute(&pool)
                .await
                .unwrap();
        }
        let (_, attempts, state) = config_state(&pool, config).await;
        assert_eq!(attempts, 3);
        assert_eq!(state, "disabled", "dead-lettered after max_attempts");

        // A disabled config is never claimed again, even when due.
        assert!(
            !deliver_next_due(
                &pool,
                &MockSender::default(),
                None,
                &policy,
                "owner-a",
                &cfg
            )
            .await
            .unwrap(),
            "disabled config is not claimed"
        );
    }

    /// No double-send: two concurrent claim+deliver cycles on the same config deliver each event
    /// exactly once (lease + SKIP LOCKED), never twice.
    #[sqlx::test(migrations = "./migrations")]
    async fn concurrent_claims_do_not_double_send(pool: PgPool) {
        let task = seed_a2a_task(&pool).await;
        seed_events(&pool, task, 3).await;
        let config = insert_config(&pool, task, PUBLIC_URL, None).await;
        let cfg = test_cfg();
        let sender_a = Arc::new(MockSender::default());
        let sender_b = Arc::new(MockSender::default());

        let (pa, pb) = (pool.clone(), pool.clone());
        let (sa, sb) = (sender_a.clone(), sender_b.clone());
        let (ca, cb) = (cfg, cfg);
        let ta = tokio::spawn(async move {
            deliver_next_due(
                &pa,
                sa.as_ref(),
                None,
                &SsrfPolicy::default(),
                "owner-a",
                &ca,
            )
            .await
        });
        let tb = tokio::spawn(async move {
            deliver_next_due(
                &pb,
                sb.as_ref(),
                None,
                &SsrfPolicy::default(),
                "owner-b",
                &cb,
            )
            .await
        });
        ta.await.unwrap().unwrap();
        tb.await.unwrap().unwrap();

        // Across BOTH workers, each seq is delivered exactly once — never duplicated by the other.
        let mut all: Vec<String> = sender_a.event_ids();
        all.extend(sender_b.event_ids());
        all.sort();
        assert_eq!(
            all,
            vec![
                format!("{task}:1"),
                format!("{task}:2"),
                format!("{task}:3")
            ],
            "each event delivered exactly once across concurrent workers"
        );
        let (delivered, _, _) = config_state(&pool, config).await;
        assert_eq!(delivered, 3);
    }

    /// SSRF re-validate at delivery: a config whose URL resolves to a private IP is NOT sent (the mock
    /// sender is never called) and is treated as a failed attempt.
    #[sqlx::test(migrations = "./migrations")]
    async fn private_url_is_reblocked_at_delivery_and_not_sent(pool: PgPool) {
        let task = seed_a2a_task(&pool).await;
        seed_events(&pool, task, 1).await;
        // A private literal — the create handler would reject this at registration, but the notifier
        // must ALSO re-block it at delivery (DNS-rebinding/TOCTOU defence). Inserted directly to
        // simulate a URL that resolves private at delivery time.
        let config = insert_config(&pool, task, "https://10.0.0.5/hook", None).await;
        let sender = MockSender::default();
        let cfg = test_cfg();

        deliver_next_due(
            &pool,
            &sender,
            None,
            &SsrfPolicy::default(),
            "owner-a",
            &cfg,
        )
        .await
        .unwrap();

        assert!(
            sender.calls.lock().unwrap().is_empty(),
            "a private-resolving URL is never POSTed"
        );
        let (delivered, attempts, state) = config_state(&pool, config).await;
        assert_eq!(
            (delivered, attempts, state.as_str()),
            (0, 1, "active"),
            "SSRF re-block is a failed attempt"
        );
    }

    /// Token: an encrypted token is decrypted and passed to the sender as the bearer; a tokenless
    /// config sends none.
    #[sqlx::test(migrations = "./migrations")]
    async fn token_is_decrypted_and_passed_as_bearer(pool: PgPool) {
        let key = test_key();
        let task = seed_a2a_task(&pool).await;
        seed_events(&pool, task, 1).await;
        let token_enc = push_crypto::encrypt(b"s3cr3t-bearer", &key);
        let config = insert_config(&pool, task, PUBLIC_URL, Some(&token_enc)).await;
        let sender = MockSender::default();
        let cfg = test_cfg();

        deliver_next_due(
            &pool,
            &sender,
            Some(&key),
            &SsrfPolicy::default(),
            "owner-a",
            &cfg,
        )
        .await
        .unwrap();

        let bearer = {
            let calls = sender.calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            calls[0].1.clone()
        };
        assert_eq!(
            bearer.as_deref(),
            Some("s3cr3t-bearer"),
            "the decrypted token is the bearer"
        );
        let (delivered, _, _) = config_state(&pool, config).await;
        assert_eq!(delivered, 1);

        // A second, tokenless config sends no bearer.
        let task2 = seed_a2a_task(&pool).await;
        seed_events(&pool, task2, 1).await;
        insert_config(&pool, task2, PUBLIC_URL, None).await;
        let sender2 = MockSender::default();
        deliver_next_due(
            &pool,
            &sender2,
            Some(&key),
            &SsrfPolicy::default(),
            "owner-a",
            &cfg,
        )
        .await
        .unwrap();
        assert_eq!(
            sender2.calls.lock().unwrap()[0].1,
            None,
            "a tokenless config sends no bearer"
        );
    }

    /// Fail CLOSED on a token that won't decrypt (ADR-0079 §3): a config whose `token_enc` was
    /// encrypted under a DIFFERENT key (a rotated/wrong key) is NOT delivered — the sender is never
    /// called — and it records a failed attempt (backoff → eventually dead-letter). Delivering the
    /// payload WITHOUT the caller's configured auth would be worse than a retryable failure.
    #[sqlx::test(migrations = "./migrations")]
    async fn undecryptable_token_fails_closed_and_is_not_sent(pool: PgPool) {
        let role_key = test_key();
        let other_key = Key::from_bytes(&[9u8; 32]).unwrap(); // a different (rotated) key
        let task = seed_a2a_task(&pool).await;
        seed_events(&pool, task, 1).await;
        // Store a token the ROLE key cannot decrypt (encrypted under `other_key`).
        let token_enc = push_crypto::encrypt(b"s3cr3t-bearer", &other_key);
        let config = insert_config(&pool, task, PUBLIC_URL, Some(&token_enc)).await;
        let sender = MockSender::default();
        let cfg = test_cfg();

        deliver_next_due(
            &pool,
            &sender,
            Some(&role_key),
            &SsrfPolicy::default(),
            "owner-a",
            &cfg,
        )
        .await
        .unwrap();

        assert!(
            sender.calls.lock().unwrap().is_empty(),
            "a config whose token won't decrypt is NEVER POSTed (no unauthenticated fallback)"
        );
        let (delivered, attempts, state) = config_state(&pool, config).await;
        assert_eq!(
            (delivered, attempts, state.as_str()),
            (0, 1, "active"),
            "the undecryptable-token config records a failed attempt, cursor unmoved"
        );
    }

    /// Head-of-line fairness (ADR-0079 P10): a claim delivers at most `max_events_per_claim` events,
    /// then yields the lease (clearing `lease_owner`) so other configs get a turn. The config resumes
    /// from its cursor on the next claim — no event skipped, ordering preserved.
    #[sqlx::test(migrations = "./migrations")]
    async fn claim_yields_after_max_events_per_claim(pool: PgPool) {
        let task = seed_a2a_task(&pool).await;
        seed_events(&pool, task, 5).await;
        let config = insert_config(&pool, task, PUBLIC_URL, None).await;
        let cfg = NotifierConfig {
            max_events_per_claim: 2,
            ..test_cfg()
        };

        // First claim delivers exactly 2 (the cap), then yields the lease.
        let first = MockSender::default();
        deliver_next_due(&pool, &first, None, &SsrfPolicy::default(), "owner-a", &cfg)
            .await
            .unwrap();
        assert_eq!(
            first.event_ids(),
            vec![format!("{task}:1"), format!("{task}:2")],
            "capped at 2 events this claim"
        );
        let (delivered, _, _) = config_state(&pool, config).await;
        assert_eq!(delivered, 2, "cursor advanced to the cap");
        // Lease was yielded (cleared), so the config is immediately re-claimable.
        let lease_owner: Option<String> =
            sqlx::query_scalar("SELECT lease_owner FROM a2a_push_configs WHERE config_id = $1")
                .bind(config)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(lease_owner, None, "lease released on head-of-line yield");

        // Next claim resumes at 3 and delivers the next 2 (3,4); a third claim finishes 5.
        let second = MockSender::default();
        deliver_next_due(
            &pool,
            &second,
            None,
            &SsrfPolicy::default(),
            "owner-a",
            &cfg,
        )
        .await
        .unwrap();
        assert_eq!(
            second.event_ids(),
            vec![format!("{task}:3"), format!("{task}:4")],
            "resumes from the cursor, in order"
        );
        let third = MockSender::default();
        deliver_next_due(&pool, &third, None, &SsrfPolicy::default(), "owner-a", &cfg)
            .await
            .unwrap();
        assert_eq!(third.event_ids(), vec![format!("{task}:5")]);
        let (delivered, _, state) = config_state(&pool, config).await;
        assert_eq!(
            (delivered, state.as_str()),
            (5, "active"),
            "all 5 delivered"
        );
    }

    /// At-least-once: if the cursor advance is lost after a successful POST (simulated by rewinding
    /// `delivered_seq`), the event is re-delivered (a duplicate) rather than skipped — never a loss.
    #[sqlx::test(migrations = "./migrations")]
    async fn lost_advance_redelivers_at_least_once(pool: PgPool) {
        let task = seed_a2a_task(&pool).await;
        seed_events(&pool, task, 2).await;
        let config = insert_config(&pool, task, PUBLIC_URL, None).await;
        let cfg = test_cfg();

        let first = MockSender::default();
        deliver_next_due(&pool, &first, None, &SsrfPolicy::default(), "owner-a", &cfg)
            .await
            .unwrap();
        assert_eq!(
            first.event_ids(),
            vec![format!("{task}:1"), format!("{task}:2")]
        );

        // Simulate a crash that lost the seq-2 advance: rewind the cursor to 1 and make it due again.
        sqlx::query("UPDATE a2a_push_configs SET delivered_seq = 1, next_attempt_at = now() - interval '1 second', lease_owner = NULL, lease_expires_at = NULL WHERE config_id = $1")
            .bind(config)
            .execute(&pool)
            .await
            .unwrap();

        let again = MockSender::default();
        deliver_next_due(&pool, &again, None, &SsrfPolicy::default(), "owner-a", &cfg)
            .await
            .unwrap();
        assert_eq!(
            again.event_ids(),
            vec![format!("{task}:2")],
            "seq 2 is re-delivered (at-least-once), never lost"
        );
        let (delivered, _, _) = config_state(&pool, config).await;
        assert_eq!(delivered, 2);
    }
}
