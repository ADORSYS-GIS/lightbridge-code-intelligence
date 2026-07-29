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

Replace the unified handler with three explicit, path-scoped handlers:

| Platform  | Path                                          | Identity source          |
|-----------|-----------------------------------------------|--------------------------|
| GitHub    | `POST /webhook/github`                        | `GITHUB_WEBHOOK_SECRET`  |
| GitLab    | `POST /webhook/gitlab/{installation_id}`      | per-project secret in `control-plane.json`, keyed by `installation_id` |
| Bitbucket | `POST /webhook/bitbucket/{installation_id}`   | per-repo secret in `control-plane.json`, keyed by `stable_id_from_key("workspace/repo_slug")` |

Platform is determined by the path, not by header inspection. For GitLab and Bitbucket, signature verification happens against raw bytes before the body is JSON-parsed — the `installation_id` in the path is sufficient to look up the secret. After verification succeeds, the payload's claimed project/repo identity is cross-checked against the path-resolved project to prevent cross-project forgery.

The old `/webhook` and `/github/webhook` routes are removed (hard cutover). A shared `persist_delivery()` helper handles deduplication and persistence for all three handlers.

Note: these routes live at `/webhook/*` in this PR. A companion PR nests all API routes (including these) under `/api/v2`, producing final paths `/api/v2/webhook/github`, `/api/v2/webhook/gitlab/{id}`, `/api/v2/webhook/bitbucket/{id}`.

## Consequences

- Signature verification for GitLab and Bitbucket no longer requires a pre-auth JSON parse.
- A cross-project forgery attack (using project A's secret to drive project B) is rejected — the payload's claimed identity is verified against the path-resolved project after signature check.
- Adding a fourth forge requires adding one route and one handler; the dispatch logic is not shared and does not need to change.
- The legacy `/webhook` and `/github/webhook` paths return 404 after deployment. All configured GitHub App webhooks and GitLab project webhooks must be repointed before the deploy.
- Bitbucket webhook URLs must use `stable_id_from_key("workspace/repo_slug")` as the `installation_id` path segment. See `docs/runbooks/bitbucket-platform-setup.md`.
- Final URLs after the `/api/v2` companion PR: `/api/v2/webhook/{github,gitlab/<id>,bitbucket/<id>}`.
