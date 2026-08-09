# ADR-0113: A dedicated OpenTelemetry Collector for LCI telemetry

- **Status:** Proposed
- **Date:** 2026-08-08
- **Deciders:** @stephane-segning

## Context and Problem Statement

LCI's telemetry needs are shaped by one fact the rest of the cluster does not share: **the
agent-runner is a one-shot Kubernetes Job**. A process that exists for the duration of a single
review cannot be scraped, and cannot report cumulative counters, because every run is a fresh process
with no memory of the last one. It has to push, and what it pushes has to be summable across
short-lived emitters.

Everything else in this cluster is a long-lived pod that Alloy either scrapes or tails. That mismatch
is why every attempt to instrument LCI has ended up proposing a change to shared infrastructure.

Three findings from #597/#598/#599, each verified by running the real tool rather than reading docs,
establish the constraint:

1. **Delta metrics are dropped.** Prod Alloy routes its OTLP receiver's metrics straight to
   `otelcol.exporter.prometheus`, which discards delta-temporality metrics and reports nothing
   upstream. The emitting process sees a successful export and produces zero series.
2. **The fix is gated behind cluster-wide stability.** `otelcol.processor.deltatocumulative` is an
   *experimental* component; `alloy validate` refuses the config unless the collector runs with
   `--stability.level=experimental`. That collector is, per its own NetworkPolicy comments, the
   cluster's sole telemetry collector — a DaemonSet carrying all logs, traces and metrics. It also
   converts sums and *exponential* histograms only, so explicit-bucket delta histograms (the OTel Rust
   SDK default) stay dropped even with it deployed.
3. **Pod logs arrive CRI-wrapped.** Alloy tails `/var/log/pods/...` with no `stage.cri`, so lines are
   stored `<ts> stdout F {json}`. Every consumer must hand-unwrap with `| pattern` before `| json`,
   and Alloy's own `stage.json` — intended to promote a `level` label and extract `trace_id` for the
   Loki→Tempo derived field — silently no-ops on every pod log in the cluster
   ([ai-helm-values#207](https://github.com/ADORSYS-GIS/ai-helm-values/issues/207)).

Cumulative temporality is not an escape hatch. The runner's resource identity is `service.name` only
— deliberately, since a per-pod `service.instance.id` would multiply every series by the number of
runs — so all runs write into one series. Under cumulative, each fresh process restarts at zero and
the collector keeps the latest datapoint: one run's histogram perpetually overwritten, never a
distribution.

So the problem is not "how do we emit metrics". It is that **every LCI telemetry improvement
currently requires changing the collector that carries every other workload's telemetry**, and the
blast radius of that is out of proportion to the benefit.

## Decision Drivers

- **Blast radius.** An LCI telemetry change should be able to fail without taking out cluster-wide
  logs, traces and metrics. This is the objection that stopped the OTLP metrics work; it was never an
  objection to the technique.
- **One-shot Jobs need push, and push needs delta.** Any design that assumes scrape or cumulative is
  answering a different question.
- **Structured fields should survive to the query layer.** Hand-unwrapping a CRI prefix in every
  dashboard query is a tax paid per consumer, forever, for a problem that belongs at ingest.
- **Don't fork what already works.** Traces reach Tempo through Alloy's OTLP receiver correctly today
  (once #598 gave runner Jobs the endpoint at all). Nothing about traces motivates this change.
- **Failure must be loud.** The recurring failure mode across this whole effort is telemetry that
  reports success and produces nothing, indistinguishable from a quiet week. Whatever we build needs
  its own health signal.

## Considered Options

- **Option A — Status quo.** Structured `tracing::info!` event → pod stdout → Alloy file tailing →
  Loki, with a `| pattern` CRI unwrap in every query. This is what #598/#599 shipped and it works.
- **Option B — Change the shared Alloy.** Add `deltatocumulative` and `stage.cri`, and set
  `alloy.stabilityLevel=experimental`.
- **Option C — A dedicated OTel Collector for LCI**, receiving OTLP from LCI workloads and exporting
  directly to Loki / Mimir / Tempo.
- **Option D — Report through the control plane.** The runner posts its numbers to the control-plane
  internal API; the control plane records them on its existing Prometheus `/metrics`, already scraped
  by Alloy.

## Decision Outcome

**Option C — a dedicated OTel Collector for LCI.**

Option B was declined on blast radius: lowering the stability floor on the cluster's only collector,
to enable one feature for one workload, risks all cluster observability for a dashboard. Option D
avoids all the infrastructure but pushes review-domain telemetry through the control plane's request
path and gives logs no help at all. Option A works and stays the fallback, but leaves the CRI unwrap
tax on every future query and leaves metrics permanently unavailable.

Option C isolates the decision: **we own the stability gate, the processors and the failure domain
for LCI telemetry, and the shared collector stops being on the critical path for our work.**

Loki being on chart 7.0.0 (Loki 3.x, native OTLP ingest at `/otlp/v1/logs`) is what makes this
worthwhile beyond metrics — it removes the CRI problem for LCI entirely rather than working around it.

### Shape

- **A `Deployment`, not a DaemonSet.** This is a push target, not a node-local tailer. **≥2 replicas
  behind a Service**: a one-shot Job flushes on exit, so a collector that is unavailable at that
  moment loses that run's telemetry permanently — there is no retry after the pod is gone.
- **Receiver:** OTLP gRPC + HTTP.
- **Exporters:** logs → `otlphttp` to Loki's native OTLP endpoint; metrics → `prometheusremotewrite`
  to Mimir; traces → `otlp` to Tempo.
- **Processors:** `memory_limiter`, `batch`, and `deltatocumulative` — the last one now a local
  decision. Whether base-2 exponential histogram aggregation is still required depends on whether the
  deployed collector version's `deltatocumulative` has gained explicit-bucket support; **verify, do
  not assume it matches Alloy's bundled version.**
- **Runner side:** restore the metrics pipeline in `lci-observability` (reverted in #598 rather than
  shipped dormant), add an OTLP logs appender (`opentelemetry-appender-tracing`), and extend the
  existing `OtelGuard` flush to cover all three signals. `OTEL_EXPORTER_OTLP_ENDPOINT` is already
  propagated into runner Job pods (#598) — it gets repointed, not re-plumbed.
- **NetworkPolicy:** the cluster is Cilium default-deny-egress. The collector needs egress to
  `loki-gateway`, `mimir-nginx` and `tempo` in `observability`, and ingress from the namespaces LCI
  runs in. Follow the existing `deps/alloy` policy as the pattern.

### Consequences

**The migration hazard, which dominates the rest.** Switching log transport changes the stream
labels. The Prompt Budget dashboard selects
`{namespace="lightbridge-agents", pod=~"lightbridge-agent-.*"}` and unwraps a CRI prefix; OTLP-ingested
logs will carry neither. **The dashboard will render empty panels, not errors.** That is the exact
failure mode this entire effort has repeatedly hit, and an empty panel is indistinguishable from "no
reviews ran". The collector, the runner change and the dashboard queries must land as one arc, and
the first check after cutover is that the p95 panel still shows a number.

**Loki OTLP label mapping needs pinning.** Loki maps resource attributes to labels and everything else
to structured metadata. Which attributes become labels must be chosen explicitly — `service.name` and
namespace yes; `task_id`, repository and SHA emphatically not, or the index blows up.

**A new failure domain.** If the collector is down, LCI telemetry is silently lost. This trades a
shared, well-watched component for a private, unwatched one. It needs its own alert, not just a
dashboard nobody opens.

**ai-helm-values#207 stays open and unchanged.** Routing LCI's logs around Alloy fixes nothing for any
other workload. The broken `level`/`trace_id` extraction is a cluster-wide defect, and this ADR is not
a reason to close it.

**Not in scope:** other workloads' telemetry, and replacing Alloy for anything. Alloy keeps tailing
pod logs cluster-wide, including LCI's raw stdout — the Task Runs log panel continues to work
unchanged.

### Open questions to resolve during implementation

1. Does the chosen collector version's `deltatocumulative` support explicit-bucket delta histograms,
   or is the base-2 exponential view still mandatory on the SDK side?
2. Do LCI logs go **only** via OTLP, or via both OTLP and pod stdout during a transition? A hard
   cutover is the house default, but the Task Runs raw-log panel reads stdout and must keep working.
3. Which OTel Collector distribution — upstream `opentelemetry-collector-contrib`, or a second Alloy
   instance configured for this purpose? Alloy is already the house idiom and the team knows its
   config language; upstream contrib has the components documented under their own names.
4. What is the collector's own health signal, and where does it alert?

### Verification requirements

Non-negotiable, given the history recorded in the Context section:

- `validate` the collector config in a container before merge — the values-repo render check only
  proves the YAML is well-formed, and the collector config is a YAML *string* inside it, so `yq`
  accepts anything.
- After cutover, query Loki and Mimir for the actual series. A green rollout proves nothing; this
  effort has produced two separate pipelines that reported success and delivered zero data points.
- Confirm the Prompt Budget dashboard still renders numbers, and update its queries in the same arc.

## References

- [ADR-0046](0046-observability-dashboard-deployment.md) — dashboards-as-code and datasource wiring
- #597 / #598 — prompt-budget instrumentation, and the recorded reasoning for abandoning OTLP metrics
- #599 — the Prompt Budget dashboard, including the CRI unwrap this ADR removes
- #600 — coverage-loss panel semantics
- [ai-helm-values#207](https://github.com/ADORSYS-GIS/ai-helm-values/issues/207) — `stage.cri` at ingest, cluster-wide, unaffected by this ADR
