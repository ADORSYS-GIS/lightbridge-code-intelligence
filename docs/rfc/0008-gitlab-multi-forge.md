# RFC-0008: GitLab / multi-forge — retrospective, gap analysis, and phasing

- **Status:** Proposed
- **Author(s):** Stephane Segning (@stephane-segning), grounded in code authored by
  Christian Leghadjeu (@leghadjeu-christian)
- **Date:** 2026-07-14
- **Resulting ADRs:** none new from this RFC's own recommendations (see Unresolved questions). The
  `Forge` boundary itself is **already** recorded as
  [ADR-0072](../adr/0072-platform-abstraction-layer.md) (Accepted, 2026-07-06) — this RFC does not
  re-decide it, it audits it against [#253](https://github.com/vymalo/lightbridge-code-intelligence/issues/253)'s
  four surfaces and recommends what (if anything) to do about what's left.

## Summary

Issue [#253](https://github.com/vymalo/lightbridge-code-intelligence/issues/253) asked for an RFC
proposing a `Forge` trait/boundary to let Lightbridge support GitLab without forking the
GitHub-only pipeline, on the premise that the system is "GitHub-only, end to end." **That premise
is stale.** A `CodePlatform` trait already exists
([`platform.rs`](../../services/control-plane/src/integrations/platform.rs)), `GithubApp` and a
new `GitlabClient` ([`gitlab.rs`](../../services/control-plane/src/integrations/gitlab.rs)) both
implement it, the webhook router detects platform from headers and dispatches to
platform-specific handlers that land on a shared schema, and the reconciler drains one `outbox`
table through a `HashMap<Platform, Arc<dyn CodePlatform>>`. This shipped as
[ADR-0072](../adr/0072-platform-abstraction-layer.md) (PR
[#283](https://github.com/vymalo/lightbridge-code-intelligence/pull/283) and follow-ups through
2026-07-13) and is documented as a deployable, ExternalSecrets-projected configuration in
[`docs/kubernetes-deployment.md`](../kubernetes-deployment.md#gitlab-configuration-adr-0072).

This RFC therefore does three things instead of proposing the boundary from scratch:

1. **Maps the four surfaces from #253 (ingress, forge-auth, egress, identity) onto what's actually
   built**, correcting one factual error in the ticket along the way (§Reference-level,
   Forge-auth).
2. **Identifies the real remaining gaps** — a token-scoping divergence from GitHub's least-privilege
   model, single-tenant GitLab configuration, and the not-yet-forge-abstracted `open`-mode
   write-back — none of which are "GitLab is unsupported," all of which are "here is what GitLab
   support does *not* yet do."
3. **Gives a build/no-build call on each gap**, per the repo's stated discipline against building
   for hypothetical future requirements — except the single-tenant gap, which turned out to already
   be in flight as its own PR while this RFC was being drafted (§Gap 2); this RFC defers to that
   work rather than re-deciding or re-reviewing it.

## Motivation

An RFC's job is to let reviewers weigh alternatives *before* a decision is made — but the decision
here was already made and shipped nine days before this ticket was filed. Writing a
forward-looking "here's how we'd build it" proposal would misrepresent the current system to any
reviewer who reads it, and it would silently duplicate ADR-0072's design record instead of
reconciling with it. The more useful thing an RFC can do at this point is: confirm the boundary
holds up under the specific lens #253 asked for (four surfaces, a GitLab mapping, a phasing call),
surface anything that lens finds that ADR-0072 didn't already resolve, and give the repo owner a
clean point to close or re-scope #253 from.

The gap that motivated #253 in the first place — "can Lightbridge point at a GitLab MR the same
way it points at a GitHub PR" — is real and answered: yes, today, in production configuration.
What's left is narrower and worth being honest about rather than silently declaring victory.

## Guide-level explanation

### The boundary that already exists

Think of the control plane as talking to "a forge" through one seam,
[`CodePlatform`](../../services/control-plane/src/integrations/platform.rs) — an `async_trait`
covering exactly the four surfaces #253 asked about, plus the read paths needed to render a
review:

```rust
#[async_trait]
pub trait CodePlatform: Send + Sync {
    fn name(&self) -> &'static str;
    fn verify_webhook(&self, headers: &HeaderMap, body: &[u8]) -> bool;      // ingress
    fn delivery_id(&self, headers: &HeaderMap) -> Option<String>;            // ingress
    fn event_type(&self, headers: &HeaderMap) -> Option<String>;             // ingress
    async fn list_changed_files(&self, repo: &RepoRef, pr_number: i64) -> anyhow::Result<Vec<ChangedFile>>;
    async fn default_branch(&self, repo: &RepoRef) -> anyhow::Result<String>;
    async fn pr_shas(&self, repo: &RepoRef, pr_number: i64) -> anyhow::Result<(Option<String>, Option<String>)>;
    async fn post_review(&self, repo: &RepoRef, review: &ReviewPost) -> anyhow::Result<PostedReview>;   // egress
    async fn post_comment(&self, repo: &RepoRef, issue_number: i64, body: &str, noteable_type: Option<&str>) -> anyhow::Result<PostedComment>; // egress
    async fn add_reaction(&self, repo: &RepoRef, target: ReactionTarget, emoji: &str, noteable_type: Option<&str>) -> anyhow::Result<()>;
    async fn add_labels(&self, repo: &RepoRef, issue_number: i64, labels: &[String]) -> anyhow::Result<()>;
    async fn list_review_comments(&self, repo: &RepoRef, pr_number: i64, review_id: i64) -> anyhow::Result<Vec<ReviewCommentRef>>;
    async fn list_comment_reactions(&self, repo: &RepoRef, comment_id: i64, is_review_comment: bool, iid: Option<i64>, noteable_type: Option<&str>) -> anyhow::Result<Vec<Reaction>>;
    fn clone_url(&self, repo: &RepoRef) -> String;
}
```

`GithubApp` implements it exactly as before (ADR-0072's phases 0–1 were a zero-behavior-change
refactor of the pre-existing GitHub code, verified by the existing test suite). `GitlabClient` is
the new second adapter. The webhook router picks a platform from request headers
(`X-GitHub-Event` vs `X-Gitlab-Event`), and the reconciler picks an implementation from a
`Platform → Arc<dyn CodePlatform>` map built once at startup from whichever of
`GithubApp::from_env()` / `GitlabClient::from_env()` are configured. Neither the task queue, the
`agent-plane`/runner, nor the database schema know or care which forge a task's repo lives on —
`identity`, is genuinely forge-agnostic in the code, in the sense #253 asked for.

### What "identity" and "ingress" look like per forge

- **Identity:** the bot answers to a per-platform handle — `GITHUB_APP_HANDLE`
  (default `lightbridge-assistant`) on GitHub, `GITLAB_BOT_HANDLE` (default `lightbridge-bot`) on
  GitLab — matched by one shared, forge-agnostic `mentions_handle()` helper
  ([`webhook.rs:591`](../../services/control-plane/src/http/webhook.rs)) that both
  `handle_issue_comment` (GitHub) and `handle_gitlab_note` (GitLab) call. The default strings
  differ (`lightbridge-assistant` vs `lightbridge-bot`); both are operator-configurable, so this is
  a cosmetic asymmetry, not a functional gap.
- **Ingress:** one route (`POST /webhook`), one `detect_platform()` header sniff, then a
  platform-specific signature check (GitHub: `X-Hub-Signature-256` HMAC-SHA256 over the raw body;
  GitLab: `X-Gitlab-Token` compared to `GITLAB_WEBHOOK_SECRET` in constant time — GitLab's webhook
  model offers no HMAC primitive, so this is the strongest verification the platform itself
  supports, not a weaker choice Lightbridge made) and a platform-specific delivery-id header
  (`X-GitHub-Delivery` vs `X-Gitlab-Event-UUID`), both landing in one `webhook_deliveries` table
  keyed by `(platform, delivery_id)`.

## Reference-level explanation

### The four surfaces, GitHub vs GitLab, as implemented today

| Surface | GitHub (`GithubApp`) | GitLab (`GitlabClient`) | Forge-agnostic core |
|---|---|---|---|
| **Ingress** | `X-Hub-Signature-256` HMAC-SHA256, `GITHUB_WEBHOOK_SECRET` | `X-Gitlab-Token`, constant-time compare, `GITLAB_WEBHOOK_SECRET` | `detect_platform()`, `webhook_router()`, `webhook_deliveries(platform, delivery_id)` — [`webhook.rs:25-142`](../../services/control-plane/src/http/webhook.rs) |
| **Forge-auth** | App private key → RS256 JWT → per-installation `installation_token()`, ~1h TTL, cached per `installation_id` — [`github.rs:66-129`](../../services/control-plane/src/integrations/github.rs) | one static `GITLAB_API_TOKEN` (PAT / project / group access token), sent as `PRIVATE-TOKEN`, no minting, no expiry rotation, one token for every configured project — [`gitlab.rs:1-73`](../../services/control-plane/src/integrations/gitlab.rs) | `RepoRef.installation_id` doubles as "GitHub installation ID" or "GitLab project ID" depending on platform; `CodePlatform` implementations own their own auth internally, callers never see a token |
| **Egress** | `create_pr_review`: one grouped review + inline `ReviewComment`s (`line`/`side`/`start_line`/`start_side`) via the PR Reviews API | no "review" aggregate exists on GitLab: `post_review` fetches `diff_refs` (base/head/start SHA), posts each inline finding as its own MR **discussion** with a `position` object, then the review body as a separate MR **note** — [`gitlab.rs:278-336`](../../services/control-plane/src/integrations/gitlab.rs) | reconciler's `HashMap<Platform, Arc<dyn CodePlatform>>`, one `outbox` table (`platform` column), single-writer/single-replica invariant (ADR-0059) — [`reconciler.rs`](../../services/control-plane/src/queue/reconciler.rs) |
| **Identity** | `GITHUB_APP_HANDLE` (default `lightbridge-assistant`) | `GITLAB_BOT_HANDLE` (default `lightbridge-bot`) | `mentions_handle()` — [`webhook.rs:591`](../../services/control-plane/src/http/webhook.rs) — shared by both |

### Correcting the ticket: forge-auth is not what #243/ADR-0092 hardened

#253's "Current Behavior" describes forge-auth as "GitHub App private key → per-installation
short-lived tokens (and per #243, now per-task scoped — read the current state of that after
PR #410)." That conflates two different credentials:

- The **forge credential** — the GitHub App installation token `GithubApp::installation_token`
  mints to call GitHub's API. This is what "forge-auth" means in #253's own four-surface framing,
  and it is unchanged by #243.
- The **internal credential** — `AGENT_RUNNER_TOKEN`, the bearer the `agent-plane` Job presents to
  the control plane's `/internal/tasks/{id}/...` routes. [ADR-0092](../adr/0092-per-task-runner-tokens.md)
  (issue [#243](https://github.com/vymalo/lightbridge-code-intelligence/issues/243), PR
  [#410](https://github.com/vymalo/lightbridge-code-intelligence/pull/410), 2026-07-14) hardened
  *this* credential from one shared long-lived secret to a per-task signed JWT. It authenticates a
  Job to the control plane, not the control plane to a forge, and its scope is identical for a
  GitHub-backed task and a GitLab-backed task — it sits entirely below the `Forge` boundary and
  this RFC's four surfaces don't reach it.

The forge-auth surface's real, GitLab-specific story is the token-scoping divergence below.

### Gap 1 — GitLab forge-auth has no analog to GitHub's least-privilege model

[ADR-0001](../adr/0001-use-github-app.md) chose a GitHub App specifically for "least privilege and
per-installation scoping" and "short-lived, narrowly scoped credentials." `GitlabClient` cannot
replicate that today: it holds **one** static token, valid for **every** project it was granted
access to, for as long as the token itself lives (a GitLab PAT or group/project access token can be
configured for up to a year, or with no expiry on some GitLab versions). A leaked
`GITLAB_API_TOKEN` env value is a wider blast radius than a leaked GitHub installation token, which
expires in about an hour and is scoped to one installation.

This is not a bug in `GitlabClient` — it is GitLab's actual auth surface. GitLab has no "App +
installation" concept; the closest analogs are:

- **A GitLab OAuth application** with a user-consent + refresh-token flow, mintable per-project
  access on demand — architecturally closer to GitHub's App model, but a materially bigger build
  (OAuth app registration, redirect URI, refresh-token storage and rotation, and a
  per-tenant-authorization UX that doesn't exist anywhere in this system today).
- **Project/group access tokens**, scoped narrowly and rotated by an operator out of band —
  cheaper, and already exactly what `GitlabClient::from_env` expects; the "scoping" would come from
  which projects the token's group/project membership actually grants, decided at token-creation
  time in GitLab, not in code here.

Note: whatever ships from Gap 2 below (per-project tokens) narrows this gap's blast radius —
a leaked token then compromises one project instead of every configured one — without adding the
missing piece, minting/expiry. The two gaps are related but distinct; Gap 2's fix does not close
Gap 1.

### Gap 2 — one GitLab tenant per control-plane deployment

> **⚠️ Already in flight — out of this RFC's scope.** [PR #414](https://github.com/vymalo/lightbridge-code-intelligence/pull/414)
> (@leghadjeu-christian, opened 2026-07-14, solving [ai-helm#653](https://github.com/ADORSYS-GIS/ai-helm/issues/653))
> replaces the single global `GITLAB_API_TOKEN`/`GITLAB_API_URL`/`GITLAB_WEBHOOK_SECRET`/`GITLAB_BOT_HANDLE`
> env vars with a `gitlab.projects[]` array in `control-plane.json` — a `GitlabRegistry` +
> `GitlabPlatformRouter` give each configured GitLab project its own access token, webhook secret,
> API URL, and bot handle, selected by the webhook payload's `project.id`. That is exactly the gap
> described below. It was opened the same day as this RFC, independently of it — this RFC's
> original draft called this gap "no-build until a second tenant is real"; that reasoning no longer
> holds, because a real need was already being built. Rather than re-litigate a design already
> under active review (gemini-code-assist, lightbridge-assistant, and the repo owner have all left
> substantive comments on #414 — a startup-blast-radius footgun where one bad project entry takes
> down GitHub too, a `path_with_namespace` cross-check that breaks silently on project rename, a
> pre-auth JSON-parse surface, and a test-coverage gap on the new verification path, among others),
> this RFC just points at it and steps back. The description below is retained as the *problem*
> statement Gap 2 was written against; treat #414 as its answer, not this RFC.

`GITLAB_API_TOKEN` and `GITLAB_API_URL` are single, global env vars. GitHub's App model lets one
Lightbridge deployment serve arbitrarily many GitHub orgs, each with its own installation and
installation-scoped token, chosen per webhook via `installation_id`. GitLab's model as built
supports exactly one GitLab instance (gitlab.com **or** one self-hosted host) and one token's worth
of project access — there is no per-project or per-group token routing. Onboarding a second,
independent GitLab tenant (a different customer's self-hosted instance, say) would require either a
second full deployment or a real multi-token `Platform → Vec<GitlabClient>` routing layer keyed on
something GitLab's webhook payload can identify the source instance/token by.

### Gap 3 — `open`-mode write-back is not in `CodePlatform` for either forge

RFC-0007 / [ADR-0088](../adr/0088-open-mode-autonomous-ticket-agent.md)'s autonomous `open` mode
needs to push a branch and open a PR/MR. The reconciler already has an outbox `kind` for it
(`pr_open`) and a tested offload/rehydrate/hash-verify path, but the delivery arm is a deliberate
dead end today:

> "the credentialed egress (branch push + PR open against the forge) is not activated in this
> slice (it needs a `CodePlatform::open_pull_request` and a security sign-off)... No `pr_open`
> intent is produced in prod (no trigger), so this arm is dormant."
> — [`reconciler.rs:309-330`](../../services/control-plane/src/queue/reconciler.rs)

This is GitHub-shaped by omission, not by GitHub-specific code: `CodePlatform` simply doesn't have
an `open_pull_request`/`push_branch` method yet, for either forge, because `open` mode itself isn't
live anywhere. Extending the trait is cheap whenever it's needed; there is nothing GitLab-specific
to design here that isn't already covered by the existing `post_review`/`clone_url` pattern.

### Known limitations already on record (not new findings)

ADR-0072's own "Known Limitations" section already documents two low-risk items in `gitlab.rs`: the
clone-URL token/path are not URL-encoded (low risk — GitLab tokens and project paths are
alphanumeric-plus-hyphen in practice), and `authenticated_clone_url()`'s `@`-detection heuristic
could misfire on a GitLab subgroup path containing `@` (neither platform actually allows that in
practice). Both are already tracked there; this RFC doesn't re-litigate them.

### Stale documentation found in passing

`platform.rs`'s and `github.rs`'s module doc comments still say "Phase 0... not yet wired into the
webhook handler, outbox, or reconciler — that happens in Phases 2–3," and `#![allow(dead_code)]`
annotations that were needed during that phase are still present. `main.rs` shows both are in fact
wired (`platforms.insert(Platform::GitHub, ...)` / `platforms.insert(Platform::GitLab, ...)` at
startup, feeding `queue::reconciler::run`). This is a doc-comment bug, not a functional one — it's
what led this RFC's own research to nearly repeat #253's mistake before checking `main.rs`
directly. Worth a trivial follow-up fix; not a reason to touch the trait itself.

## Drawbacks

- **This RFC's format is unusual for this repo.** RFCs normally precede a decision; here the
  decision (ADR-0072) predates the RFC by more than a week. Filing it anyway is deliberate: #253
  asked a specific, reviewable question, and closing it with only a GitHub-comment answer would
  leave no durable design-review record for the gaps this RFC did find (Gaps 1–3), which are real
  even though the headline ask is already resolved.
- **The token-scoping gap (Gap 1) is being accepted, not fixed, by this RFC's own recommendation
  below.** That is a real, if bounded, security tradeoff for any GitLab tenant this system serves
  today or in the future, and it should be visible to whoever configures `GITLAB_API_TOKEN`, not
  buried in a source comment.

## Alternatives

- **Do nothing — close #253 with a comment pointing at ADR-0072.** Cheapest, and technically
  answers the ticket's literal ask (a `Forge` boundary exists, GitLab is mapped onto it). Rejected
  as the *sole* action because it would leave Gaps 1–3 undocumented anywhere reviewable, and #253's
  acceptance criteria explicitly asked for a build/no-build + phasing call, which a bare "already
  done" comment doesn't give.
- **Build the heavier OAuth-app + refresh-token flow for Gap 1, speculatively.** Still rejected —
  no evidence a GitLab tenant needs per-project *minted, expiring* tokens rather than
  operator-managed static ones. This is distinct from Gap 2's per-project *static*-token routing,
  which turned out to already be needed and is answered by #414, not by this RFC.
- **Extend `CodePlatform` with `open_pull_request` now, ahead of `open` mode shipping.** Rejected
  for the same reason: no caller exists yet (`open` mode is gated on its own security sign-off,
  RFC-0007). Building the forge method first would mean designing its contract without a real
  caller to validate it against.

## Unresolved questions

- **Gap 1 (token blast radius) — no-build, operational mitigation only.** Recommend documenting
  (in `docs/kubernetes-deployment.md`, already the right place) that operators should provision
  `GITLAB_API_TOKEN` as a **project or group access token scoped to only the repos Lightbridge
  reviews**, with the shortest expiry GitLab allows for the deployment's rotation cadence, and a
  rotation runbook — not a new minting subsystem. No ADR needed unless an operator incident makes
  the tradeoff unacceptable in practice.
- **Gap 2 (single-tenant GitLab) — already being built, out of scope here.** [PR #414](https://github.com/vymalo/lightbridge-code-intelligence/pull/414)
  is the live answer; its own review thread is where design questions on it belong. This RFC takes
  no further position on it beyond noting it closes the gap.
- **Gap 3 (`open`-mode write-back) — deferred, tracked by ADR-0088 already.** `CodePlatform::open_pull_request`
  should be designed alongside `open` mode's own activation and security sign-off, not ahead of it.
- **Stale doc comments in `platform.rs`/`github.rs`** — mechanical fix, not a design question;
  suitable for a follow-up housekeeping PR rather than blocking on this RFC.
- **Should #253 be closed as resolved, or re-scoped to track Gaps 1–3?** Left to the repo owner —
  this RFC's job was to give the reviewable answer either way, not to make that call.
