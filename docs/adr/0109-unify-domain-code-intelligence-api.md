# ADR-0109: Unify A2A, MCP, webhooks, and the API under one domain, path-routed

- **Status:** Accepted
- **Date:** 2026-07-26
- **Deciders:** @stephane-segning
- **Fulfills:** RFC-0007 (ingress-plane direction)
- **Supersedes:** the own-Deployment/Ingress topology proposed by Epic #295

## Context and Problem Statement

RFC-0007 already specifies one control-plane binary hosting `serve`/`a2a`/`mcp` together, and
explicitly flags "does the `mcp` surface warrant its own ingress role or fold into `serve`?" as an
open question. Today the answer is neither, cleanly: A2A serves at `/` and, per its own module doc,
plans **its own** Deployment/Ingress (Epic #295) — the opposite topology from RFC-0007's stated
direction. There is no `/mcp` surface at all yet (`services/review-mcp` is stdio-only, internal to
the review agent). Webhooks are inconsistently routed: a unified `POST /webhook` exists alongside a
kept-for-compatibility legacy `POST /github/webhook` alias, and there is no GitLab or Bitbucket
path. Every route in `main.rs` is flat and unversioned — dashboard (OIDC), runner-internal
(shared-bearer), and public (webhook, A2A card) routes share one unversioned namespace with no
segmentation. Separately, `ROADMAP.md` references "ADR-0098" for A2A per-finding streaming, but no
such file exists — the link actually points to PR #458.

## Decision Drivers

- Resolve RFC-0007's open ingress-plane question with a concrete answer: one service, one domain,
  path-routed — not per-surface Deployments/Ingresses.
- A real API version segment (`/api/v2`) so dashboard/runner-internal/public routes are at least
  namespaced, even though they'll continue to carry different auth models.
- Multi-forge webhook paths need to exist before [ADR-0108](0108-codeplatform-github-gitlab-bitbucket.md)'s
  GitLab/Bitbucket wiring has anywhere sensible to receive requests.
- Hard cutover: no dual-path legacy alias left running "for compatibility" — every existing
  webhook-configuring integration gets updated to the new path in the same change.

## Considered Options

- **A — Keep A2A's own Deployment/Ingress (Epic #295) and add `/mcp` as a second separate service.**
  Rejected: multiplies ai-helm-side Ingress/cert/DNS surface for no functional gain, and directly
  contradicts RFC-0007's already-accepted one-binary-three-planes direction.
- **B — Single domain `code-intelligence-api.ai.camer.digital`, path-routed: `/a2a`, `/mcp`,
  `/api/v2` (dashboard + runner-internal + `/api/v2/webhook/{github, gitlab/<installation-id>,
  bitbucket/<installation-id>}`), all configurable (base path/domain overridable per deployment,
  not hardcoded).** Chosen.

## Decision Outcome

Chosen option: **B**. The control plane's existing single binary gains a versioned route tree:

- `/a2a` — the existing A2A JSON-RPC/REST surface, unchanged in behavior, moved under this prefix;
  Epic #295's separate-Ingress plan is dropped.
- `/mcp` — `services/review-mcp`'s tool surface (read_file, graph, vector search, SAST, etc.,
  [ADR-0104](0104-full-opencode-fs-tool-suite.md)) exposed over HTTP/SSE for external MCP clients,
  reusing the same mediated-tool implementations the review agent already calls in-process — no
  second implementation of the tools themselves.
- `/api/v2` — every existing flat route (`/tasks`, `/repositories`, `/me`, `/admin/*`,
  `/internal/*`) moves here verbatim (no behavior change, pure path migration), plus the new
  `/api/v2/webhook/{github, gitlab/<installation-id>, bitbucket/<installation-id>}` family
  replacing today's `/webhook` + `/github/webhook` alias. The legacy alias is removed in the same
  change — every configured webhook (GitHub App settings, GitLab project hooks) gets repointed, no
  transition period.
- Domain, base path, and per-surface path segments are all read from config (env/Helm values), not
  hardcoded, so `code-intelligence-api.ai.camer.digital` is the deployed value, not a literal in
  the router.

The `ADR-0098` roadmap gap is resolved by correcting `ROADMAP.md`'s reference to link PR #458
directly rather than a nonexistent ADR file — PR #458 is still open/unmerged, so a numbered ADR is
not backfilled for a decision that hasn't landed; `0098` stays a deliberately-skipped number rather
than being reused, so no existing reference silently starts pointing at different content.

### Consequences

- Good, because RFC-0007's ingress-plane question is answered concretely and the answer matches
  what RFC-0007 already leaned toward.
- Good, because GitLab and Bitbucket webhook wiring ([ADR-0108](0108-codeplatform-github-gitlab-bitbucket.md))
  has a defined path scheme to land on.
- Bad, because every external integration pointing at the current webhook URLs (GitHub App config,
  GitLab project hooks) must be updated at cutover — tracked explicitly as a rollout step, not an
  afterthought, since a missed repoint silently drops webhook delivery.
- Bad, because the k8s Ingress/DNS/cert side of this lives in `ai-helm`/`ai-helm-values`, outside
  this repo — this ADR's repo-side change (route surface + config) has a cross-repo dependency
  tracked as its own ticket, not assumed to ship atomically.
- Neutral, because A2A's own functional behavior (JSON-RPC/REST semantics, agent card) is unchanged
  — only its mount path and Ingress ownership move.

## More Information

Epic #295's remaining Phase 4 (`ListTasks`) and #457 (A2A per-finding streaming) survive this
supersession — they're event/pagination semantics at the A2A layer, orthogonal to which path A2A is
mounted at, and carry forward under the epic implementing this ADR.
