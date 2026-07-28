//! The `replay` role (ADR-0087): owner of the `durable_step` store's lifecycle.
//!
//! The store is written by the agent-plane (through the mediated internal API) when it runs under
//! `CheckpointRuntime`, and it is self-purging by construction:
//!
//! - **Purge-on-success** — `finalize_review` drops a run's whole journal the moment it finalizes
//!   (`crate::db::purge_durable_steps`). That is the fast path; it lives on the write side so the
//!   state is gone immediately.
//! - **TTL sweep** — this role periodically deletes rows older than `DURABLE_STEP_RETENTION`, success
//!   OR failure — the backstop for abandoned/failed/cancelled runs (the k8s Job
//!   `ttlSecondsAfterFinished` it functionally replaces for the loop).
//!
//! Additive + prod-neutral: not wired into Helm here, and a no-op while every run is `Passthrough`.

use std::time::Duration;

use sqlx::PgPool;

/// Default retention if `DURABLE_STEP_RETENTION` is unset (ADR-0087: "default e.g. 6 h"). Long enough
/// to outlive a stuck run's whole retry budget, short enough that abandoned state doesn't linger.
pub const DEFAULT_RETENTION: Duration = Duration::from_secs(6 * 60 * 60);

/// How often the TTL sweep runs. The sweep is a single indexed `DELETE`, so a frequent tick is cheap.
const SWEEP_INTERVAL: Duration = Duration::from_secs(300);

/// Why a configured `DURABLE_STEP_RETENTION` was rejected (ADR-0087's required load-time guard).
/// Scoped to this module: both distinct string-error cases the old `Result<_, String>` signature
/// carried, now typed so a caller gets a real `std::error::Error` (and `?`/`anyhow` interop) instead
/// of a bare string.
#[derive(Debug, thiserror::Error)]
pub enum ReplayConfigError {
    /// A zero or negative retention makes the age cutoff `now()`, which would sweep EVERY in-flight
    /// run's journal on the next tick and silently disable resume — a config footgun caught here, at
    /// load, not as a runtime surprise.
    #[error(
        "DURABLE_STEP_RETENTION must be > 0 seconds (got {secs}); a non-positive retention would \
         sweep in-flight durable-step state on every tick and disable resume (ADR-0087)"
    )]
    NonPositive { secs: i64 },
    /// The raw env value did not parse as an integer number of seconds.
    #[error("DURABLE_STEP_RETENTION must be an integer number of seconds (got {raw:?})")]
    NotAnInteger { raw: String },
}

/// Validate a configured retention (ADR-0087's **required load-time guard**): it MUST be `> 0`. A
/// zero or negative retention makes the age cutoff `now()`, which would sweep EVERY in-flight run's
/// journal on the next tick and silently disable resume — a config footgun caught here, at load, not
/// as a runtime surprise. Pure, so the guard is unit-tested without a DB.
pub fn retention_from_secs(secs: i64) -> Result<Duration, ReplayConfigError> {
    if secs <= 0 {
        return Err(ReplayConfigError::NonPositive { secs });
    }
    Ok(Duration::from_secs(secs as u64))
}

/// Read `DURABLE_STEP_RETENTION` (seconds) into a validated `Duration`. An unset/unparseable value
/// falls back to [`DEFAULT_RETENTION`]; a **set but non-positive** value is REJECTED (the required
/// guard) rather than silently clamped, so a misconfiguration fails loud at startup.
pub fn retention_from_env() -> Result<Duration, ReplayConfigError> {
    match std::env::var("DURABLE_STEP_RETENTION") {
        Ok(raw) => {
            let secs: i64 = raw
                .trim()
                .parse()
                .map_err(|_| ReplayConfigError::NotAnInteger { raw })?;
            retention_from_secs(secs)
        }
        Err(_) => Ok(DEFAULT_RETENTION),
    }
}

/// Run the TTL sweep loop until the process is stopped. Ticks every [`SWEEP_INTERVAL`], deleting
/// journal rows older than `retention`. Errors are logged, never fatal — the next tick retries.
pub async fn run(pool: PgPool, retention: Duration) -> anyhow::Result<()> {
    let retention_secs = retention.as_secs_f64();
    tracing::info!(
        retention_secs,
        sweep_interval_secs = SWEEP_INTERVAL.as_secs(),
        "replay role: durable-step TTL sweep starting"
    );
    let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
    // After a long stall (e.g. DB unavailable) don't fire a burst of catch-up ticks all at once —
    // one sweep per interval is enough; skip the missed ones.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        match crate::db::sweep_durable_steps(&pool, retention_secs).await {
            Ok(0) => {}
            Ok(removed) => {
                tracing::info!(
                    removed,
                    "replay role: TTL sweep removed stale durable-step rows"
                );
            }
            Err(error) => {
                tracing::error!(%error, "replay role: durable-step TTL sweep failed (retrying next tick)");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_rejects_zero_and_negative() {
        // The load-time guard: a `0` cutoff is `now()` → would sweep in-flight state (ADR-0087).
        assert!(retention_from_secs(0).is_err());
        assert!(retention_from_secs(-1).is_err());
        assert!(retention_from_secs(-3600).is_err());
    }

    #[test]
    fn retention_accepts_positive_values() {
        assert_eq!(retention_from_secs(1).unwrap(), Duration::from_secs(1));
        assert_eq!(
            retention_from_secs(6 * 60 * 60).unwrap(),
            Duration::from_secs(21_600)
        );
    }

    #[test]
    fn rejection_message_names_the_knob_and_the_hazard() {
        let err = retention_from_secs(0).unwrap_err().to_string();
        assert!(
            err.contains("DURABLE_STEP_RETENTION"),
            "names the knob: {err}"
        );
        assert!(err.contains("resume"), "names the hazard: {err}");
    }
}
