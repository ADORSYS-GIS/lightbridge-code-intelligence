# ADR-0063: CLI-only repository approval (retire the web approval gate)

- **Status:** Superseded by [ADR-0110](0110-apps-web-investment.md) (web approval returns as an additional surface alongside the CLI, not instead of it)
- **Date:** 2026-06-28
- **Amended:** 2026-07-03
- **Deciders:** @stephane-segning

## Context and Problem Statement

The web console's **last unique function** is the repository **approval gate** ([ADR-0023](0023-db-backed-rbac.md),
admin governance epic #75/#89): before a repository is indexed or reviewed, an admin must approve it
(`repo:approve` / `repo:deny`). Everything else the console did — runs, insights, transcripts, feedback —
is moving to Grafana ([ADR-0064](0064-observability-via-grafana-behind-caddy-oauth2.md)). Maintaining a
full Next.js + OIDC single-page app ([ADR-0006](0006-nextjs-app-router-web-ui.md),
[ADR-0027](0027-daisyui-design-system.md)) just to flip an approve/deny toggle is disproportionate.

**Can approvals move to a CLI, so `apps/web` can be retired entirely?**

## Decision Drivers

- **Shrink the maintained surface** — retire a Next.js app, its OIDC SPA client, and the daisyUI design system.
- **Reuse the existing trust boundary + authz** ([ADR-0002](0002-rust-control-plane-trust-boundary.md),
  [ADR-0014](0014-keycloak-oidc-resource-server.md), [ADR-0023](0023-db-backed-rbac.md)) — do **not** invent a new auth path.
- **Auditability** — who approved/denied what, and when.
- **Single-operator ergonomics** — the operator runs their own infra and lives in a terminal; a GitOps-leaning shop.

## Considered Options

- **Option A — CLI over the existing OIDC-gated control-plane endpoints (device-code flow).** A small binary
  authenticates via Keycloak's OAuth **device authorization grant**, then calls the same per-capability
  endpoints the web used (`repo:read` / `repo:approve` / `repo:deny`). No new server surface; the authz model
  is unchanged.
- **Option B — GitOps-declared approvals.** The approved-repo set lives declaratively in `ai-helm-values`
  (or a small CRD) and the control plane reconciles it. No interactive auth, fully audited via git history,
  matches the GitOps ethos — but approval becomes a PR/merge, not a one-liner.
- **Option C — Status quo.** Keep the web console solely for approvals.

## Decision Outcome

Chosen option: **A — a CLI using the OAuth device-code flow against the existing permission-gated
endpoints**, with **B noted as a strong complement** (and the likely long-term home if approvals should be
declarative). A best satisfies the drivers: it reuses [ADR-0023](0023-db-backed-rbac.md) authz verbatim
(no new trust path), keeps approval an explicit, audited human action, and — together with
[ADR-0064](0064-observability-via-grafana-behind-caddy-oauth2.md) — lets `apps/web` be **deleted**.

This ADR is now **Accepted** — see the 2026-07-03 amendment below for the as-built refinements. The
imperative-vs-GitOps tension (A vs B) is recorded as an unresolved consequence, not a blocker: B remains
the likely long-term home if approvals should become declarative.

### Amendment (2026-07-03) — as-built refinements

The client shipped as `clients/lci` (binary `lci`, ratatui). Three refinements to the original decision,
none of which change the trust boundary or reuse of [ADR-0014](0014-keycloak-oidc-resource-server.md)/[ADR-0023](0023-db-backed-rbac.md)
authz:

1. **An interactive ratatui TUI, not a bare imperative CLI.** Rather than one-shot `list`/`approve`/`deny`
   subcommands, `lci` is a small terminal app with two views — **Repositories** (approve/deny, capability-gated
   on the token's `repo:approve`/`repo:deny`) and **Runs** (watch active review/index tasks, cancel with
   `task:cancel`). The operator lives in a terminal and watches runs as often as they approve; a TUI serves
   both without a second tool. The endpoints called are exactly the ones the web consumed
   (`/admin/repositories`, `.../approve`, `.../deny`, `/tasks`, `/tasks/{id}/cancel`, `/me`).

2. **Authorization-Code + PKCE with a loopback `127.0.0.1` redirect, not the device-code flow** originally
   chosen in Option A. Rationale: it **reuses the web's existing PKCE public-client pattern**
   ([ADR-0014](0014-keycloak-oidc-resource-server.md); the web's `lightbridge-web` client is already a
   `standardFlowEnabled`, `pkce.code.challenge.method=S256` public client), so no new Keycloak flow needs to
   be enabled or reasoned about; there is **no device-code polling**; and the terminal UX is better because a
   browser is available on the operator's laptop (we print the authorize URL *and* auto-open it, then catch
   the redirect on a one-shot loopback listener). PKCE is hand-rolled (`base64url(sha256(verifier))` + plain
   token POSTs) — no `oauth2` crate — to keep the dependency surface minimal.

3. **Token cached as JSON in the OS config dir with silent refresh.** The token lives at
   `<config_dir>/token.json` (`ProjectDirs::from("fyi","camer","lci")`), written `0600`, storing an
   **absolute** `expires_at` (computed from `expires_in` at fetch time). Startup uses a fresh cached token,
   else a `grant_type=refresh_token` exchange, else interactive login; a background task refreshes within ~60s
   of expiry and surfaces a "re-auth needed" state in the status bar on failure rather than crashing. A new
   Keycloak **public** client `lightbridge-cli` (loopback redirect URIs, same audience/permissions scopes as
   `lightbridge-web`) is required before it authenticates against prod — see `clients/lci/README.md`.

The **imperative-vs-GitOps (Option B)** tension is **unchanged**: `lci` is still an imperative state change,
and B remains noted as the declarative long-term option.

### Consequences

- **Good** — `apps/web` (Next.js + OIDC SPA + daisyUI) can be retired once observability is on Grafana
  (ADR-0064); one fewer language/stack to maintain.
- **Good** — approval stays a `repo:approve`-gated, attributable action; the control plane records the
  approving identity (from the token's permission claim) + timestamp.
- **Bad / tension** — a CLI is an **imperative** state change, which sits awkwardly next to this project's
  **GitOps-declarative** norm (prod mutation via merge, not exec). Mitigated by Option B, or by having the
  CLI write an auditable record; worth resolving in discussion.
- **Bad** — must build + distribute the binary and handle device-code token caching/refresh.
- **Neutral** — needs a stable admin API surface (`list` / `approve` / `deny`); the web already consumes
  one, so the CLI mostly reuses it.

## Pros and Cons of the Options

### Option A — device-code CLI over existing endpoints
- Good — reuses ADR-0014/0023 authz; zero new trust path; approval stays explicit + audited.
- Good — tiny surface; a single binary the operator already wants.
- Bad — imperative; another artifact to build/ship; device-code UX + token storage.

### Option B — GitOps-declared approvals
- Good — declarative, fully git-audited, no interactive auth, matches the deploy model.
- Bad — approval is now a PR/merge round-trip; needs a reconcile loop + drift handling.

### Option C — keep the web for approvals
- Good — nothing to build.
- Bad — keeps an entire Next.js + OIDC app alive for one toggle; the thing we're trying to retire.

## More Information

- Retires the web: [ADR-0006](0006-nextjs-app-router-web-ui.md), [ADR-0027](0027-daisyui-design-system.md).
- Authz reused: [ADR-0014](0014-keycloak-oidc-resource-server.md), [ADR-0023](0023-db-backed-rbac.md).
- Companion: [ADR-0064](0064-observability-via-grafana-behind-caddy-oauth2.md) (observability → Grafana).
- Admin governance origin: epic #75 / permission authz #89.
