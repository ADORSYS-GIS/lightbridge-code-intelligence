# ADR-0046: Deploying the Grafana dashboards (and why they read Postgres)

- **Status:** Accepted
- **Date:** 2026-06-24
- **Deciders:** @stephane-segning

> **Update (2026-07-18, [ADR-0100](0100-retire-db-transcript-logs-as-observability.md)):** the
> `agent_transcript` table referenced below (decision 3 / References) was **dropped** with the DB run
> transcript. Token-usage is therefore re-sourced off `agent_transcript` onto the **Loki**
> `gen_ai_request_model` label; the `review-quality`/`overview` panels that read `agent_transcript` were
> **removed**, and `review-cost` is kept on the Loki source. The Loki rebuild is tracked in **#472**. The
> original decision below is unchanged as a point-in-time record.

## Context and Problem Statement

The dashboards-as-code chart (`deploy/observability/`, [PR #30]) has existed since the observability
landing but **was never deployed** — it is referenced by no ArgoCD Application in `ai-helm`, and even
if it were, it would not have worked: its `instanceSelector` and datasource names did not match the
live cluster, the control-plane `/metrics` endpoints were never scraped, and there was no Postgres
datasource at all. So a lot of operational data we already persist (findings, token usage, index
size, reviewer reactions) was invisible.

This ADR records how the dashboards actually get deployed and the one design choice worth pinning:
**most dashboards read Postgres, not Prometheus.**

## Decision Drivers

- Make the dashboards bind to the existing Grafana and resolve real datasources — no new Grafana, no
  hand-edited JSON.
- Surface the data we already collect; "too much data we don't use" was the trigger.
- Respect the chart genericization ([ADR-0057 in ai-helm]) — all deployment-specifics live in
  `ai-helm-values`, the chart ships placeholder defaults.
- Least-privilege DB access for Grafana.

## Decision

1. **A dedicated ArgoCD Application (`lightbridge-observability`)** in `ai-helm` sources this repo's
   `deploy/observability` path (Source A) with values from `ai-helm-values` (Source B), deployed into
   the `observability` namespace where the Grafana Operator watches. It mirrors the multi-source shape
   of the `lightbridge-code-intelligence` app, minus the image-updater annotations (no images).

2. **Metrics scraping via `PodMonitor` CRs**, not pod annotations. Alloy discovers Prometheus-Operator
   CRs; the `serve` role exposes `/metrics` on `:8080` and `dispatcher` on `:9090`. The PodMonitors
   live in the workload namespace (`converse`) alongside the deployment in `ai-helm`, not in this
   chart. This powers `operations.json` (RED metrics) only.

3. **Agent analytics come from Postgres, not Prometheus.** The review agent runs as a **one-shot
   Kubernetes Job** ([ADR-0037](0037-agent-acts-via-mediated-tools.md)), so its output cannot be
   pull-scraped — the Job is gone before any scrape. But the data is already persisted: findings with
   priority/category ([ADR-0032], `reviews.findings`), token usage ([ADR-0034], `agent_transcript`),
   and reviewer reactions ([ADR-0035], `review_feedback`). The new `review-quality` dashboard and the
   enrichments to `overview`/`repositories` read these tables directly. This is deliberate: a
   pushgateway or sidecar to get these into Prometheus would add infrastructure for data we can simply
   query where it lives.

4. **Read-only Postgres access** via a least-privilege `grafana_ro` **CNPG managed role** (login +
   `SELECT` only) on the control-plane database, reached through the CNPG `*-rw` Service (no read
   replica exists). The datasource password is sourced from a Secret; the chart's `GrafanaDatasource`
   CR never stores it. There is no read replica, so `-rw` against a read-only role is the pragmatic,
   safe default.

## Consequences

- **Good:** the dashboards finally deploy and bind; data we already collect is charted; the one-shot
  Job model is respected instead of fought; DB exposure is read-only and least-privilege.
- **Cost:** dashboard queries hit the primary (no replica). They are read-only, infrequent
  (`30s`–`1m` refresh), and bounded (`LIMIT`); acceptable until a replica exists, at which point the
  datasource host moves to `*-r`.
- **Limitation:** `operations.json` depends on scrape wiring in `ai-helm`; the Postgres dashboards do
  not, so they light up as soon as the datasource resolves.

## References

- [PR #30] — the original dashboards-as-code chart.
- [ADR-0037](0037-agent-acts-via-mediated-tools.md) — agent runs as a one-shot Job via mediated tools.
- [ADR-0032] priority/category findings, [ADR-0034] transcript token usage, [ADR-0035] reaction
  feedback — the persisted data the Postgres dashboards read.

[PR #30]: https://github.com/adorsys-gis/lightbridge-code-intelligence/pull/30
[ADR-0032]: 0032-review-finding-priority-and-category.md
[ADR-0034]: 0034-agent-run-transcript-and-observability.md
[ADR-0035]: 0035-review-feedback-signal.md
[ADR-0057 in ai-helm]: https://github.com/ADORSYS-GIS/ai-helm
