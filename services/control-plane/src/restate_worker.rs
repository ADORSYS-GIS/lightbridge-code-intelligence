//! The `restate-worker` role (RFC-0005 / ADR-0074, ticket #296/#297).
//!
//! Stands up the Restate SDK HTTP endpoint that the Restate server discovers and invokes. It serves:
//!
//! - **`Health`** — a trivial durable service (`ping`) proving the endpoint serves and the `ctx.run`
//!   pattern links inside this binary.
//! - **`PlatformEgress`** (RFC-0005 Phase A / ADR-0074) — the pilot: a virtual object keyed
//!   `"{platform}:{installation}"` whose `post(outbox_id)` handler delivers one `outbox` intent. The
//!   engine's per-key exclusivity makes ADR-0059's single-writer-per-installation invariant *structural*
//!   (no more replicas=1 comment), and its per-invocation retry policy + one explicit dead-letter branch
//!   replace the drain's `attempts²` backoff. It is only *reached* when a producer routes egress to
//!   Restate (`egress.mode = restate`, [`crate::egress`]); with the default `drain` mode the object is
//!   still served/registered but never invoked, so the reconciler drain remains the sole egress.
//!
//! ## Determinism (RFC-0005)
//!
//! All I/O (sqlx, `CodePlatform`) runs inside `ctx.run` and is journaled; the payload is re-read from the
//! row and consumed *inside* the deliver step, so only a small outcome enum (a status + optional id) ever
//! enters the journal. Handler code outside `ctx.run` is deterministic (no wall-clock, no RNG, no Context
//! use inside `ctx.run`). There is no concurrent Context fan-out (sdk-rust #89): the handler awaits its
//! steps sequentially.
//!
//! ## Transport / TLS
//!
//! The endpoint is served over **plain HTTP (h2c)** — the Restate server ↔ SDK link does not need
//! TLS, so the serve path never touches a rustls crypto provider. This matters because the workspace
//! already links two providers (`ring` via sqlx, `aws-lc-rs` transitively via rmcp); `main` installs
//! `ring` as the process default up front, and `restate-sdk` adds no third stack (it is not on the
//! `aws-lc-rs` dependency path, and its default `rust_crypto` feature — used for request-identity
//! verification, not TLS — is pure-Rust). See the note in `main`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use restate_sdk::context::ContextSideEffects as _;
use restate_sdk::prelude::{
    Context, Endpoint, HandlerError, HandlerResult, HttpServer, Json, ObjectContext,
    RunFuture as _, RunRetryPolicy, TerminalError,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::AppState;
use crate::config::ReviewSection;
use crate::db::OutboxRow;
use crate::integrations::platform::{CodePlatform, Platform, RepoRef};

/// Default bind address for the Restate SDK endpoint. The Restate server connects here to discover
/// and invoke services; 9080 is the SDK's conventional port.
const DEFAULT_BIND: &str = "0.0.0.0:9080";

/// The single trivial durable service this role serves. `ping` echoes a greeting, wrapping the
/// (pure) response construction in exactly one `ctx.run` to exercise the durable-step journaling
/// pattern that real handlers will use.
#[restate_sdk::service]
trait Health {
    /// Return a greeting for `name`. Durable: the response is produced inside a journaled step.
    async fn ping(name: String) -> Result<String, HandlerError>;
}

struct HealthImpl;

impl Health for HealthImpl {
    async fn ping(&self, ctx: Context<'_>, name: String) -> Result<String, HandlerError> {
        // Durable step: wrap the side-effect in `ctx.run` so its result is journaled and replayed on
        // retry instead of re-executed. Here the "side-effect" is a pure value, which is enough to
        // prove the pattern compiles and the endpoint serves.
        // TODO(RFC-0005 Phase B): replace the pure value with a real sqlx `SELECT 1` via the pool.
        let greeting = ctx.run(|| async move { Ok(ping_response(&name)) }).await?;
        Ok(greeting)
    }
}

/// Pure response body for `Health/ping`. Factored out of the handler so it is unit-testable without a
/// live Restate server or a `Context`.
fn ping_response(name: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        "pong".to_string()
    } else {
        format!("pong {name}")
    }
}

// ── PlatformEgress virtual object (RFC-0005 Phase A / ADR-0074) ─────────────────────────────────────

/// The egress virtual object. Keyed `"{platform}:{installation}"` ([`crate::egress::egress_key`]); the
/// engine runs at most one `post` per key at a time — the structural form of ADR-0059's single-writer
/// egress. `post(outbox_id)` delivers exactly the intent that row describes.
#[restate_sdk::object]
trait PlatformEgress {
    /// Deliver the `outbox` row `outbox_id`: load + status-guard + post (one journaled step), then mark
    /// posted — or dead-letter on the retry ceiling / a permanent error. Idempotent: a row already
    /// settled (by a prior invocation or a mode-flip drain) is skipped, never re-posted.
    async fn post(outbox_id: i64) -> Result<(), HandlerError>;
}

/// Serves [`PlatformEgress`]. Holds exactly what delivery needs — the pool, the platform dispatch table
/// (ADR-0072), and the review-label config — cloned out of `AppState` at bind time. Clones are cheap
/// (pool + `Arc`s), taken fresh per journaled step so each `ctx.run` closure owns its captures.
struct PlatformEgressImpl {
    pool: PgPool,
    platforms: HashMap<Platform, Arc<dyn CodePlatform>>,
    review: Arc<ReviewSection>,
}

/// The small, journaled result of the deliver step — deliberately tiny (a status + an optional id), so
/// the (possibly multi-KB) review payload never enters the journal (RFC-0005 determinism rule).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum DeliverOutcome {
    /// The status guard fired: the row is absent (pruned) or already terminal. Idempotent no-op.
    Skip,
    /// Posted (or intentionally consumed, e.g. a deduped failure-notice → `platform_ref_id = None`).
    Posted { platform_ref_id: Option<i64> },
}

/// Pure pre-flight decision for the deliver step, factored out so the load / status-guard / terminal
/// branches are unit-testable without a DB, a live platform, or a Restate `Context`. `row = None` means
/// the status-guarded load found nothing pending (absent or already terminal) → skip. A row with no
/// registered platform impl is a *permanent* failure (retrying won't conjure an impl) → terminal.
enum Preflight {
    Skip,
    Terminal(String),
    Post,
}

fn preflight(row: Option<&OutboxRow>, platform_present: bool) -> Preflight {
    match row {
        None => Preflight::Skip,
        Some(row) if !platform_present => {
            Preflight::Terminal(format!("no platform implementation for {}", row.platform))
        }
        Some(_) => Preflight::Post,
    }
}

impl PlatformEgress for PlatformEgressImpl {
    async fn post(&self, ctx: ObjectContext<'_>, outbox_id: i64) -> Result<(), HandlerError> {
        // Step 1 — deliver: load + status-guard + post, all inside ONE journaled step. The payload is
        // re-read from the row and consumed here, so it never enters the journal (only `DeliverOutcome`
        // is journaled). Bounded retries via the engine's per-run policy replace the drain's `attempts²`
        // backoff; on exhaustion `ctx.run` yields a `TerminalError` → the dead-letter branch below. The
        // same ceiling (`OUTBOX_MAX_ATTEMPTS`) the drain dead-letters at is reused, so both paths give up
        // at the same point.
        let deliver = {
            let pool = self.pool.clone();
            let platforms = self.platforms.clone();
            let review = self.review.clone();
            ctx.run(|| async move { deliver_step(&pool, &platforms, &review, outbox_id).await })
                .retry_policy(
                    // Minute-scale backoff, matching the drain's recovery window. `default()` caps
                    // `max_duration` at 50s (6 retries finish in ~7s), which would dead-letter a
                    // rate-limit that recovers in minutes — where the drain (`attempts²` minutes)
                    // would still deliver. Widen the envelope: keep the same `OUTBOX_MAX_ATTEMPTS`
                    // ceiling, but let those attempts span up to an hour.
                    RunRetryPolicy::default()
                        .max_attempts(crate::db::OUTBOX_MAX_ATTEMPTS as u32)
                        .initial_delay(std::time::Duration::from_secs(60))
                        .max_delay(std::time::Duration::from_secs(15 * 60))
                        .max_duration(std::time::Duration::from_secs(60 * 60)),
                )
                .name("deliver")
                .await
        };

        match deliver {
            Ok(outcome) => match outcome.into_inner() {
                DeliverOutcome::Skip => {
                    tracing::debug!(
                        outbox_id,
                        "PlatformEgress: row not pending; skipping (idempotent)"
                    );
                    Ok(())
                }
                DeliverOutcome::Posted { platform_ref_id } => {
                    // Step 2 — mark posted (records the platform id for the ADR-0035 feedback join). A
                    // separate journaled step: an unconditional UPDATE by id (re-marking an
                    // already-posted row just re-stamps posted_at — harmless under replay).
                    let pool = self.pool.clone();
                    ctx.run(
                        || async move { mark_posted_step(&pool, outbox_id, platform_ref_id).await },
                    )
                    .name("mark_posted")
                    .await?;
                    Ok(())
                }
            },
            Err(terminal) => {
                // Retry ceiling reached, or a permanent error (unknown platform / kind / malformed
                // payload). Dead-letter: park the row `failed` (preserved for inspection), mirroring
                // `mark_outbox_failed`'s terminal state. Returning `Ok` completes the invocation so the
                // engine does not retry it forever against, e.g., a deleted PR.
                let message = terminal.message().to_string();
                tracing::warn!(outbox_id, error = %message, "PlatformEgress: dead-lettering intent");
                let pool = self.pool.clone();
                ctx.run(|| async move { dead_letter_step(&pool, outbox_id, &message).await })
                    .name("dead_letter")
                    .await?;
                Ok(())
            }
        }
    }
}

/// The deliver step's body (inside `ctx.run`): status-guarded load, then post via the **same**
/// [`crate::queue::reconciler::deliver`] the drain uses. A transient platform/DB error is returned as a
/// retryable [`HandlerError`] (the engine retries per policy); a permanent condition is a
/// [`TerminalError`] (no retry — straight to dead-letter).
async fn deliver_step(
    pool: &PgPool,
    platforms: &HashMap<Platform, Arc<dyn CodePlatform>>,
    review: &ReviewSection,
    outbox_id: i64,
) -> HandlerResult<Json<DeliverOutcome>> {
    let row = crate::db::load_pending_outbox_row(pool, outbox_id)
        .await
        .map_err(HandlerError::from)?;
    match preflight(
        row.as_ref(),
        row.as_ref()
            .is_some_and(|r| platforms.contains_key(&r.platform)),
    ) {
        Preflight::Skip => Ok(Json(DeliverOutcome::Skip)),
        Preflight::Terminal(reason) => Err(TerminalError::new(reason).into()),
        Preflight::Post => {
            // `row` is Some here (Post is only produced for Some) and its platform is registered.
            let row = row.expect("preflight returns Post only for a present row");
            let platform = platforms
                .get(&row.platform)
                .expect("preflight returns Post only when the platform impl is present");
            let repo = RepoRef {
                platform: row.platform,
                full_name: format!("{}/{}", row.owner, row.repo),
                platform_repo_id: 0,
                installation_id: row.installation_id,
            };
            match crate::queue::reconciler::deliver(pool, platform.as_ref(), &repo, review, &row)
                .await
            {
                Ok(platform_ref_id) => Ok(Json(DeliverOutcome::Posted { platform_ref_id })),
                // Transient (network / 5xx / rate-limit): retryable → the engine retries per policy.
                Err(error) => Err(HandlerError::from(error)),
            }
        }
    }
}

/// Mark the row `posted`, recording the platform id (review/comment) for the feedback join. Idempotent.
async fn mark_posted_step(
    pool: &PgPool,
    outbox_id: i64,
    platform_ref_id: Option<i64>,
) -> HandlerResult<()> {
    crate::db::mark_outbox_posted(pool, outbox_id, platform_ref_id)
        .await
        .map_err(HandlerError::from)?;
    Ok(())
}

/// Park the row `failed` (dead-letter) with the terminal error. Idempotent (unconditional UPDATE by id).
async fn dead_letter_step(pool: &PgPool, outbox_id: i64, error: &str) -> HandlerResult<()> {
    crate::db::mark_outbox_dead_letter(pool, outbox_id, error)
        .await
        .map_err(HandlerError::from)?;
    Ok(())
}

/// Resolve the SDK endpoint bind address: `RESTATE_WORKER_BIND` when set and non-empty, else
/// [`DEFAULT_BIND`]. Bound as a raw string so hostnames resolve via `ToSocketAddrs`, consistent with
/// the `serve` role's `BIND_ADDR` handling.
fn bind_addr() -> String {
    bind_addr_from(std::env::var("RESTATE_WORKER_BIND").ok())
}

fn bind_addr_from(value: Option<String>) -> String {
    match value {
        Some(v) if !v.trim().is_empty() => v,
        _ => DEFAULT_BIND.to_string(),
    }
}

/// The `restate-worker` role entrypoint. Serves the Restate SDK endpoint (plain h2c) with graceful
/// shutdown, and — like the other non-`serve` roles — stands up the metrics-only Axum listener so it
/// is scraped/observed the same way as `dispatcher`/`reconciler`.
///
/// `state` is accepted for parity with the other roles and to make the DB pool available to future
/// durable handlers; today only the metrics handle is used.
pub async fn run(state: AppState) -> anyhow::Result<()> {
    // Observable like the other headless roles: /metrics (+ /healthz) on METRICS_ADDR.
    crate::spawn_metrics_server(state.metrics.clone());

    let addr: SocketAddr = bind_addr().parse().map_err(|error| {
        anyhow::anyhow!("RESTATE_WORKER_BIND must be a socket address (host:port): {error}")
    })?;

    // Always serve `Health`. Serve `PlatformEgress` (ADR-0074) too when a pool + at least one platform
    // impl are available — the pilot's egress path needs both to load rows and post. Without them the
    // object simply isn't registered (the reconciler drain, which requires the same, stays the egress
    // path); with the default `egress.mode = drain` the object is registered but never invoked.
    let mut builder = Endpoint::builder().bind(HealthImpl.serve());
    match (state.db.clone(), state.platforms.is_empty()) {
        (Some(pool), false) => {
            let egress = PlatformEgressImpl {
                pool,
                platforms: state.platforms.clone(),
                review: state.review.clone(),
            };
            builder = builder.bind(egress.serve());
            tracing::info!("restate-worker: serving PlatformEgress virtual object (ADR-0074)");
        }
        (None, _) => tracing::warn!(
            "restate-worker: no database pool — PlatformEgress not served (Health only)"
        ),
        (Some(_), true) => tracing::warn!(
            "restate-worker: no platform implementation configured — PlatformEgress not served (Health only)"
        ),
    }
    let endpoint = builder.build();

    // Plain-HTTP listener; no TLS on the Restate server ↔ SDK link (see the module note).
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(addr = %addr, "restate-worker SDK endpoint listening (h2c)");

    // Graceful shutdown on SIGTERM/Ctrl-C, consistent with `dispatcher`/`reconciler`.
    HttpServer::new(endpoint)
        .serve_with_cancel(listener, shutdown_signal())
        .await;
    tracing::info!("restate-worker received shutdown signal; endpoint stopped");
    Ok(())
}

/// Resolves on SIGTERM (Kubernetes pod termination) or Ctrl-C. Mirrors the dispatcher's handler so
/// the `restate-worker` role shuts down on the same signals as the other headless roles.
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

    #[test]
    fn ping_response_greets_a_named_caller() {
        assert_eq!(ping_response("restate"), "pong restate");
    }

    #[test]
    fn ping_response_trims_surrounding_whitespace() {
        assert_eq!(ping_response("  restate  "), "pong restate");
    }

    #[test]
    fn ping_response_falls_back_to_bare_pong_when_blank() {
        assert_eq!(ping_response(""), "pong");
        assert_eq!(ping_response("   "), "pong");
    }

    #[test]
    fn bind_addr_resolves_default_blank_and_override() {
        assert_eq!(
            bind_addr_from(None),
            DEFAULT_BIND,
            "unset should fall back to default"
        );

        assert_eq!(
            bind_addr_from(Some("   ".to_string())),
            DEFAULT_BIND,
            "blank should fall back to default"
        );

        assert_eq!(
            bind_addr_from(Some("127.0.0.1:19080".to_string())),
            "127.0.0.1:19080",
            "a set value wins"
        );
    }

    // The default and override addresses must be valid `SocketAddr`s — otherwise `run` bails before
    // it ever binds. This covers the parse path without needing a live Restate server.
    #[test]
    fn resolved_bind_addr_parses_as_a_socket_addr() {
        use std::net::SocketAddr;
        assert!(DEFAULT_BIND.parse::<SocketAddr>().is_ok());
        assert!("127.0.0.1:19080".parse::<SocketAddr>().is_ok());
    }

    // ── PlatformEgress delivery decision (ADR-0074) ──────────────────────────────────────────────────

    fn sample_row(platform: Platform) -> OutboxRow {
        OutboxRow {
            id: 7,
            task_id: None,
            installation_id: 42,
            owner: "acme".to_string(),
            repo: "web".to_string(),
            kind: "reaction".to_string(),
            payload: serde_json::json!({ "issue": 1, "content": "eyes" }),
            attempts: 0,
            platform,
        }
    }

    // The status guard: a `None` row (absent OR already terminal — the load filters `status='pending'`)
    // must skip, never post. This is the no-double-send / no-re-post invariant of the Restate path.
    #[test]
    fn preflight_skips_when_the_row_is_absent_or_terminal() {
        assert!(matches!(preflight(None, false), Preflight::Skip));
        assert!(matches!(preflight(None, true), Preflight::Skip));
    }

    // A present row whose platform has no registered impl is a *permanent* failure → terminal (straight
    // to dead-letter), not an endless retry.
    #[test]
    fn preflight_dead_letters_a_row_with_no_platform_impl() {
        let row = sample_row(Platform::GitHub);
        match preflight(Some(&row), false) {
            Preflight::Terminal(reason) => assert!(reason.contains("no platform implementation")),
            _ => panic!("expected a terminal (dead-letter) decision"),
        }
    }

    // The happy path: present, pending, platform impl registered → post.
    #[test]
    fn preflight_posts_a_pending_row_with_a_registered_platform() {
        let row = sample_row(Platform::GitLab);
        assert!(matches!(preflight(Some(&row), true), Preflight::Post));
    }

    // `DeliverOutcome` is journaled, so it must survive a serde round-trip. It also must stay *small* —
    // only a status + an optional id — so the review payload never leaks into the journal.
    #[test]
    fn deliver_outcome_round_trips_through_json() {
        for outcome in [
            DeliverOutcome::Skip,
            DeliverOutcome::Posted {
                platform_ref_id: None,
            },
            DeliverOutcome::Posted {
                platform_ref_id: Some(999),
            },
        ] {
            let json = serde_json::to_string(&outcome).unwrap();
            let back: DeliverOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(outcome, back);
        }
    }

    // The step bodies that run *inside* `ctx.run` are ordinary async fns over the pool, so their DB
    // branches are testable without a live Restate server (the `Post` branch, which calls the live
    // platform, is exercised post-merge against real Restate — #297/#296). Covers: the deliver step's
    // Skip (absent row) and Terminal (no platform impl) branches, plus mark-posted and dead-letter.
    #[sqlx::test]
    async fn egress_step_bodies_guard_mark_and_dead_letter(pool: sqlx::PgPool) {
        let payload = serde_json::json!({ "issue": 1, "content": "eyes" });
        crate::db::enqueue_outbox_post(
            &pool,
            Platform::GitHub,
            None,
            42,
            "o",
            "r",
            "reaction",
            &payload,
            "worker-k1",
        )
        .await
        .unwrap();
        let id = crate::db::outbox_id_by_dedup_key(&pool, "worker-k1")
            .await
            .unwrap()
            .unwrap();
        let review = ReviewSection::default();

        // Deliver on an ABSENT id → Skip (the status guard's idempotent no-op).
        let empty: HashMap<Platform, Arc<dyn CodePlatform>> = HashMap::new();
        let skip = deliver_step(&pool, &empty, &review, 9_000_001)
            .await
            .unwrap()
            .into_inner();
        assert_eq!(skip, DeliverOutcome::Skip);

        // Deliver a PENDING row with NO registered platform impl → a permanent (terminal) error, so the
        // handler dead-letters it rather than retrying forever.
        assert!(
            deliver_step(&pool, &empty, &review, id).await.is_err(),
            "a row with no platform impl must be a terminal error"
        );

        // mark-posted settles the row out of the pending set (records the platform id).
        mark_posted_step(&pool, id, Some(777)).await.unwrap();
        assert!(
            crate::db::load_pending_outbox_row(&pool, id)
                .await
                .unwrap()
                .is_none()
        );

        // dead-letter parks a fresh pending row `failed`.
        crate::db::enqueue_outbox_post(
            &pool,
            Platform::GitHub,
            None,
            42,
            "o",
            "r",
            "reply",
            &payload,
            "worker-k2",
        )
        .await
        .unwrap();
        let dead = crate::db::outbox_id_by_dedup_key(&pool, "worker-k2")
            .await
            .unwrap()
            .unwrap();
        dead_letter_step(&pool, dead, "deleted PR").await.unwrap();
        assert!(
            crate::db::load_pending_outbox_row(&pool, dead)
                .await
                .unwrap()
                .is_none()
        );
    }
}
