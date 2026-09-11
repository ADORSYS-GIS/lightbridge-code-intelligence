# ADR-0115: Retire `apps/web`'s deployment; the console moves to `apps/lci` in `converse-frontends`

- **Status:** Accepted (infra migration in progress — see Verification)
- **Date:** 2026-09-11
- **Deciders:** @leghadjeu-christian

## Context and Problem Statement

[ADR-0112](0112-invest-in-apps-web-supersede-0063.md) (2026-08-02) reversed
[ADR-0063](0063-cli-only-repository-approval.md)'s retirement plan and committed to `apps/web` as
this repo's **permanent** structured-admin surface — the repository approval gate, per-repo review
settings, model-override picker — coexisting indefinitely with `clients/lci` (the fast
terminal-native TUI).

Since then, a full replacement console shipped in a sibling repository: **`apps/lci` in
`ADORSYS-GIS/converse-frontends`**, a from-scratch Next.js app on its own design system
(`packages/ui-web`), OIDC-authenticated (not `apps/web`'s `better-auth`). It does not cover a
narrower slice of `apps/web`'s job — it covers *both* halves ADR-0112 discussed: the general
browsing surface ADR-0063 had already been moving to Grafana (repositories, runs, run detail —
including logs, per [ADR-0102](0102-grafana-loki-embedded-run-logs.md)'s iframe-embed decision),
**and** the admin surface ADR-0112 wanted `apps/web` to keep owning (approve/deny, per-repo
settings). It has been live at `lci.ai.camer.digital`, DNS-confirmed resolving to the same
load-balancer IP as `apps/web`'s existing production host, `code-intelligence.ai.camer.digital`.

That is exactly the kind of drift ADR-0112 itself was written to catch and resolve for ADR-0063 vs.
ADR-0109 — a later, concrete state of the world silently contradicting an earlier ADR's stated
conclusion, left unreconciled. `apps/web`'s "permanent" status does not survive a full,
already-shipped replacement occupying its own production domain.

**Does ADR-0112's "`apps/web` is permanent" conclusion still hold, now that a full-featured
replacement exists and has already taken over the production domain?**

## Decision Drivers

- **Don't let infra reality silently contradict an accepted ADR** — the same discipline ADR-0112
  itself insisted on. An ADR that says "permanent" while the actual Kubernetes deployment routes
  traffic somewhere else is worse than no ADR at all.
- **One canonical console, not two divergent stacks solving the same problem.** Two OIDC/auth
  configurations, two RBAC surfaces, two design systems, two sets of the same repository-browsing
  and approval UI — with no user ever needing both — is pure maintenance debt once one of them is
  live and DNS-canonical.
- **`apps/lci` already reached parity-plus, not a subset.** It isn't a narrower replacement that
  would leave a gap ADR-0112's admin surface still needs to fill; it absorbed both roles ADR-0112
  discussed splitting between `apps/web` and `clients/lci`.
- **`clients/lci` (the TUI) is untouched and orthogonal.** Nothing here revisits ADR-0063's or
  ADR-0112's judgment that fast terminal-native ops has its own lane; this ADR is only about the
  browser-based console.

## Considered Options

- **Option A — Reaffirm ADR-0112**: keep `apps/web` as this repo's admin surface, and treat
  `apps/lci` as redundant effort to be reconciled or retired instead.
- **Option B — Split responsibility**: `apps/web` stays as the admin-only surface (approve/deny,
  settings), `apps/lci` stays browsing-only (repositories, runs) — two consoles, each with a
  distinct, non-overlapping job.
- **Option C — Retire `apps/web`'s deployment; `apps/lci` becomes the sole console.** Source-code
  removal from this repo is an explicit follow-up, not a requirement of this cutover.

## Decision Outcome

Chosen option: **C**.

`apps/lci` was not built to a scope that leaves Option B available — nobody scoped it to exclude
admin features, or scoped `apps/web` to exclude browsing after ADR-0063's Grafana migration; `apps/lci`
simply grew to cover both, and is already the one serving the production domain. Option B would
require *building* a split that doesn't exist today, for no functional gain over what's already
shipped. Option A would mean reverting a working, DNS-live, feature-complete replacement to
re-invest in the app it replaced — the inverse of ADR-0112's own reasoning ("match the tool to the
job," "don't leave the app in permanent limbo").

### Verification / rollout status

This decision is **enacted via infrastructure changes, tracked and partly landed at the time of this
ADR**:

- Tracking issue: [ai-helm-values#435](https://github.com/ADORSYS-GIS/ai-helm-values/issues/435).
- [ai-helm#1122](https://github.com/ADORSYS-GIS/ai-helm/pull/1122) — removes `apps/web`'s
  `controllers.web` block (Deployment, Service) from the `lightbridge-code-intelligence` Helm
  chart, along with its RBAC (`templates/agents-rbac.yaml`'s `-web` ServiceAccount/Role/RoleBinding
  — the direct-Kubernetes-API log-reading grant that [ADR-0102](0102-grafana-loki-embedded-run-logs.md)
  had already made obsolete by moving run logs to an embedded Grafana panel, but which nothing had
  cleaned up until now) and its `-auth` ExternalSecret (`BETTER_AUTH_SECRET`).
- [ai-helm-values#436](https://github.com/ADORSYS-GIS/ai-helm-values/pull/436) — permanently
  redirects `code-intelligence.ai.camer.digital` to `lci.ai.camer.digital` (Traefik `Middleware`,
  whole-host 301), reusing the existing Ingress object's identity so its TLS Certificate updates in
  place instead of being deleted and re-issued.

Both PRs were open, CI-green, and had passed cross-checks confirming nothing `apps/lci` depends on
was being removed, as of this ADR. **This repo's own `apps/web` source code is unaffected by those
PRs** — they retire the *deployment*, not the code; source-level removal is called out below as a
deliberate follow-up, the same "kept, not deleted, until a dedicated pass" shape ADR-0063's own
Epic #241 used.

### Consequences

- Good, because there is one canonical console again — the state ADR-0063 originally wanted, just
  reached by parity-plus replacement rather than by porting features into `clients/lci`.
- Good, because the `lightbridge-code-intelligence` Helm chart sheds a whole controller, its RBAC
  surface, a `better-auth` OIDC client, and dead Kubernetes-log-reading permissions that had already
  been functionally obsolete since ADR-0102.
- Bad, because `apps/web`'s source code in this repo becomes dead weight until a follow-up removes
  it — not addressed by this ADR or its linked PRs.
- Bad, because CI keeps actively building, pushing to GHCR, and cosign-signing a `lightbridge-web`
  image on every push to `main` (`.github/workflows/build-images.yml` →
  `image-pipeline.yml`'s `web-build` job and `images` matrix), for a service the linked infra PRs
  leave with no Kubernetes deployment to consume it — an ongoing compute/storage cost, and a signed
  `:latest` tag that could read as "still deployed." Not addressed by this ADR or its linked PRs;
  tracked as its own follow-up in [#646](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/646).
- Bad, because the console now lives in a different repository (`converse-frontends`) from the
  control-plane it talks to, a cross-repo split ADR-0006 never had to account for when `apps/web`
  was co-located; this ADR documents that reality, it does not resolve the coordination cost.
- Neutral, because `clients/lci` is completely unaffected — it remains the terminal-native ops tool
  ADR-0063/ADR-0112 already established it to be.

### Required side effects (landed in the same PR as this ADR)

- `docs/adr/0112-invest-in-apps-web-supersede-0063.md`: status line changes from `Accepted` to
  `Superseded by [ADR-0115](0115-retire-apps-web-move-console-to-lci-ui.md)` — body untouched, per
  this project's own ADR immutability rule (`docs/adr/README.md`).
- `docs/adr/README.md`: update ADR-0112's index row status; add this ADR's row.

## Pros and Cons of the Options

### Option A — reaffirm ADR-0112, keep `apps/web`

- Good — no migration cost; the design and IA investment ADR-0112 cited (ADR-0024, ADR-0027) keeps
  its home.
- Bad — reverts a working, DNS-live, feature-complete replacement for no functional gain; ignores
  that the production domain already routes elsewhere.
- Bad — leaves two OIDC configurations, two RBAC surfaces, and duplicate UI investment running
  indefinitely with no user ever needing both.

### Option B — split responsibility (admin vs. browsing)

- Good — smallest conceptual change from ADR-0112's stated split (`apps/web` = admin, `clients/lci`
  = terminal ops) if `apps/lci` were narrowed to match.
- Bad — requires actively *removing* features from `apps/lci` (or `apps/web`) that already work
  today, to manufacture a split neither app was built to; pure regression with no offsetting benefit.
- Bad — still leaves two consoles, two auth configurations, and two design systems to maintain.

### Option C — retire `apps/web`'s deployment, `apps/lci` is the sole console (chosen)

- Good — matches what was actually built and is actually live; no functionality is removed from
  anything users currently use.
- Good — collapses the RBAC/secret/controller surface this ADR's linked PRs remove.
- Bad — `apps/web`'s source code is left as a follow-up cleanup, and the console now spans two
  repositories instead of one.

## More Information

- Superseded: [ADR-0112](0112-invest-in-apps-web-supersede-0063.md) (invest in `apps/web`
  permanently), which itself superseded [ADR-0063](0063-cli-only-repository-approval.md) (CLI-only
  repository approval) — that chain's `clients/lci` conclusion is unaffected and stands.
- Directly informs this decision:
  [ADR-0006](0006-nextjs-app-router-web-ui.md) (the original Next.js app-router choice for
  `apps/web`, now superseded in effect by a differently-stacked replacement in another repo),
  [ADR-0064](0064-observability-via-grafana-behind-caddy-oauth2.md) and
  [ADR-0100](0100-retire-db-transcript-logs-as-observability.md) (Grafana/Loki absorbing most of
  `apps/web`'s original scope, the trend this ADR concludes),
  [ADR-0102](0102-grafana-loki-embedded-run-logs.md) (the run-log RBAC this ADR's linked PR finally
  removes).
- Infra tracking: [ai-helm-values#435](https://github.com/ADORSYS-GIS/ai-helm-values/issues/435),
  [ai-helm#1122](https://github.com/ADORSYS-GIS/ai-helm/pull/1122),
  [ai-helm-values#436](https://github.com/ADORSYS-GIS/ai-helm-values/pull/436).
- CI cleanup follow-up: [#646](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/646)
  (stop building/pushing/signing the `lightbridge-web` image once `apps/web`'s deployment retires).
- The replacement: `apps/lci` in `ADORSYS-GIS/converse-frontends`, deployed as its own Application
  (`lci-ui`, chart `converse-lci`) at `lci.ai.camer.digital`.
- `clients/lci`, explicitly unaffected by this ADR: `clients/lci/README.md`.
