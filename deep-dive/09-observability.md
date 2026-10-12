# 09 — Observability

Two independent systems carry telemetry in this codebase, and they are deliberately not merged:
**OpenTelemetry tracing** (distributed spans, exported to Tempo) and **Prometheus metrics** (`/metrics`,
scraped by Alloy). This doc covers the shared tracing crate, the cross-process trace-propagation
mechanism it adds, the `/metrics` surface, and what the committed Grafana dashboards actually watch.

## 1. The shared crate: `services/observability`

Both long-running Rust binaries (`control-plane`, `agent-runner`) used to duplicate their own JSON
`tracing_subscriber::fmt` setup inline. `lci-observability` is that setup factored out, plus an
optional OTLP→Tempo export layer:

```rust
//! Shared tracing bootstrap for the two long-running Rust binaries (`control-plane`,
//! `agent-runner`): the JSON `tracing_subscriber::fmt` setup both duplicated inline, now with an
//! optional OTLP→Tempo export layer on top (ticket #246). Log/metric pipelines are unchanged —
//! this crate only adds the distributed-tracing layer.
```
(`services/observability/src/lib.rs:1-4`)

`init_tracing(service_name: &str) -> OtelGuard` is the entry point. Its behaviour is deliberately a
hard fork on one env var:

```rust
/// Install the process-wide `tracing` subscriber: the existing JSON `fmt` layer + `EnvFilter`
/// (unchanged from today), plus an OTLP export layer **only when** `OTEL_EXPORTER_OTLP_ENDPOINT`
/// is set — required external config absent (no Tempo endpoint configured) means fmt-only,
/// byte-identical to today, not a policy toggle.
pub fn init_tracing(service_name: &str) -> OtelGuard {
```
(`services/observability/src/lib.rs:61-72`)

If `OTEL_EXPORTER_OTLP_ENDPOINT` is unset, nothing about trace export is wired in at all — logs still
go to stdout as JSON via `tracing_subscriber::fmt().json()`, but there's no tracer provider, no
OTel layer, and `current_traceparent()` always returns `None` (proven directly by the test
`current_traceparent_is_none_without_the_otel_layer`, `services/observability/src/lib.rs:263-269`).

### The two "load-bearing fixes"

`build_tracer_provider`'s doc comment is explicit that two of its details are bug fixes discovered
against a real collector, not style choices — and it says so in exactly those words:

```rust
/// Builds the tracer provider. Two things here are load-bearing fixes, not style choices —
/// confirmed against a real local OTLP collector while implementing #246 (a run that failed
/// silently either way, until both were found).
```
(`services/observability/src/lib.rs:107-109`)

**Fix one — the Tokio-integrated batch span processor.** The SDK's default batch exporter runs on
its own dedicated OS thread, not inside a Tokio task:

```rust
/// Fix one: a **Tokio-integrated batch processor**
/// (`experimental_trace_batch_span_processor_with_async_runtime` + `runtime::Tokio`), not
/// `SdkTracerProviderBuilder::with_batch_exporter`'s default. The default processor exports from its
/// own dedicated OS thread, not a Tokio task — a plain async `reqwest::Client` used there panics
/// outright on first export ("there is no reactor running, must be called from the context of a
/// Tokio 1.x runtime"). Running the processor as an ordinary Tokio task instead gives the client a
/// real reactor.
```
(`services/observability/src/lib.rs:111-117`)

In code, this is `BatchSpanProcessor::builder(exporter, Tokio).build()` from
`opentelemetry_sdk::trace::span_processor_with_async_runtime`
(`services/observability/src/lib.rs:132-156`) — not the plain `with_batch_exporter` builder method.

**Fix two — the OTLP/HTTP path suffix.** `OTEL_EXPORTER_OTLP_ENDPOINT` is conventionally a *base* URL
(e.g. `http://tempo:4318`); the exporter has to append the signal-specific path itself:

```rust
/// Fix two: the **OTLP/HTTP path suffix**. `OTEL_EXPORTER_OTLP_ENDPOINT` is documented (and
/// conventionally set) as a *base* URL ... — the exporter must append the signal-specific
/// `/v1/traces` itself; `opentelemetry-otlp` 0.32's `with_endpoint` does NOT do this
/// automatically. Missing it sends every export to the base path, which a real collector 404s —
/// surfaced by the SDK's own error handling as a generic, misleading "network error" with no status
/// code, not an HTTP error ...
```
(`services/observability/src/lib.rs:119-125`)

```rust
let traces_endpoint = format!("{}/v1/traces", endpoint.trim_end_matches('/'));
```
(`services/observability/src/lib.rs:147`)

Both bugs were "reproduced and fixed by actually running this against a live collector … not inferred
from documentation" (`services/observability/src/lib.rs:127-129`) — worth noting because it's the
kind of failure mode (silent, no stack trace, no obviously-wrong status code) that's easy to misdiagnose
as a transport or runtime bug rather than a missing path segment.

The exporter also builds its own `reqwest::Client` explicitly, "after the caller has pinned the
crypto provider," so the OTLP exporter rides the same audited TLS stack as the rest of the binary
instead of an independently-resolved one (`services/observability/src/lib.rs:140-143`) — this is the
same `install_default_crypto_provider()` call both binaries make before `init_tracing`.

Sampling is `Sampler::ParentBased(TraceIdRatioBased(ratio))`, `ratio` from
`OTEL_TRACES_SAMPLER_ARG` (default `0.1`): the decision is made once at whatever span starts the
trace (an unparented root), and every descendant — including one re-parented from a stored
`traceparent` across a process boundary — mechanically inherits it via the traceparent's own flags
byte, with no cross-process coordination needed (`services/observability/src/lib.rs:66-71, 135-138`).

## 2. Role-aware `service.name`

The control-plane binary is one image running several roles (`serve`, `dispatcher`, `reconciler`,
`a2a`, `notifier`, `mcp` — see `08-kubernetes-deployment.md`). If every role reported as the same
`service.name`, Tempo's service list couldn't tell them apart. `main.rs` resolves the role first, then
builds the service name from it:

```rust
let role = std::env::args().nth(1)
    .or_else(|| std::env::var("CONTROL_PLANE_ROLE").ok())
    .unwrap_or_else(|| "serve".to_string());

// Tracing/OTel init (ticket #246), after role resolution so `service.name` is role-aware
// (`control-plane-serve`, `control-plane-dispatcher`, ...) — Tempo's service list otherwise can't
// tell the roles apart.
let otel_guard = lci_observability::init_tracing(&format!("control-plane-{role}"));
```
(`services/control-plane/src/main.rs:645-653`)

So Tempo sees `control-plane-serve`, `control-plane-dispatcher`, `control-plane-reconciler`,
`control-plane-a2a`, `control-plane-notifier`, `control-plane-mcp` as distinct services, all from one
binary/image. `agent-runner` calls `init_tracing("agent-runner")` directly, with no role suffix
(`services/agent-runner/src/run.rs:53`) — it's always the same kind of workload (a per-task Job).

## 3. Cross-process trace propagation: `current_traceparent` / `set_remote_parent`

`tracing`'s span stack is a live, in-process call tree. It has no way to survive a boundary like "this
value becomes a Kubernetes Job's env var" or "this value is written to a Postgres row and read back
minutes later by a different process." `lci-observability` adds exactly two functions to bridge that
gap, and the module doc calls this out as the one genuinely new piece of plumbing in the crate:

```rust
//! Also carries the one genuinely new piece of plumbing the ticket calls for: threading a W3C
//! `traceparent` across boundaries that aren't a live call stack — a Kubernetes Job env var, a
//! Postgres row read back much later — via [`current_traceparent`] / [`set_remote_parent`].
```
(`services/observability/src/lib.rs:6-8`)

`current_traceparent()` serializes the *current* span's OTel context as a W3C `traceparent` string
(`version-traceid-spanid-flags`), or `None` if the current span carries no valid OTel context at all
(e.g. the OTel layer was never installed):

```rust
pub fn current_traceparent() -> Option<String> {
    let cx = tracing::Span::current().context();
    if !cx.span().span_context().is_valid() { return None; }
    let mut carrier = HashMap::new();
    global::get_text_map_propagator(|propagator| propagator.inject_context(&cx, &mut carrier));
    carrier.remove("traceparent")
}
```
(`services/observability/src/lib.rs:176-184`)

`set_remote_parent(span, traceparent)` does the reverse: re-parents a *new* span under a remote
`traceparent` string, with a `None`/unparseable value degrading gracefully to "just start a new
independent root," never a panic or a silently-dropped context
(`services/observability/src/lib.rs:191-201`).

Two concrete uses of this pair, both already covered elsewhere in this deep-dive series:

- **Dispatcher → Job.** `job_manifest` calls `lci_observability::current_traceparent()` while still
  inside the dispatcher's own `"job.dispatch"` span, and — if it returns `Some` — pushes it as the
  Job's `TRACEPARENT` env var (`services/control-plane/src/integrations/k8s.rs:479-481`, see
  `08-kubernetes-deployment.md`). The runner then reads that env var back and calls
  `set_remote_parent` on its own root span:

```rust
let root_span = tracing::info_span!("runner.task");
lci_observability::set_remote_parent(&root_span, std::env::var("TRACEPARENT").ok().as_deref());
```
(`services/agent-runner/src/run.rs:55-56`)

  The result: the Job's whole root span becomes a **child** of the dispatcher's `"job.dispatch"` span,
  not a disconnected sibling trace — one trace spans the dispatch decision and the actual Job
  execution, across a process (and Pod) boundary.

- **Collector endpoint/sampler passthrough.** Since the agent-runner Job isn't a Helm-templated pod
  (unlike `control-plane`/`dispatcher`, which get `OTEL_EXPORTER_OTLP_ENDPOINT` from chart values
  directly), `job_manifest` also forwards the dispatcher's own `OTEL_EXPORTER_OTLP_ENDPOINT` and
  `OTEL_TRACES_SAMPLER_ARG` env vars verbatim into the Job — deliberately **outside** the `is_open`
  credential gate, since a collector URL is a non-secret endpoint, not a credential, and `open`
  Jobs (untrusted repo + generated code) are exactly the ones whose telemetry matters most
  (`services/control-plane/src/integrations/k8s.rs:482-502`).

## 4. Prometheus `/metrics` — a separate pipeline from OTel, by design

To be explicit about something the rest of this doc otherwise only implies: **`/metrics` is not part
of the OTel/Tempo tracing pipeline described above.** It's a second, independent telemetry channel —
a `metrics` crate (via `metrics_exporter_prometheus::PrometheusHandle`) feeding a pull-based
Prometheus-format endpoint, scraped by Alloy and stored in Mimir, entirely separate from the
span-export path.

```rust
pub metrics: PrometheusHandle,
...
async fn metrics_endpoint(State(state): State<AppState>) -> impl IntoResponse { ... }
```
(`services/control-plane/src/main.rs:116-117, 513`)

The `serve` role exposes `/metrics` on its main HTTP listener (`:8080`, alongside the webhook/admin/
A2A routes); every headless role without its own HTTP server (`dispatcher`, `reconciler`, `a2a`,
`notifier`, `mcp`) stands up a tiny dedicated metrics-only listener on `METRICS_ADDR` (default
`:9090`) instead:

```rust
/// Serve `/metrics` (+ `/healthz`) on `METRICS_ADDR` for roles without a main HTTP server.
fn spawn_metrics_server(handle: PrometheusHandle) {
    let addr = std::env::var("METRICS_ADDR").unwrap_or_else(|_| "0.0.0.0:9090".to_string());
    ...
}
```
(`services/control-plane/src/main.rs:909-911`)

`agent-runner` carries no `/metrics` endpoint at all — it's a one-shot Job, not a long-running
service, so there's nothing to pull-scrape from it. Its data (findings, token usage, per-turn
proof-of-work) is either pushed to the control plane (which *does* expose `/metrics`) or logged, to
be read back via Loki (see §5's `prompt-budget` dashboard, which deliberately reads Loki rather than
Postgres or Prometheus for exactly this reason).

Scraping is done with **`PodMonitor` CRs** (Alloy discovers Prometheus-Operator-style CRs), not pod
annotations, and those `PodMonitor`s live alongside the workload Deployment in `ai-helm`, not in this
repo's `deploy/observability` chart (`deploy/observability/README.md:42-45`).

## 5. What's actually monitored: the Grafana dashboards

Dashboards are **generated**, not hand-written: Python sources under `tools/dashboard-gen/`, built
with the `grafana-foundation-sdk`, compiled to the committed JSON under
`deploy/observability/dashboards/*.json`. The chart's own README is explicit that the JSON is a build
artifact: "Edit the Python, regenerate, commit — don't hand-edit the JSON."
(`deploy/observability/README.md:8-9`).

| File | What it watches | Datasource |
|---|---|---|
| `overview.json` | Landing-page KPIs + run-duration p95 | Postgres |
| `task-runs.json` | Per-run list + detail, with a Loki drill-down | Postgres, Loki |
| `review-runs.json` | Per-run audit of what each review run actually offered the model (the ADR-0034/0062 turn-0 tool surface and the redacted resolved `ReviewConfig` — model, preset, temperature, budgets), from `tasks.run_tools`/`tasks.run_config_b64` | Postgres |
| `prompt-budget.json` | Whether `review.max_diff_chars` needs raising — diff/context-block byte accounting (source vs. rendered vs. budget vs. omitted) parsed from the `log_prompt_metrics` log event, aggregated across every run rather than one task | Loki |
| `repositories.json` | Per-repository view: index size, languages | Postgres |
| `review-quality.json` | Findings by priority/category, reviewer 👍/👎 reactions | Postgres |
| `review-delivery.json` | Review state, outbox delivery, and the finding payloads actually posted | Postgres |
| `review-cost.json` | Billed review cost/tokens straight from the AI-Gateway's own billing logs, by model/by repo/per-run | Loki (+ Postgres for the repo-name variable) |
| `feedback.json` | Reaction quality: approval rate, 👎 broken down by finding category/priority | Postgres |
| `ingress-dispatcher.json` | Webhook ingestion + queue/dispatch health | Postgres, Loki |
| `operations.json` | RED metrics (rate/errors/duration) off `/metrics` | Prometheus |

(File list + the first 8 rows' descriptions: `deploy/observability/README.md:23-40`; the three not
listed there — `review-runs.json`, `prompt-budget.json`, `review-delivery.json` — are later additions,
described above from their own generator-module doc comments:
`tools/dashboard-gen/lci_dashboards/review_runs.py:1-12`,
`tools/dashboard-gen/lci_dashboards/prompt_budget.py:1-8`,
`tools/dashboard-gen/lci_dashboards/review_delivery.py:1`.)

The README's own explanation of *why* most of these are Postgres-sourced rather than scraped is worth
keeping: "the review agent runs as a one-shot Kubernetes Job, so its output (findings, reactions)
can't be pull-scraped — it's persisted to Postgres and read from there"
(`deploy/observability/README.md:36-37`). `review-cost.json` and `prompt-budget.json` are the explicit
exceptions, both reading Loki instead — one because the cost/token figures are the AI-Gateway's own
billing-log lines, the other because the diff/context-budget accounting is logged once per run across
every task and needs to be aggregated across runs, which Postgres's per-task row shape doesn't suit as
directly as a Loki query over the `log_prompt_metrics` event.

## Verification notes

- Confirmed `init_tracing` genuinely no-ops (no tracer provider, no OTel layer) when
  `OTEL_EXPORTER_OTLP_ENDPOINT` is unset by reading the `and_then` chain directly
  (`services/observability/src/lib.rs:78-92`) and cross-checking the test
  `current_traceparent_is_none_without_the_otel_layer`
  (`services/observability/src/lib.rs:263-269`), which asserts this with no OTel subscriber installed
  at all.
- Confirmed the dispatcher→Job trace handoff is a real parent/child relationship, not just two
  independent traces sharing a label, by reading both ends: `current_traceparent()` called inside
  `KubeLauncher::launch`'s span before `job_manifest` is built
  (`services/control-plane/src/integrations/k8s.rs:233, 479-481`), and `set_remote_parent` called on
  the runner's own root span from the `TRACEPARENT` env var it reads back
  (`services/agent-runner/src/run.rs:55-56`) — plus the crate's own round-trip test asserting the
  child span shares the root's trace-id while getting its own span-id
  (`services/observability/src/lib.rs:226-249`).
- Confirmed the Grafana dashboard file list against the actual directory listing
  (`ls deploy/observability/dashboards/*.json` — 11 files) rather than the README table alone (which
  lists only 8), then read the three undocumented generators' own module docstrings
  (`review_runs.py`, `prompt_budget.py`, `review_delivery.py`) for the extra three rather than
  guessing from filename.
