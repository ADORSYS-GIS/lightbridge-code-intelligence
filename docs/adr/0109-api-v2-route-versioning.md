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

The version prefix is applied at the lowest possible level in each client — inside the constructor
— so env vars (`CONTROL_PLANE_URL`, `api_url`) carry the bare origin and no operator config
needs to change:

- `apps/web`: `controlPlaneUrl()` defaults to `http://localhost:8080/api/v2`
- `lci` CLI: `ApiClient::new()` appends `/api/v2` to `self.base`
- `agent-clients`: `ControlPlaneClient::new()` appends `/api/v2` to `self.base_url`

The cutover is hard — old flat paths return 404 immediately after deployment. Every consumer is
updated in the same change to avoid a broken intermediate state.

## Consequences

- All API paths are now under `/api/v2`, making future breaking changes straightforward to
  introduce under `/api/v3` without affecting existing clients.
- The single `api_v2_router()` function is the canonical list of all versioned routes; adding a
  route requires touching one place.
- Operators must ensure `CONTROL_PLANE_URL` points at the bare service origin (no `/api/v2`
  suffix), which is the existing convention.
