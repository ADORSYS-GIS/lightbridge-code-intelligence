# ADR-0064: Observability via Grafana behind a Caddy + oauth2 proxy (retire the web dashboards)

- **Status:** Superseded by [ADR-0110](0110-apps-web-investment.md) (analytics move back into apps/web via embedded Grafana panels, not a standalone Grafana replacing the web dashboards)
- **Date:** 2026-06-28
- **Deciders:** @stephane-segning

## Context and Problem Statement

The web console renders **runs / insights** dashboards from Postgres. But the observability stack already
holds most of that data and more: Grafana + Prometheus/Mimir ([ADR-0046](0046-observability-dashboard-deployment.md))
for metrics, Loki for the agent's structured per-turn logs, and the operational Postgres for run state.
The two remaining "dashboard" tickets — surfacing the run **transcript** (#143, persisted via
[ADR-0034](0034-agent-run-transcript-and-observability.md)) and the **feedback** signal (#144, captured +
consumed via [ADR-0044](0044-feedback-memory-m1.md)) — would duplicate in a bespoke Next.js UI what Grafana
does better.

**Decision: stop building observability in `apps/web`; surface everything in Grafana, exposed securely
behind a Caddy reverse proxy with an oauth2 auth layer.**

## Decision Drivers

- **Don't duplicate Grafana** — it already does dashboards, log exploration, metrics, and alerting far
  better than a hand-rolled SPA.
- **One auth story** — reuse Keycloak for the observability surface, ideally a single edge-auth pattern.
- **Minimal, conventional infra** — Caddy + oauth2 forward-auth is a small, well-trodden edge pattern.
- **The data already exists** — transcripts, feedback, per-run tokens are in Postgres/Loki today.

## Considered Options

- **Option A — Grafana behind Caddy + an oauth2 forward-auth layer** (oauth2-proxy or `caddy-security`)
  against Keycloak. One edge-auth pattern that also fronts any other internal tool; Grafana stays simple.
- **Option B — Grafana's native Keycloak OIDC** (no Caddy/oauth2-proxy). Fewer moving parts, but auth is
  configured per-app rather than once at the edge.
- **Option C — Status quo / extend the bespoke web dashboards.** Keep building observability in `apps/web`.

## Decision Outcome

Chosen option: **A — Grafana fronted by Caddy + oauth2 forward-auth to Keycloak.** It gives **one reusable
edge-auth pattern** for Grafana (and future internal tools), keeps Grafana itself unconfigured for auth, and
lets the bespoke web dashboards be **deleted**. Data sources stay as today: **Postgres** (transcript #143 +
feedback #144 + run state — reuse the [ADR-0046](0046-observability-dashboard-deployment.md) panels),
**Loki** (the agent's structured per-turn logs — tools, tokens, `reasoning_chars` (the length of the
captured `reasoning_content`, [ADR-0060](0060-capture-model-reasoning-and-glm-5-2-latency-finding.md)),
latency), **Prometheus/Mimir**
(metrics), and **optionally Tempo** (request-level traces — the one genuinely new build, the OTel door
[ADR-0034](0034-agent-run-transcript-and-observability.md) left open).

**Proposed** — open for discussion, especially A-vs-B (edge forward-auth vs Grafana-native OIDC) and whether
OTel→Tempo tracing is worth standing up now.

### Consequences

- **Good** — delete the bespoke web dashboards; Grafana becomes the single observability pane (logs +
  metrics + SQL panels + alerting). Closes #143 (transcript = a Postgres panel + Loki) and #144
  (feedback = a Postgres panel over `review_feedback`).
- **Good** — combined with [ADR-0063](0063-cli-only-repository-approval.md) (approvals → CLI), **`apps/web`
  can be retired entirely** — no Next.js, OIDC SPA, or daisyUI to maintain.
- **Good** — a reusable Caddy + oauth2 edge for any internal tool, not just Grafana.
- **Bad** — Grafana + Caddy + oauth2-proxy is more moving infra than a single Next.js app — but it's
  generic, off-the-shelf, and Grafana is already running.
- **Bad** — SQL panels over the operational Postgres couple dashboards to the live schema and can be heavy
  ([ADR-0046](0046-observability-dashboard-deployment.md) already accepts this trade-off).
- **Neutral** — confirm Loki is scraping the agent pod logs; OTel→Tempo remains optional/future.

## Pros and Cons of the Options

### Option A — Grafana behind Caddy + oauth2 forward-auth
- Good — one edge-auth pattern for all internal tools; Grafana stays simple; reuses Keycloak.
- Bad — three components to run (Caddy, oauth2-proxy, Grafana) vs one app.

### Option B — Grafana native Keycloak OIDC
- Good — fewer moving parts; Grafana handles its own auth.
- Bad — per-app auth config; no reusable edge for the next internal tool.

### Option C — extend the bespoke web dashboards
- Good — one app, one language.
- Bad — re-implements Grafana badly; no log/metric correlation; the thing we're retiring.

## More Information

- Builds on: [ADR-0046](0046-observability-dashboard-deployment.md) (dashboards-as-code, mostly Postgres),
  [ADR-0034](0034-agent-run-transcript-and-observability.md) (transcript + the OTel door),
  [ADR-0044](0044-feedback-memory-m1.md) (the feedback signal already consumed).
- Closes (by superseding the UI need): #143 (transcript view), #144 (feedback display).
- Companion: [ADR-0063](0063-cli-only-repository-approval.md) (approvals → CLI, the web's last function).
