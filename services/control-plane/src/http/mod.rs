//! HTTP surface — the `serve` role. GitHub webhooks, the runner-facing internal API, the dashboard
//! admin endpoints, and the Prometheus `/metrics` renderer. Routing and auth middleware are wired in
//! the crate root (`main.rs`).

pub(crate) mod admin;
pub(crate) mod config;
pub(crate) mod internal;
pub(crate) mod metrics;
pub(crate) mod webhook;
