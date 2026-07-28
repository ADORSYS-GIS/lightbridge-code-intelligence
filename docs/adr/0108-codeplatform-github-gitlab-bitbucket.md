# ADR-0108: Wire `CodePlatform` for GitHub/GitLab, add Bitbucket

- **Status:** Accepted
- **Date:** 2026-07-26
- **Deciders:** @stephane-segning
- **Activates:** [ADR-0072](0072-platform-abstraction-layer.md)
- **Resolves:** RFC-0008 (GitLab / multi-forge, open questions)

## Context and Problem Statement

[ADR-0072](0072-platform-abstraction-layer.md) already defines `Platform`
(`GitHub`/`GitLab`) and a `CodePlatform` trait in
`services/control-plane/src/integrations/platform.rs` — but the module is explicit Phase-0
scaffolding (`#![allow(dead_code)]`), not yet called from the webhook handler, outbox, or
reconciler. GitHub and GitLab remain two fully independent, ad-hoc client modules
(`integrations/github.rs`, `integrations/gitlab.rs`) with different auth models (App installation
token vs. static access token) that the trait was designed to unify but never got wired into.
Bitbucket does not exist anywhere in the codebase. RFC-0008 already did the gap analysis and left
its "unresolved questions" open, including the GitLab single-token blast-radius concern and the
`open`-mode write-back gap (deferred to ADR-0088).

## Decision Drivers

- Finish what ADR-0072 started rather than let it stay dead scaffolding — every place that
  currently branches on `X-GitHub-Event` vs `X-Gitlab-Event` headers, or calls `github.rs`/
  `gitlab.rs` directly, should go through `CodePlatform` instead, per this repo's SOLID/SRP-by-file
  convention (a real seam already exists with two implementations; use it).
- Bitbucket is net-new scope, not a RFC-0008 gap — it gets its own `CodePlatform` implementation
  following the GitHub/GitLab pattern, with its own auth model (Bitbucket App password / OAuth,
  decided during implementation).
- This is a hard cutover: once `CodePlatform` is wired into the webhook router, outbox, and
  reconciler, the direct `github.rs`/`gitlab.rs` call sites in those three places are removed, not
  kept as a parallel path.

## Considered Options

- **A — Leave GitHub/GitLab as separate ad-hoc modules; add Bitbucket as a third ad-hoc module.**
  Rejected: triples the problem ADR-0072 exists to solve, and contradicts the "traits only at a
  real seam" convention — three concrete implementations is unambiguously a real seam.
- **B — Wire `CodePlatform` into webhook routing, outbox, and reconciler now (GitHub + GitLab
  behind the trait, no behavior change per this repo's refactor discipline), then add Bitbucket as
  a third `CodePlatform` implementation using the now-proven seam.** Chosen.

## Decision Outcome

Chosen option: **B**, in two parts:

1. **Wire the existing trait** — `services/control-plane/src/http/webhook.rs` resolves a
   `Platform` from the request (still via header sniffing, or from the new path-scoped routes in
   [ADR-0109](0109-unify-domain-code-intelligence-api.md)) and dispatches through `CodePlatform`
   instead of branching on GitHub/GitLab modules directly; `outbox.rs`/`reconciler.rs` egress calls
   move behind the same trait. This part is explicitly a **behavior-neutral refactor** — zero
   observable change, verified against the full existing test suite plus a workspace build, per
   this repo's refactor-discipline rules — not bundled with any feature change.
2. **Add Bitbucket** — a new `BitbucketPlatform` implementing `CodePlatform`, its own webhook
   signature verification and auth-token minting, registered the same way GitHub/GitLab are.

### Consequences

- Good, because GitHub, GitLab, and Bitbucket become three implementations of one seam instead of
  three independent code paths that each need their own bugfixes.
- Good, because RFC-0008's open questions (GitLab blast radius, `open`-mode write-back) get a
  concrete home to be resolved in — the trait boundary — instead of staying analysis-only.
- Bad, because wiring an existing-but-dead trait into three live call sites is real surface area;
  tracked as its own story with the behavior-neutral verification this repo requires, separate from
  the Bitbucket-addition story (a structural PR and a new-capability PR should not be the same
  change, per this repo's refactor discipline).
- Neutral, because Bitbucket's specific auth model (App password vs. OAuth) is an implementation
  decision made when that story starts, not fixed by this ADR.

## More Information

Tracked as new stories under [Epic 2 — unify service domain](0109-unify-domain-code-intelligence-api.md),
since the webhook-routing rewrite ([ADR-0109](0109-unify-domain-code-intelligence-api.md)) and the
`CodePlatform` wiring touch the same call sites and should land together.
