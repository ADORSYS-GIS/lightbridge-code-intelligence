//! Producer-side egress routing (RFC-0005 Phase A / ADR-0074, ticket #297).
//!
//! Every producer keeps writing a fully-shaped `outbox` intent row first ([`crate::outbox`],
//! [`crate::db::enqueue_outbox_post`]) — that is the durable ledger and is **unchanged** by the pilot.
//! This module adds the *second* step producers take when the egress flag is flipped to `restate`:
//! after the intent row is enqueued, `send` a `PlatformEgress::post(outbox_id)` invocation to Restate,
//! so the per-`platform:installation` virtual object ([`crate::restate_worker`]) delivers it instead of
//! the reconciler drain.
//!
//! ## Default is a no-op
//!
//! [`EgressMode::Drain`] is the default. In drain mode [`PlatformEgressRouter::announce`] returns before
//! touching the pool or the network, so the producer path is byte-for-byte identical to today's — the
//! reconciler's `outbox` drain (ADR-0059) stays the sole egress. Merging the pilot changes no behavior;
//! flipping the flag to `restate` is what activates the new path.
//!
//! ## Idempotency & dedup
//!
//! The `send` carries idempotency key = the row's `dedup_key`, so a re-finalize (or a producer retry)
//! that re-enqueues the same intent also re-announces it under the same key — Restate collapses the
//! duplicate invocations exactly as `enqueue_outbox_post`'s `ON CONFLICT (dedup_key)` collapses the
//! duplicate rows. Only the `outbox_id` crosses the wire; the payload is re-read from the row inside the
//! handler's `ctx.run` (keeps journal entries small — RFC-0005 determinism rule).

use std::time::Duration;

use anyhow::Context as _;
use sqlx::PgPool;

use crate::config::{EgressMode, EgressSection};
use crate::integrations::platform::Platform;

/// Restate virtual-object service name. **Must** match the `#[restate_sdk::object] trait PlatformEgress`
/// served in [`crate::restate_worker`]: the SDK macro uses the trait ident verbatim (no case
/// conversion), so the served name and this producer-side name are the same string. Unit-tested against
/// [`send_url`] so the wire path can't silently drift from the served object.
pub const PLATFORM_EGRESS_SERVICE: &str = "PlatformEgress";
/// The object's single exclusive handler — the `post` method on the trait.
pub const POST_HANDLER: &str = "post";

/// Header Restate reads for invocation idempotency on its ingress.
const IDEMPOTENCY_HEADER: &str = "idempotency-key";
/// Cap on the ingress `send` — a fast in-cluster POST; a hang must not stall a producer.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Routes freshly-enqueued intents to their consumer (RFC-0005 Phase A / ADR-0074). Held in `AppState`
/// and shared by every producer (serve/finalize, dispatcher 👀, reaper 😕/failure-notice).
#[derive(Debug)]
pub struct PlatformEgressRouter {
    inner: Router,
}

#[derive(Debug)]
enum Router {
    /// Default: the reconciler drain owns egress. Announcing is a no-op — the `NOTIFY` inside
    /// `enqueue_outbox_post` already wakes the drain, so nothing more is needed.
    Drain,
    /// Pilot: `send` each intent to the `PlatformEgress` virtual object via Restate's HTTP ingress.
    Restate {
        client: reqwest::Client,
        /// Base URL of the Restate ingress, no trailing slash (e.g.
        /// `http://restate.converse.svc.cluster.local:8080`).
        ingress_base: String,
    },
}

impl PlatformEgressRouter {
    /// Build from the `egress` config section, falling back to `RESTATE_INGRESS_URL` for the ingress
    /// base. Fails loud when `mode = restate` but no ingress URL resolves — the pilot must not silently
    /// enqueue rows nothing will deliver (the drain is off in restate mode).
    pub fn from_config(section: &EgressSection) -> anyhow::Result<Self> {
        match section.mode {
            EgressMode::Drain => Ok(Self {
                inner: Router::Drain,
            }),
            EgressMode::Restate => {
                let ingress_base = section
                    .restate_ingress_url
                    .clone()
                    .or_else(|| std::env::var("RESTATE_INGRESS_URL").ok())
                    .map(|s| s.trim().trim_end_matches('/').to_string())
                    .filter(|s| !s.is_empty())
                    .context(
                        "egress.mode = restate requires egress.restate_ingress_url \
                         (or the RESTATE_INGRESS_URL env var)",
                    )?;
                let client = reqwest::Client::builder().timeout(HTTP_TIMEOUT).build()?;
                tracing::info!(
                    ingress_base = %ingress_base,
                    "egress: routing platform egress through Restate (PlatformEgress virtual object)"
                );
                Ok(Self {
                    inner: Router::Restate {
                        client,
                        ingress_base,
                    },
                })
            }
        }
    }

    /// The drain (no-op) router — the default, and the one tests/dev use when Restate isn't wired.
    pub fn disabled() -> Self {
        Self {
            inner: Router::Drain,
        }
    }

    /// True when egress flows through Restate (the pilot). False = the drain default.
    pub fn is_restate(&self) -> bool {
        matches!(self.inner, Router::Restate { .. })
    }

    /// Announce a just-enqueued intent so Restate delivers it. **No-op in drain mode** (returns `Ok`
    /// before any pool or network access — the default path is unchanged). In restate mode: resolve the
    /// row id for `dedup_key`, then `send` `PlatformEgress::post(outbox_id)` to the ingress with
    /// idempotency key = `dedup_key`.
    ///
    /// An error here is surfaced to the caller and handled exactly as an `enqueue` error is at that site
    /// (fatal for the review post → 500 → runner re-finalizes idempotently; best-effort for cosmetic
    /// reactions). The row is already durably persisted regardless, so a failed announce delays delivery
    /// (until a re-finalize re-announces, or the operator flips back to drain) — it never loses the row.
    pub async fn announce(
        &self,
        pool: &PgPool,
        platform: Platform,
        installation_id: i64,
        dedup_key: &str,
    ) -> anyhow::Result<()> {
        let Router::Restate {
            client,
            ingress_base,
        } = &self.inner
        else {
            return Ok(());
        };

        let Some(outbox_id) = crate::db::outbox_id_by_dedup_key(pool, dedup_key).await? else {
            // The row we (or a prior identical enqueue) just wrote should exist; a miss means it was
            // pruned between enqueue and announce — there is nothing to deliver, so this is not an error.
            tracing::warn!(
                dedup_key,
                "egress announce: no outbox row for dedup_key; skipping send"
            );
            return Ok(());
        };

        let key = egress_key(platform, installation_id);
        let url = send_url(ingress_base, &key);
        let resp = client
            .post(&url)
            .header(IDEMPOTENCY_HEADER, dedup_key)
            .json(&outbox_id)
            .send()
            .await
            .with_context(|| format!("POST {url} (PlatformEgress::post send)"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Restate ingress returned {status} for {url}: {body}");
        }
        tracing::debug!(
            outbox_id,
            key = %key,
            "egress: announced PlatformEgress::post to Restate"
        );
        Ok(())
    }
}

/// The virtual-object key: `"{platform}:{installation_id}"` — the granularity GitHub/GitLab rate limits
/// and abuse detection operate at, so per-key serialization is also rate-limit alignment (ADR-0074). The
/// virtual object guarantees at-most-one running `post` handler per key, replacing ADR-0059's
/// single-replica invariant with a structural one. Pure; unit-tested.
pub fn egress_key(platform: Platform, installation_id: i64) -> String {
    format!("{platform}:{installation_id}")
}

/// The Restate ingress URL for a one-way `send` of `PlatformEgress::post` to `key`:
/// `{base}/{service}/{key}/{handler}/send`. Keys are `{platform}:{id}` — only a colon, a valid path
/// character — so no percent-encoding is needed. Pure; unit-tested.
pub fn send_url(ingress_base: &str, key: &str) -> String {
    format!("{ingress_base}/{PLATFORM_EGRESS_SERVICE}/{key}/{POST_HANDLER}/send")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EgressSection;

    #[test]
    fn egress_key_is_platform_colon_installation() {
        assert_eq!(egress_key(Platform::GitHub, 12345), "github:12345");
        assert_eq!(egress_key(Platform::GitLab, 42), "gitlab:42");
    }

    #[test]
    fn send_url_matches_the_served_object_and_handler() {
        // Guards the wire path against drift from the served `PlatformEgress::post` object.
        assert_eq!(
            send_url("http://restate:8080", "github:99"),
            "http://restate:8080/PlatformEgress/github:99/post/send"
        );
    }

    #[test]
    fn default_mode_builds_a_drain_router_that_does_not_announce() {
        let router = PlatformEgressRouter::from_config(&EgressSection::default())
            .expect("drain is the default and needs no ingress URL");
        assert!(
            !router.is_restate(),
            "the default egress mode must be the drain (no behavior change on merge)"
        );
        assert!(!PlatformEgressRouter::disabled().is_restate());
    }

    #[test]
    fn restate_mode_requires_an_ingress_url() {
        // No URL configured and (in this test) RESTATE_INGRESS_URL unset → fail loud.
        std::env::remove_var("RESTATE_INGRESS_URL");
        let section = EgressSection {
            mode: EgressMode::Restate,
            restate_ingress_url: None,
        };
        let err = PlatformEgressRouter::from_config(&section)
            .expect_err("restate mode without an ingress URL must fail");
        assert!(
            err.to_string().contains("restate_ingress_url"),
            "the error should name the missing setting: {err}"
        );
    }

    #[test]
    fn restate_mode_trims_trailing_slash_from_the_configured_url() {
        let section = EgressSection {
            mode: EgressMode::Restate,
            restate_ingress_url: Some("http://restate:8080/".to_string()),
        };
        let router = PlatformEgressRouter::from_config(&section).expect("valid restate config");
        assert!(router.is_restate());
        // The trailing slash is trimmed so `send_url` doesn't produce a `//`.
        let Router::Restate { ingress_base, .. } = &router.inner else {
            unreachable!("just asserted restate")
        };
        assert_eq!(ingress_base, "http://restate:8080");
    }

    #[test]
    fn egress_mode_parses_from_config_json() {
        // The flag is spelled `drain` | `restate` (lowercase) in the config file.
        let drain: EgressMode = serde_json::from_str("\"drain\"").unwrap();
        assert_eq!(drain, EgressMode::Drain);
        let restate: EgressMode = serde_json::from_str("\"restate\"").unwrap();
        assert_eq!(restate, EgressMode::Restate);
        assert_eq!(EgressMode::default(), EgressMode::Drain);
    }
}
