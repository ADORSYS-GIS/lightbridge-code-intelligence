# Lightbridge observability chart

Deploys the Lightbridge Grafana dashboards via the [Grafana Operator](https://grafana.github.io/grafana-operator/):
a `GrafanaFolder`, the generated `GrafanaDashboard` CRs, and an optional read-only Postgres
`GrafanaDatasource`. Install into the `observability` namespace (or wherever your Grafana operator
watches).

Dashboards are **generated** by `tools/dashboard-gen` and committed under `dashboards/`. Edit the
Python, regenerate, commit — don't hand-edit the JSON.

## Before deploying — fill in `values.yaml`

| Value | What |
|---|---|
| `instanceSelector.matchLabels` | Labels selecting your Grafana instance (copy from a working dashboard, e.g. `alloy-collector`). |
| `datasources.{postgres,loki,prometheus}` | Names of the Grafana datasources the dashboards resolve to. |
| `postgresDatasource.*` | Read-replica connection + a Secret holding the read-only password (set `create: false` to manage it yourself). |

The dashboard JSON references datasources by the placeholders `${DS_POSTGRES}`, `${DS_LOKI}`,
`${DS_PROMETHEUS}`; each `GrafanaDashboard.spec.datasources` maps those to the configured datasource
names, and the operator resolves them to UIDs at import.

## Dashboards

| File | Forwards | Datasource |
|---|---|---|
| `overview.json` | UI landing KPIs (+ run-duration p95) | Postgres |
| `task-runs.json` | runs list + detail (+ Loki drill-down) | Postgres, Loki |
| `repositories.json` | repositories view (+ index size, languages) | Postgres |
| `review-quality.json` | findings by priority/category, reviewer reactions | Postgres |
| `review-cost.json` | billed review cost/tokens straight from the AI-Gateway (eaig) Loki billing logs — by-model / by-repo / per-run | Loki (+ Postgres for the `$repo` var) |
| `feedback.json` | reaction quality: approval rate, 👎 by finding category/priority | Postgres |
| `ingress-dispatcher.json` | webhook + queue/dispatch health | Postgres, Loki |
| `operations.json` | RED metrics | Prometheus |

Most dashboards are **Postgres**-sourced: the review agent runs as a one-shot Kubernetes Job, so its
output (findings, reactions) can't be pull-scraped — it's persisted to Postgres and read from there.
That's why a read-only Postgres datasource matters (see `postgresDatasource` above). Review cost/token
figures are the exception: they come from the AI-Gateway's own billing logs in Loki
(`review-cost.json`), not from Postgres.

`operations.json` needs the control plane's `/metrics` scraped into Prometheus/Mimir. Scraping is
done with **`PodMonitor` CRs in the workload namespace** (Alloy discovers Prometheus-Operator CRs),
NOT pod annotations — the `serve` role exposes `/metrics` on `:8080` and the `dispatcher` role on
`:9090`. The PodMonitors live with the workload deployment (in `ai-helm`), not in this chart.
