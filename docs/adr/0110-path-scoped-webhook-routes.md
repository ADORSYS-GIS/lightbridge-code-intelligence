# ADR-0110: Path-scoped, per-forge webhook routes

- **Status:** Accepted
- **Date:** 2026-07-28
- **Deciders:** @stephane-segning, @leghadjeu-christian

## Context

Webhook ingress used a single `POST /webhook` endpoint that determined the source platform
(GitHub or GitLab) by inspecting request headers at runtime. This approach had several problems:

- For GitLab, the JSON body had to be parsed before signature verification to extract the
  project ID needed to look up the per-project secret, exposing an attacker-controlled JSON
  parse before authentication.
- A legacy `POST /github/webhook` alias remained live for backward compatibility, creating two
  active entry points with identical behaviour.
- There was no Bitbucket webhook path at all; adding a third forge required extending the
  header-sniffing dispatch.
- The route was flat and unversioned, inconsistent with the `/api/v2` migration.

## Decision

Replace the unified handler with three explicit, path-scoped handlers mounted under
`/api/v2/webhook/`:

| Platform  | Path                                          | Identity source          |
|-----------|-----------------------------------------------|--------------------------|
| GitHub    | `POST /api/v2/webhook/github`                 | `GITHUB_WEBHOOK_SECRET`  |
| GitLab    | `POST /api/v2/webhook/gitlab/{installation_id}` | per-project secret in `control-plane.json`, keyed by `installation_id` |
| Bitbucket | `POST /api/v2/webhook/bitbucket/{installation_id}` | per-repo secret in `control-plane.json`, keyed by `stable_id_from_key("workspace/repo_slug")` |

Platform is determined by the path, not by header inspection. For GitLab and Bitbucket the
installation ID comes from the path segment, so the registry lookup happens before the JSON body
is parsed — the body is only read after the secret is known.

The old `/webhook` and `/github/webhook` routes are removed (hard cutover). A shared
`record_or_dedup()` helper handles deduplication and persistence for all three handlers.

## Consequences

- Signature verification for GitLab and Bitbucket no longer requires a pre-auth JSON parse.
- Adding a fourth forge requires adding one route and one handler; the dispatch logic is not
  shared and does not need to change.
- The legacy `/webhook` and `/github/webhook` paths return 404 after deployment. All configured
  GitHub App webhooks and GitLab project webhooks must be repointed before the deploy.
- Bitbucket webhook URLs must use `stable_id_from_key("workspace/repo_slug")` as the
  `installation_id` path segment. See `docs/runbooks/bitbucket-platform-setup.md`.
