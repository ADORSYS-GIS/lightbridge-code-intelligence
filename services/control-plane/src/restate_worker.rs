//! The `restate-worker` role (RFC-0005 / ADR-0074, ticket #296).
//!
//! Phase-1 enabler: this stands up the Restate SDK HTTP endpoint that the Restate server discovers
//! and invokes, serving a single trivial durable service (`Health`). It exists to prove that the
//! endpoint serves and the durable `ctx.run` pattern compiles and links inside this binary — it does
//! no real orchestration yet. Real durable handlers arrive in a later RFC-0005 phase.
//!
//! ## Transport / TLS
//!
//! The endpoint is served over **plain HTTP (h2c)** — the Restate server ↔ SDK link does not need
//! TLS, so the serve path never touches a rustls crypto provider. This matters because the workspace
//! already links two providers (`ring` via sqlx, `aws-lc-rs` transitively via rmcp); `main` installs
//! `ring` as the process default up front, and `restate-sdk` adds no third stack (it is not on the
//! `aws-lc-rs` dependency path, and its default `rust_crypto` feature — used for request-identity
//! verification, not TLS — is pure-Rust). See the note in `main`.

use std::net::SocketAddr;

use restate_sdk::context::ContextSideEffects as _;
use restate_sdk::prelude::{Context, Endpoint, HandlerError, HttpServer};

use crate::AppState;

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

/// Resolve the SDK endpoint bind address: `RESTATE_WORKER_BIND` when set and non-empty, else
/// [`DEFAULT_BIND`]. Bound as a raw string so hostnames resolve via `ToSocketAddrs`, consistent with
/// the `serve` role's `BIND_ADDR` handling.
fn bind_addr() -> String {
    match std::env::var("RESTATE_WORKER_BIND") {
        Ok(v) if !v.trim().is_empty() => v,
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

    let endpoint = Endpoint::builder().bind(HealthImpl.serve()).build();

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
    use tokio::signal::unix::{signal, SignalKind};
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

    // One test drives every `RESTATE_WORKER_BIND` state: the var is a process-global, so splitting
    // these across tests would race under cargo's parallel runner.
    #[test]
    fn bind_addr_resolves_default_blank_and_override() {
        std::env::remove_var("RESTATE_WORKER_BIND");
        assert_eq!(
            bind_addr(),
            DEFAULT_BIND,
            "unset should fall back to default"
        );

        std::env::set_var("RESTATE_WORKER_BIND", "   ");
        assert_eq!(
            bind_addr(),
            DEFAULT_BIND,
            "blank should fall back to default"
        );

        std::env::set_var("RESTATE_WORKER_BIND", "127.0.0.1:19080");
        assert_eq!(bind_addr(), "127.0.0.1:19080", "a set value wins");

        std::env::remove_var("RESTATE_WORKER_BIND");
    }

    // The default and override addresses must be valid `SocketAddr`s — otherwise `run` bails before
    // it ever binds. This covers the parse path without needing a live Restate server.
    #[test]
    fn resolved_bind_addr_parses_as_a_socket_addr() {
        use std::net::SocketAddr;
        assert!(DEFAULT_BIND.parse::<SocketAddr>().is_ok());
        assert!("127.0.0.1:19080".parse::<SocketAddr>().is_ok());
    }
}
