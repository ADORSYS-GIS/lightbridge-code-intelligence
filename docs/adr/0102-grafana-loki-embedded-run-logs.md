# ADR-0102: Run logs move from a native k8s pod-log stream to an embedded Grafana/Loki panel

- **Status:** Accepted (code live; Grafana-side embed prerequisites not yet configured — degrades to
  fallback until they are)
- **Date:** 2026-07-19
- **Deciders:** Stephane Segning Lambou (owner/eng)
- **Source of truth:** epic #459, #479, #480

## Context and Problem Statement

The `apps/web` run-detail page's "Logs" card used to live-stream the review pod's logs directly:
a Node API route (`app/api/runs/[id]/logs/route.ts`) used `@kubernetes/client-node` to open the k8s
API's pod-log stream and piped it to the browser via a `use-log-stream` hook and a `RunLogs`
component.

[ADR-0100](0100-retire-db-transcript-logs-as-observability.md) already made Loki the single
observability surface for run internals (the DB transcript was retired; the OpenCode logger plugin
ships every per-turn signal to Loki). The k8s-stream code path duplicated that capability through a
second, independent mechanism that: needed cluster RBAC (`pods/log`) baked into the web deployment
(a durable credential grant just for this one feature); only worked while the pod still existed
(ephemeral per-task Job pods are garbage-collected, so the stream went dead once the run finished and
its pod was reaped); and read raw container stdout rather than the leveled, structured lines the
logger plugin already emits to Loki. Loki, by contrast, retains logs past pod lifetime and is already
the canonical store epic #459 built around.

## Decision

**Replace the native k8s log stream with an embedded Grafana panel sourced from Loki, via the
generated `task-runs` dashboard's "Logs for $task_id" panel, rendered chromeless in a `d-solo`
iframe.**

Landed in #479 (initial embed) and #480 (deterministic panel id follow-up):

- **Removed** (#479): `apps/web/app/api/runs/[id]/logs/route.ts`, `apps/web/components/runs/run-logs.tsx`,
  `apps/web/lib/hooks/use-log-stream.ts`, the `@kubernetes/client-node` dependency
  (`apps/web/package.json` + lockfile), and the now-orphaned `serverExternalPackages` entry in
  `apps/web/next.config.ts`.
- **Added** (#479): `apps/web/components/runs/run-logs-embed.tsx` (`RunLogsEmbed`). It builds a
  `d-solo` iframe URL from the dashboard UID and a client-side task id:
  `${NEXT_PUBLIC_GRAFANA_URL}/d-solo/lci-task-runs/task-runs?orgId=1&panelId=<id>&var-task_id=<taskId>&theme=dark&kiosk`.
  The dashboard UID (`lci-task-runs`) is the `UID` constant in
  `tools/dashboard-gen/lci_dashboards/task_runs.py`.
- **Fixed** (#480): the initial embed inferred the logs panel's numeric id from array order
  (`panelId=2`), which the generator doesn't pin explicitly — Grafana auto-assigns ids on load, so
  inserting or reordering a panel ahead of the logs panel would silently break the embed. #480 pins an
  explicit `.id(100)` on the "Logs for $task_id" panel in the generator (verified:
  `tools/dashboard-gen/lci_dashboards/task_runs.py` line 75, `.id(100)`) and updates the frontend
  constant to match (`GRAFANA_LOGS_PANEL_ID = 100` in `run-logs-embed.tsx`) — 100 is picked high
  enough that Grafana's auto-assignment for the remaining id-less panels (which start at 1) can't
  collide with it.
- **New env var:** `NEXT_PUBLIC_GRAFANA_URL` — client-visible (the `NEXT_PUBLIC_*` prefix inlines it
  into the browser bundle), so it must be a browser-reachable Grafana URL, not an in-cluster-only
  address. Documented in `apps/web/.env.example` and `apps/web/README.md` (both already updated in
  #479).
- **Graceful fallback, not a broken iframe.** When `NEXT_PUBLIC_GRAFANA_URL` is unset — the default in
  CI and local dev — `RunLogsEmbed` renders a `StatusLine` note instead of an iframe, and the run-detail
  page's existing `kubectl logs -f ...` snippet (a plain string helper, not a k8s API client) remains
  the terminal fallback.
- **Operational follow-up, not yet done.** The web deployment's RBAC grant for `pods/log` in the
  agents namespace is now unused by this feature and should be revoked. That grant lives in
  ai-helm/ai-helm-values (home-os-adjacent infra, out of this repo) and is **not** addressed by this
  ADR or by #479/#480 — flagging it here so it isn't lost.

## Consequences

- **Positive.** One observability surface for run internals (Loki), not two independently-maintained
  ones; no `pods/log` RBAC needed in the web deployment for this feature; logs survive past pod
  garbage collection; the panel definition (and any future field additions to it) lives in one place,
  the dashboard generator, instead of being reimplemented in the frontend.
- **Positive.** Explicit panel-id pinning (#480) makes the embed URL robust to future dashboard
  edits — a real bug (fragile array-order-inferred ids) caught and fixed one PR after the feature
  first shipped, not left to surface as a silent production break.
- **Negative / not yet realized.** The embed only renders once the Grafana-side prerequisites below
  are configured; until then, every environment — including prod — sees the fallback `kubectl logs`
  snippet, not the panel. This is by design (no broken iframe), but it means the feature is currently
  **shipped-but-inert** in prod pending an operator decision, not a code change.
- **Neutral / follow-up.** The unused `pods/log` RBAC grant on the web deployment should be revoked
  (home-os side); tracked here, not actioned by this PR.

## Prerequisites for the embed to render in prod (open, infra-side — verified NOT done in this repo)

1. **Grafana `allow_embedding = true`.** Grafana refuses to be iframed by default; this must be set on
   the Grafana instance apps/web points at. Not verifiable from this repo (Grafana config is
   infra-side); not confirmed done.
2. **An auth model for the iframe.** The embed needs to load for a logged-in `apps/web` user without a
   separate login prompt inside the iframe — either anonymous read on a scoped Grafana org/Viewer
   role, or Grafana behind the same Keycloak SSO so the browser's existing session authenticates the
   iframe transparently. **This is an open decision for the operator; it has not been made.** Neither
   option is implemented or configured as of this ADR.
3. **`NEXT_PUBLIC_GRAFANA_URL` set in the deployed environment**, pointing at a browser-reachable
   Grafana URL (not an in-cluster-only DNS name). Not set as of this ADR (verified: no occurrence in
   this repo's deploy config; the value is expected to come from ai-helm-values, out of scope here).
4. **No framing block at the edge or in `apps/web`'s own response headers.** Checked this repo for a
   `Content-Security-Policy` / `X-Frame-Options` response header in `apps/web` — **none exists** as of
   this ADR, so `apps/web` itself imposes no `frame-src` restriction on embedding Grafana. Whether the
   ingress in front of the deployed Grafana instance sends a blocking `X-Frame-Options: DENY` or a
   restrictive CSP is infra-side and not verified here.

## More Information

- [ADR-0100](0100-retire-db-transcript-logs-as-observability.md) — made Loki the single
  run-observability surface; this ADR is the `apps/web` frontend consequence of that decision.
- [ADR-0046](0046-observability-dashboard-deployment.md) — how the Grafana dashboards deploy
  (generator → committed JSON → Helm chart).
- Epic #459 (source of truth), #479 (initial embed), #480 (pinned panel id).
