# ADR-0109: Version all control-plane routes under `/api/v2`

- **Status:** Accepted
- **Date:** 2026-07-28
- **Deciders:** @stephane-segning, @leghadjeu-christian

## Context

Every route in the control plane was registered at the root path with no version prefix. Three
different auth models (OIDC-gated dashboard routes, shared-bearer runner-internal routes, and
admin routes) shared one flat, unversioned namespace. There was no mechanism to introduce a
breaking API change without affecting all consumers simultaneously.

## Decision

All consumer-facing routes are nested under `/api/v2` using Axum's `.nest()`. The router is
split into two functions:

- `api_v2_router()` — returns all versioned routes as a `Router<AppState>`
- `app()` — mounts the versioned sub-router plus the infra probes

Health probes (`/healthz`, `/readyz`) and `/metrics` stay at root because they are consumed by
Kubernetes and Prometheus respectively, not by API clients. Moving them would require coordinating
changes in every Helm chart and monitoring config with no user-facing benefit.

The version prefix is carried in the env var, not appended by client constructors. Each client
trims trailing slashes and uses the value as-is:

- `apps/web`: `controlPlaneUrl()` uses `CONTROL_PLANE_URL` / `AUTH_BACKEND_URL` as-is
- `lci` CLI: `ApiClient::new()` trims trailing slashes; `api_url` must include `/api/v2`
- `agent-clients`: `ControlPlaneClient::new()` trims trailing slashes; `CONTROL_PLANE_INTERNAL_URL` must include `/api/v2`

The chart values are updated in the companion Helm PR (ADORSYS-GIS/ai-helm) to append `/api/v2`
to `CONTROL_PLANE_INTERNAL_URL` and `AUTH_BACKEND_URL`.

The cutover is hard — old flat paths return 404 immediately after deployment. The chart update
must be deployed in the same window as this image to avoid downtime.

## Consequences

- All API paths are now under `/api/v2`, making future breaking changes straightforward to
  introduce under `/api/v3` without affecting existing clients.
- The single `api_v2_router()` function is the canonical list of all versioned routes; adding a
  route requires touching one place.
- Operators must ensure `CONTROL_PLANE_INTERNAL_URL` and `AUTH_BACKEND_URL` include `/api/v2`.
  The chart default values are updated accordingly (ADORSYS-GIS/ai-helm#817). Local dev env vars
  must also be updated if set explicitly.
