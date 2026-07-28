# Runbook — setting up and demoing the Bitbucket `CodePlatform` (ADR-0072/0108)

**Audience:** the operator (repo owner) and anyone demoing the multi-platform review pipeline.
**Status of the system today:** GitHub, GitLab, and Bitbucket are all live implementations of the
same `CodePlatform` trait, dispatched from one `HashMap<Platform, Arc<dyn CodePlatform>>` built once
at startup. Bitbucket is the newest of the three (#505) — this runbook is the checklist to configure
one Bitbucket repo end to end and prove a webhook really produces a review.

> This is an operational runbook, not a design doc. The decision lives in
> [ADR-0072](../adr/0072-platform-abstraction-layer.md) (the trait) and
> [ADR-0108](../adr/0108-codeplatform-github-gitlab-bitbucket.md) (activating it for all three
> platforms, adding Bitbucket). Read those for *why*; read this for *how*. Config reference:
> [Kubernetes and deployment — Bitbucket configuration](../kubernetes-deployment.md#bitbucket-configuration-adr-0108).

---

## The mental model (read this first)

One route (`/webhook`), one handler, one dispatch table. `detect_platform()` sniffs the request
headers (`X-GitHub-Event` / `X-Gitlab-Event` / `X-Event-Key`) to decide which platform a webhook
came from; everything downstream — signature verification, event routing, task creation, egress —
goes through the `CodePlatform` trait, so GitHub/GitLab/Bitbucket are three interchangeable
implementations of the same seam, not three parallel code paths.

Bitbucket is configured the same way GitLab is: **per-repo, file-only, no env fallback** (there is
no single Bitbucket "App" the way GitHub has — each repo carries its own credentials). That means
onboarding one Bitbucket repo is entirely a `control-plane.json` change plus one webhook
registration on the Bitbucket side — no code change, no redeploy of anything but the config.

**The one thing that trips people up:** Bitbucket Cloud retired **App Passwords** in 2026 in favor
of **API tokens**. If you've configured Bitbucket integrations before mid-2026, forget the
`username`/app-password pattern — it is fully removed as of 2026-07-28. The control plane's
`BitbucketProjectConfig` only has `email` + `api_token` fields; there is no app-password field to
misconfigure.

---

## Pre-flight

- [ ] You have (or can create) a Bitbucket Cloud **bot account** — a real or service Atlassian
      account dedicated to this integration (not your personal account), since its email + API
      token become long-lived credentials mounted into the control plane.
- [ ] You can reach that account's Atlassian settings: **avatar → Settings → Security → API tokens
      with scopes**.
- [ ] You have write access to the target Bitbucket repo (to register a webhook) and to
      `control-plane.json` (or the Kubernetes Secret it's mounted from).
- [ ] The control plane is already running with GitHub and/or GitLab (Bitbucket adds a platform, it
      doesn't replace anything) — or a fresh dev instance per
      [the local setup guide](../local-setup.md).

---

## Step 1 — create a Bitbucket API token (replaces the retired App Password)

1. In the bot account's Atlassian settings: **Security → API tokens with scopes → Create API token
   with scopes**.
2. Select **Bitbucket** as the product.
3. Grant scopes: `read:repository:bitbucket` + `write:repository:bitbucket` (repo read/write, needed
   for both the REST calls and — indirectly — the clone), plus the pull-request scopes needed to
   read/post PR comments (Bitbucket's scope picker groups these under "Pull requests" — grant both
   read and write).
4. Copy the token immediately — **it cannot be viewed again after creation**, only regenerated.
5. Note the bot account's **email address** — that's the Basic-auth *username* for REST calls (the
   token itself never doubles as a username, unlike some other platforms' tokens).

## Step 2 — register a webhook on the Bitbucket repo

1. In the target repo: **Repository settings → Webhooks → Add webhook**.
2. URL: your control plane's public `/webhook` endpoint (the same endpoint GitHub/GitLab post to —
   there is no separate Bitbucket-specific path in this codebase; see the scope note below).
3. Triggers: at minimum **Pull Request: Created**, **Pull Request: Merged**, **Pull Request:
   Declined**, **Pull Request: Comment created**, and **Repository: Push** (for re-indexing on a
   default-branch push).
4. Enable **request signing** (HMAC-SHA256) and set a secret — this is the value that goes into
   `webhook_secret` below. Bitbucket sends the signature as `X-Hub-Signature: sha256=<hex>`, the
   same header/format GitHub uses.

> **Scope note:** Bitbucket stays on the existing single `/webhook` header-detection route in this
> codebase, exactly like GitHub/GitLab — there is no dedicated `/webhook/bitbucket/<id>` path yet.
> ADR-0109's domain/path-scoped webhook routing (`/api/v2/webhook/bitbucket/<workspace>`) is a
> separate epic's (#492) scope, not adopted here.

## Step 3 — configure the repo in `control-plane.json`

```json
{
  "bitbucket": {
    "enabled": true,
    "projects": [
      {
        "workspace": "myteam",
        "repo_slug": "my-repo",
        "email": "bot-account@example.com",
        "api_token": "<the token from Step 1>",
        "webhook_secret": "<the secret from Step 2>"
      }
    ]
  }
}
```

- `workspace`/`repo_slug` are the two path segments of the repo's Bitbucket URL
  (`bitbucket.org/<workspace>/<repo_slug>`).
- Omit `api_url`/`bot_handle` to use the defaults (`https://api.bitbucket.org/2.0`,
  `lightbridge-bot`) — set them per-project only if you're pointing at a non-standard API host or
  want a distinct `@mention` handle for this repo.
- In production, mount this via an ExternalSecret-managed Kubernetes Secret at
  `/etc/lightbridge/control-plane.json` (same pattern as GitLab) rather than committing the token to
  a values file.
- Restart/redeploy `serve`, `dispatcher`, and `reconciler` (all three read `control-plane.json` at
  boot) so the new Bitbucket registry entry takes effect.

## Step 4 — approve the repo (Epic #75's gate)

Bitbucket repos go through the same admin-approval gate GitHub/GitLab do — a repo won't run a review
until approved. Approve it via the `lci` admin TUI or the equivalent admin API call, same as any
other platform (see [the `lci` TUI docs](../adr/0063-cli-only-repository-approval.md) if you haven't
approved a repo before).

---

## Demo: prove one webhook produces one review, end to end

1. **Open a real PR** on the configured Bitbucket repo (or push a small commit to an existing one).
2. **Watch the control plane logs** for the `webhook.receive` span — it should show
   `platform="bitbucket"`, the event key (e.g. `pullrequest:created`), and `accepted webhook`.
3. **Confirm a task was created**: query `tasks` for a row with `platform='bitbucket'` and
   `target_id` matching the PR number, or check it via the `lci` TUI / `/tasks` API.
4. **Watch the dispatcher launch a Job** for that task (same as any other platform — Bitbucket
   changes nothing about task execution, only ingress/egress).
5. **Confirm the review posts back** to the PR: a general comment (the review body) plus inline
   comments on the changed lines. Bitbucket has no aggregate "review" object like GitHub's, so this
   posts as one general comment + N inline comments rather than one grouped review — that's the
   platform's own API shape, not a bug.
6. **Trigger a manual re-review**: comment `@lightbridge-bot please review` (or your configured
   `bot_handle`) on the PR — confirm a second task is created and a second pass posts.
7. **Push to the default branch** and confirm a re-index task is queued (check `tasks` for a
   `target_type='index'` row, or the indexing logs).

If all seven steps produce the expected row/log/comment, the integration is live end to end.

## Known gaps (by platform limitation, not oversight)

- **No 👍/👎 feedback loop**: Bitbucket Cloud's REST API v2.0 has no comment-reaction (award-emoji)
  endpoint, so `add_reaction`/`list_comment_reactions` are no-ops/always-empty. The feedback
  suppression mechanism (ADR-0035) simply never has anything to find on this platform — this is not
  something to debug, it's the documented ceiling.
- **No PR labels**: Bitbucket Cloud pull requests have no native label feature, so outcome labels
  (e.g. "needs-changes") never appear on Bitbucket PRs.
- **`list_changed_files` reconstructs per-file diffs** from Bitbucket's one whole-PR unified-diff
  endpoint (there's no per-file JSON diff endpoint the way GitHub/GitLab have) — this works for the
  common case but is a weaker guarantee than GitHub/GitLab's structured per-file API.

## Rollback

Set `"bitbucket": { "enabled": false }` (or delete the section — it's `#[serde(default)]`, so an
absent section is equivalent) and redeploy. No data migration: existing `tasks`/`outbox` rows with
`platform='bitbucket'` are simply never claimed again by a role that no longer has a Bitbucket
`CodePlatform` registered — same lossless "config-scoped, not data-scoped" shutoff GitLab uses.

## See also

- [ADR-0072](../adr/0072-platform-abstraction-layer.md) — the `CodePlatform` trait decision.
- [ADR-0108](../adr/0108-codeplatform-github-gitlab-bitbucket.md) — activating the trait for
  GitHub/GitLab and adding Bitbucket.
- [Kubernetes and deployment — Bitbucket configuration](../kubernetes-deployment.md#bitbucket-configuration-adr-0108) —
  the config-reference companion to this runbook.
- [ADR-0035](../adr/0035-review-feedback-signal.md) — the 👍/👎 feedback loop Bitbucket can't
  participate in (no reaction API).
