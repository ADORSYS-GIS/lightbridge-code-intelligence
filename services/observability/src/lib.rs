//! Shared tracing bootstrap for the two long-running Rust binaries (`control-plane`,
//! `agent-runner`): the JSON `tracing_subscriber::fmt` setup both duplicated inline, now with an
//! optional OTLP→Tempo export layer on top (ticket #246). Log/metric pipelines are unchanged —
//! this crate only adds the distributed-tracing layer.
//!
//! Also carries the one genuinely new piece of plumbing the ticket calls for: threading a W3C
//! `traceparent` across boundaries that aren't a live call stack — a Kubernetes Job env var, a
//! Postgres row read back much later — via [`current_traceparent`] / [`set_remote_parent`].

use std::collections::HashMap;

use opentelemetry::global;
use opentelemetry::trace::{TraceContextExt, TracerProvider as _};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::prelude::*;

/// Pin the process-default rustls crypto provider before anything builds a TLS client. Moved here
/// (was inline in `control-plane/src/main.rs`) so both binaries share one audited call site —
/// `agent-runner` pulls both `ring` and `aws-lc-rs` in its production binary too (via the
/// workspace's `rustls` feature pin and `reqwest`'s `rustls-platform-verifier` respectively) and
/// had no pin at all before this crate existed. Idempotent: a benign `Err` means a provider is
/// already installed (e.g. a re-entrant test), which is left in place.
pub fn install_default_crypto_provider() {
    if rustls::crypto::ring::default_provider()
        .install_default()
        .is_err()
    {
        tracing::warn!("a rustls CryptoProvider was already installed; keeping the existing one");
    }
}

/// Holds the OTel tracer provider (if one was built) so the caller can flush it before the
/// process exits. Call [`OtelGuard::shutdown`] on every exit path — success, failure, and any
/// signal-triggered early return — since a short-lived process (the agent-runner Job) that skips
/// it loses its still-batched spans.
pub struct OtelGuard {
    provider: Option<SdkTracerProvider>,
}

impl OtelGuard {
    /// Flush and shut down the tracer provider. No-op when no OTLP endpoint was configured (the
    /// fmt-only path). Runs the (synchronous) SDK shutdown call on a blocking thread so it never
    /// stalls the async runtime.
    pub async fn shutdown(self) {
        let Some(provider) = self.provider else {
            return;
        };
        let result = tokio::task::spawn_blocking(move || provider.shutdown()).await;
        match result {
            Ok(Err(error)) => {
                tracing::warn!(%error, "otel tracer provider shutdown reported an error")
            }
            Err(error) => tracing::warn!(%error, "otel tracer provider shutdown task panicked"),
            Ok(Ok(())) => {}
        }
    }
}

/// Install the process-wide `tracing` subscriber: the existing JSON `fmt` layer + `EnvFilter`
/// (unchanged from today), plus an OTLP export layer **only when** `OTEL_EXPORTER_OTLP_ENDPOINT`
/// is set — required external config absent (no Tempo endpoint configured) means fmt-only,
/// byte-identical to today, not a policy toggle. Call [`install_default_crypto_provider`] first.
///
/// Sampling ratio (`OTEL_TRACES_SAMPLER_ARG`, standard OTel env var name, default `0.1`) is
/// applied via `Sampler::ParentBased(TraceIdRatioBased)`: the decision is made once wherever the
/// trace starts (an unparented root span) and every descendant — including ones re-parented from
/// a stored `traceparent` in a Job env var or a DB row — mechanically inherits it via the
/// traceparent's own flags byte, with no coordination needed between this process's sampler and
/// any other process's.
pub fn init_tracing(service_name: &str) -> OtelGuard {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer().json();

    let mut provider = None;
    let otel_layer = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .ok()
        .and_then(|endpoint| match build_tracer_provider(service_name, &endpoint) {
            Ok(built) => {
                let tracer = built.tracer(service_name.to_string());
                provider = Some(built);
                Some(tracing_opentelemetry::layer().with_tracer(tracer))
            }
            Err(error) => {
                eprintln!(
                    "lci-observability: OTLP init failed ({error:#}); continuing without trace export"
                );
                None
            }
        });

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .with(otel_layer)
        .init();

    if provider.is_some() {
        global::set_text_map_propagator(TraceContextPropagator::new());
    }

    OtelGuard { provider }
}

/// Builds the tracer provider. Two things here are load-bearing fixes, not style choices —
/// confirmed against a real local OTLP collector while implementing #246 (a run that failed
/// silently either way, until both were found).
///
/// Fix one: a **Tokio-integrated batch processor**
/// (`experimental_trace_batch_span_processor_with_async_runtime` + `runtime::Tokio`), not
/// `SdkTracerProviderBuilder::with_batch_exporter`'s default. The default processor exports from its
/// own dedicated OS thread, not a Tokio task — a plain async `reqwest::Client` used there panics
/// outright on first export ("there is no reactor running, must be called from the context of a
/// Tokio 1.x runtime"). Running the processor as an ordinary Tokio task instead gives the client a
/// real reactor.
///
/// Fix two: the **OTLP/HTTP path suffix**. `OTEL_EXPORTER_OTLP_ENDPOINT` is documented (and
/// conventionally set) as a *base* URL (e.g. `http://tempo:4318`) — the exporter must append the
/// signal-specific `/v1/traces` itself; `opentelemetry-otlp` 0.32's `with_endpoint` does NOT do this
/// automatically. Missing it sends every export to the base path, which a real collector 404s —
/// surfaced by the SDK's own error handling as a generic, misleading "network error" with no status
/// code, not an HTTP error, which is what made this easy to mistake for a transport/runtime bug
/// during debugging rather than a wrong URL.
///
/// Both were reproduced and fixed by actually running this against a live collector — including
/// across a real process boundary (a spawned child process picking up a `TRACEPARENT` env var, not
/// just same-process spans) — not inferred from documentation.
fn build_tracer_provider(service_name: &str, endpoint: &str) -> anyhow::Result<SdkTracerProvider> {
    use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig, WithHttpConfig};
    use opentelemetry_sdk::runtime::Tokio;
    use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;

    let ratio: f64 = std::env::var("OTEL_TRACES_SAMPLER_ARG")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0.1);

    // Build our own reqwest client — after the caller has pinned the crypto provider — rather than
    // letting opentelemetry-otlp construct its own internal client, so this exporter rides the same
    // audited TLS stack as the rest of the binary instead of an independently-resolved one.
    // Allow insecure TLS to match Alloy collector's configuration.
    let http_client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?;

    // `endpoint` is the configured BASE url; append the OTLP/HTTP traces path ourselves (see doc
    // comment above).
    let traces_endpoint = format!("{}/v1/traces", endpoint.trim_end_matches('/'));

    let exporter = SpanExporter::builder()
        .with_http()
        .with_endpoint(traces_endpoint)
        .with_protocol(Protocol::HttpBinary)
        .with_http_client(http_client)
        .build()?;

    let processor = BatchSpanProcessor::builder(exporter, Tokio).build();

    let resource = opentelemetry_sdk::Resource::builder()
        .with_service_name(service_name.to_string())
        .build();

    Ok(SdkTracerProvider::builder()
        .with_span_processor(processor)
        .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
            ratio,
        ))))
        .with_resource(resource)
        .build())
}

/// The current span's OTel context, serialized as a W3C `traceparent` header value
/// (`version-traceid-spanid-flags`). `None` when the current span carries no valid OTel context
/// (e.g. the OTel layer isn't installed — `OTEL_EXPORTER_OTLP_ENDPOINT` unset). This is the value
/// to persist wherever a trace needs to survive a boundary `tracing`'s own span stack can't cross:
/// a Job env var, a `tasks`/`outbox` row read back later.
pub fn current_traceparent() -> Option<String> {
    let cx = tracing::Span::current().context();
    if !cx.span().span_context().is_valid() {
        return None;
    }
    let mut carrier = HashMap::new();
    global::get_text_map_propagator(|propagator| propagator.inject_context(&cx, &mut carrier));
    carrier.remove("traceparent")
}

/// Re-parent `span` under a remote `traceparent` (e.g. one read back from a Job's `TRACEPARENT`
/// env var, or a `tasks`/`outbox` row's stored `trace_context`). A `None`/unparseable traceparent
/// is a no-op — the span just starts its own (independently sampled) root, which is the correct,
/// graceful degradation when no upstream context is available (e.g. an outbox row not tied to any
/// task).
pub fn set_remote_parent(span: &tracing::Span, traceparent: Option<&str>) {
    let Some(traceparent) = traceparent else {
        return;
    };
    let mut carrier = HashMap::new();
    carrier.insert("traceparent".to_string(), traceparent.to_string());
    let cx = global::get_text_map_propagator(|propagator| propagator.extract(&carrier));
    if let Err(error) = span.set_parent(cx) {
        tracing::warn!(%error, "failed to set remote span parent from traceparent");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A registry with the OTel layer wired to a no-export tracer provider, scoped to the closure
    /// via `tracing::subscriber::with_default` — deliberately NOT `init()` (that's a process-global,
    /// call-once-per-process operation unsuitable for tests run in the same binary).
    fn with_test_subscriber<R>(f: impl FnOnce() -> R) -> R {
        global::set_text_map_propagator(TraceContextPropagator::new());
        let provider = SdkTracerProvider::builder().build();
        let tracer = provider.tracer("test");
        let subscriber =
            tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer));
        tracing::subscriber::with_default(subscriber, f)
    }

    fn trace_id_of(traceparent: &str) -> &str {
        traceparent
            .split('-')
            .nth(1)
            .expect("traceparent has a trace-id field")
    }

    #[test]
    fn current_traceparent_round_trips_into_a_child_span_with_the_same_trace_id() {
        with_test_subscriber(|| {
            let root = tracing::info_span!("root");
            let _root_enter = root.enter();
            let root_tp = current_traceparent().expect("entered span should carry an otel context");
            assert!(
                root_tp.starts_with("00-"),
                "unexpected traceparent shape: {root_tp}"
            );

            let child = tracing::info_span!("child");
            set_remote_parent(&child, Some(&root_tp));
            let _child_enter = child.enter();
            let child_tp =
                current_traceparent().expect("re-parented span should carry an otel context");

            assert_eq!(
                trace_id_of(&root_tp),
                trace_id_of(&child_tp),
                "child must share the root's trace-id"
            );
            assert_ne!(root_tp, child_tp, "child must have its own span-id");
        });
    }

    #[test]
    fn set_remote_parent_with_none_is_a_graceful_noop() {
        with_test_subscriber(|| {
            let span = tracing::info_span!("standalone");
            set_remote_parent(&span, None);
            let _enter = span.enter();
            // Starts its own root rather than panicking or silently dropping context.
            assert!(current_traceparent().is_some());
        });
    }

    #[test]
    fn current_traceparent_is_none_without_the_otel_layer() {
        // No `with_test_subscriber` here — the default/no-op subscriber carries no otel context.
        let span = tracing::info_span!("no_otel_layer");
        let _enter = span.enter();
        assert_eq!(current_traceparent(), None);
    }
}
