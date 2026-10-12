# Support multiple GitLab projects with per-project tokens and webhooks

## Type

Refactor

## Summary

We need to refactor the control-plane GitLab integration so multiple GitLab projects can be configured
through `control-plane.json`, each with its own API access token, webhook secret, GitLab API URL, and
bot handle, because the current implementation uses one process-wide GitLab token and one process-wide
webhook secret.

Expected result:

> The control plane can safely receive webhooks from multiple GitLab projects and use the correct
> project-specific token for webhook verification, API calls, clone URLs, and reconciler write-back,
> without redesigning the whole platform abstraction or changing the agent runner.

## Intent

The intent is to remove the current single-tenant GitLab assumption while keeping the change surgical.
GitLab is already integrated behind the existing `CodePlatform` abstraction, and GitLab tasks already
use `installation_id = project_id`. This ticket should use that existing routing key instead of
introducing a broad database or platform redesign.

The developer should be able to explain:

- why the current `GITLAB_API_TOKEN` / `GITLAB_WEBHOOK_SECRET` model is insufficient;
- why GitLab webhook verification must become project-aware;
- why `tasks.installation_id` is the right existing lookup key for GitLab project credentials;
- why credentials should stay in mounted config/env-backed secrets rather than being stored in
  Postgres for this ticket.

## Source of truth (links)

- Governance source of truth: https://adorsys-gis.github.io/ai-governance/
- Technical investigation: `docs/gitlab-multi-project-config-report.md`
- Existing GitLab implementation plan: `docs/gitlab-implementation-plan.md`
- Existing GitLab integration report: `docs/gitlab-integration-report.md`
- Deployment/config context: `docs/kubernetes-deployment.md`

## Current Behavior

The control plane supports GitLab, but the GitLab integration is configured process-wide:

- `services/control-plane/src/main.rs` stores one `gitlab_webhook_secret`, one
  `Option<gitlab::GitlabClient>`, and one `gitlab_app_handle`.
- `services/control-plane/src/integrations/gitlab.rs` reads `GITLAB_API_TOKEN`,
  `GITLAB_API_URL`, and `GITLAB_WEBHOOK_SECRET` from env in `GitlabClient::from_env()`.
- `services/control-plane/src/http/webhook.rs` verifies every GitLab webhook against the same
  `state.gitlab_webhook_secret`.
- `services/control-plane/src/http/internal.rs` builds GitLab clone URLs from the single configured
  `GitlabClient`, so every GitLab task uses the same token.
- `services/control-plane/src/queue/reconciler.rs` dispatches all GitLab outbox rows to one
  `Arc<dyn CodePlatform>` implementation.

Evidence from the investigation:

- `docs/gitlab-multi-project-config-report.md` identifies the limitation as one GitLab API token,
  one webhook secret, and one bot handle for the whole control-plane process.
- The database already has a useful project routing key: GitLab handlers store
  `installation_id = project_id` when creating repositories and tasks.

## Expected Behavior

The control plane should support multiple configured GitLab projects:

- Operators can define `gitlab.projects[]` in `/etc/lightbridge/control-plane.json`.
- Each configured project can have:
  - `project_id`
  - `api_url`
  - `access_token`
  - `webhook_secret`
  - optional `bot_handle`
- GitLab webhook verification selects the expected secret by `payload.project.id`.
- GitLab API calls and clone URLs select the correct client/token by GitLab project ID.
- GitLab reconciler write-back selects the correct client/token by `RepoRef.installation_id`.
- Existing single-project GitLab env config is removed; there is no legacy fallback.
- GitHub behavior remains unchanged.

Example target config:

```json
{
  "gitlab": {
    "default_api_url": "https://gitlab.com/api/v4",
    "default_bot_handle": "lightbridge-bot",
    "projects": [
      {
        "project_id": 1001,
        "access_token": "<mounted-secret-value>",
        "webhook_secret": "<mounted-secret-value>"
      },
      {
        "project_id": 1002,
        "api_url": "https://gitlab.internal.example/api/v4",
        "access_token": "<mounted-secret-value>",
        "webhook_secret": "<mounted-secret-value>",
        "bot_handle": "lb-reviewer"
      }
    ]
  }
}
```

## Acceptance Criteria

- [ ] Given `control-plane.json` contains multiple `gitlab.projects[]` entries, when the control plane
      starts, then it builds a GitLab project registry keyed by `project_id`.
- [ ] Given two GitLab projects have different webhook secrets, when project A sends a webhook with
      project A's secret, then the webhook is accepted.
- [ ] Given project A sends a webhook with project B's webhook secret, when the control plane verifies
      the webhook, then it rejects the request with `401 Unauthorized`.
- [ ] Given a GitLab webhook payload has a `project.id` that is not configured, when it is received,
      then the control plane fails closed and does not create or update tasks.
- [ ] Given a GitLab task belongs to project A, when the agent runner fetches task context, then the
      returned GitLab clone URL is built with project A's token only.
- [ ] Given a GitLab outbox row belongs to project A, when the reconciler posts review/comment/reaction
      egress, then it uses project A's configured GitLab client/token only.
- [ ] Given only legacy `GITLAB_API_TOKEN`, `GITLAB_API_URL`, `GITLAB_WEBHOOK_SECRET`, and
      `GITLAB_BOT_HANDLE` env vars are set, when the control plane starts, then GitLab is not
      configured from those env vars.
- [ ] Given GitHub webhooks and GitHub outbox rows, when this refactor is deployed, then existing
      GitHub behavior remains unchanged.
- [ ] Config validation rejects duplicate `project_id` values, empty `access_token`, empty
      `webhook_secret`, and invalid header bytes.
- [ ] Relevant unit and integration tests are added or updated.
- [ ] Documentation is updated for `gitlab.projects[]`, deploy ordering, and the removal of
      GitLab env fallback.
- [ ] Verification evidence is added to the implementation PR.

## Out of Scope

- Storing GitLab access tokens or webhook secrets in Postgres.
- Building admin APIs or UI screens to manage GitLab credentials dynamically.
- Replacing the `CodePlatform` trait wholesale.
- Changing the agent-runner clone contract.
- Changing GitHub App installation-token behavior.
- Adding GitLab group-level webhook management.
- Rotating credentials at runtime without a deploy/restart.
- Fixing unrelated GitLab limitations such as feedback polling, note threading edge cases, or URL
  encoding unless directly required by this refactor.

## Technical Context

Primary files:

- `services/control-plane/src/config.rs` — add `GitlabSection` and `GitlabProjectConfig`.
- `services/control-plane/src/integrations/gitlab.rs` — add config-based constructor,
  `GitlabRegistry`, and likely a registry-backed `GitlabPlatformRouter`.
- `services/control-plane/src/main.rs` — build the registry from file config and register GitLab in
  `AppState.platforms`; do not read `GITLAB_*` env vars.
- `services/control-plane/src/http/webhook.rs` — make GitLab verification project-aware by parsing
  `payload.project.id` before token comparison.
- `services/control-plane/src/http/internal.rs` — resolve GitLab clone URL by
  `context.installation_id`.
- `services/control-plane/src/queue/reconciler.rs` — keep platform dispatch stable by using a
  registry-backed GitLab platform adapter, or resolve GitLab project client before each egress call.
- `docs/kubernetes-deployment.md` — document multi-project GitLab config and removal of GitLab env fallback.

Important existing constraints:

- `control-plane.json` uses strict `deny_unknown_fields`; deploy order matters:
  binary first, chart second, values third.
- GitLab webhook auth is a plain `X-Gitlab-Token` comparison, not HMAC.
- For per-project webhook verification, unsigned GitLab JSON must be parsed only far enough to read
  `project.id`; no trusted action may happen until after the project-specific token is verified.
- GitLab already uses `installation_id = project_id`; preserve that convention.
- The database schema probably does not need to change for this ticket.

Suggested implementation shape:

```rust
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitlabSection {
    pub default_api_url: Option<String>,
    pub default_bot_handle: Option<String>,
    #[serde(default)]
    pub projects: Vec<GitlabProjectConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitlabProjectConfig {
    pub project_id: i64,
    pub api_url: Option<String>,
    pub access_token: String,
    pub webhook_secret: String,
    pub bot_handle: Option<String>,
}
```

Suggested registry shape:

```rust
#[derive(Clone)]
pub struct GitlabProject {
    pub project_id: i64,
    pub bot_handle: String,
    pub client: GitlabClient,
}

#[derive(Clone, Default)]
pub struct GitlabRegistry {
    by_project_id: std::collections::HashMap<i64, GitlabProject>,
}

impl GitlabRegistry {
    pub fn get(&self, project_id: i64) -> Option<&GitlabProject> {
        self.by_project_id.get(&project_id)
    }

    pub fn client_for_repo(&self, repo: &RepoRef) -> Option<&GitlabClient> {
        self.get(repo.installation_id).map(|project| &project.client)
    }
}
```

## Risks

| Risk | Mitigation |
|---|---|
| Parsing unsigned GitLab JSON before verification | Parse only `project.id`; do not act on payload until after token verification. |
| Project ID spoofing | Verify with the secret selected by the claimed `project.id`; an attacker still needs that project's secret. |
| Wrong token used for clone or write-back | Resolve all GitLab clients by `installation_id = project_id`; never fall back to another configured project. |
| Secret leakage in logs | Keep existing redaction and never log `access_token`, `webhook_secret`, or full authenticated `clone_url`. |
| Config mistakes crash production | Validate duplicates/empty fields on startup; document strict `deny_unknown_fields` deploy order. |
| Stale `GITLAB_*` env vars hide misconfiguration | Remove the fallback entirely; GitLab is configured only from `control-plane.json`. |
| Over-broad refactor | Keep database, agent runner, GitHub integration, and review shaping out of scope unless directly necessary. |

## Test Plan

### Unit tests

```bash
cargo test -p control-plane gitlab
```

Expected:

- `GitlabRegistry` accepts valid multi-project config.
- `GitlabRegistry` rejects duplicate project IDs.
- `GitlabRegistry` rejects empty access tokens and webhook secrets.
- `GitlabRegistry` rejects invalid header values.
- Project lookup by `RepoRef.installation_id` returns the expected client.

### Webhook handler tests

```bash
cargo test -p control-plane webhook
```

Expected:

- Project A webhook + secret A returns accepted.
- Project A webhook + secret B returns unauthorized.
- Unknown GitLab `project.id` returns unauthorized or bad request without task creation.
- GitHub webhook tests still pass unchanged.

### Internal task context tests

```bash
cargo test -p control-plane get_context
```

Expected:

- GitLab task for project A returns a pre-authenticated clone URL using project A's token.
- GitLab task for project B returns a pre-authenticated clone URL using project B's token.
- Missing GitLab project config returns `503` and does not use any fallback token.

### Reconciler tests

```bash
cargo test -p control-plane reconciler
```

Expected:

- GitLab outbox rows are delegated to the project-specific client selected by
  `RepoRef.installation_id`.
- Missing project config fails closed and does not post with a different token.
- GitHub outbox behavior remains unchanged.

### Full control-plane checks

```bash
cargo test -p control-plane
cargo clippy -p control-plane --all-targets -- -D warnings
```

Expected:

- All relevant tests pass.
- No new clippy warnings.
- Any tests requiring live Postgres are either run with the documented local setup or explicitly
  reported as not run with justification.

### Manual verification

- [ ] Start control plane with two configured GitLab projects.
- [ ] Send a signed GitLab webhook fixture for project A using secret A; verify accepted.
- [ ] Resend project A fixture using secret B; verify rejected.
- [ ] Trigger or simulate task context for project A; verify clone URL uses project A credentials
      without logging the token.
- [ ] Trigger or simulate reconciler write-back for project A; verify it selects project A client.
- [ ] Verify existing GitHub webhook path still works.
- [ ] Check logs for no token or authenticated clone URL leakage.

## Verification evidence

Implementation PR must include:

- Test commands run and full pass/fail output.
- Manual webhook verification steps and observed status codes.
- Evidence that GitLab project A cannot use project B's webhook secret.
- Evidence that task context and reconciler use the project-specific GitLab token.
- Evidence that GitHub behavior was regression-checked.
- Logs or screenshots showing no secret values are emitted.
- Any known remaining limitations or skipped tests with justification.

At ticket creation time:

- Technical investigation completed in `docs/gitlab-multi-project-config-report.md`.
- Implementation not yet completed.
- Runtime verification not yet completed.

## Human accountable owner

@leghadjeu-christian

## AI Usage Declaration

AI was used for:

- [x] Drafting the ticket
- [x] Understanding code
- [x] Proposing implementation
- [ ] Generating code
- [ ] Refactoring
- [ ] Generating tests
- [x] Writing documentation
- [ ] Reviewing the diff
- [ ] Not used

Human verification:

- [ ] I understood the intent
- [ ] I checked the source of truth
- [ ] I reviewed all AI-generated text/code
- [ ] I verified the implementation manually
- [ ] I verified the tests
- [ ] I checked for hallucinated assumptions
- [ ] I documented remaining risks
- [ ] I am the accountable owner and accept responsibility for this ticket.

## Human verification completed

- [ ] I understood the intent
- [ ] I checked the source of truth
- [ ] I reviewed all AI-generated text/code
- [ ] I verified the implementation manually
- [ ] I verified the tests
- [ ] I checked for hallucinated assumptions
- [ ] I documented remaining risks
- [ ] I am the accountable owner and accept responsibility for this ticket.
