# PR Title

Configure GitLab projects from file config only

## 1. Summary

This PR changes:

- Adds `gitlab.projects[]` to `control-plane.json` so each GitLab project has its own API token, webhook secret, API URL, and bot handle.
- Removes the single process-wide GitLab env configuration path (`GITLAB_API_TOKEN`, `GITLAB_API_URL`, `GITLAB_WEBHOOK_SECRET`, `GITLAB_BOT_HANDLE`).
- Routes GitLab webhook verification, clone URL generation, and platform egress through a project-aware GitLab registry keyed by GitLab `project.id`.

It solves:

- `docs/tickets/gitlab-multi-project-config.md`
- `docs/gitlab-file-config-only-implementation-plan.md`

---

## 2. Intent

The intent of this PR is:

> Make GitLab configuration explicit, project-scoped, and fail-closed by using the mounted control-plane file config as the single source of truth. This removes ambiguous env fallback behavior and prevents one GitLab token or webhook secret from being reused across all projects.

---

## 3. Scope

### In Scope

- Add typed GitLab file config and validation in `services/control-plane/src/config.rs`.
- Add `GitlabRegistry` and a registry-backed `GitlabPlatformRouter`.
- Verify GitLab webhooks with the configured secret for `payload.project.id`.
- Resolve GitLab clone URLs, MR SHA lookups, bot handles, and egress clients by GitLab project ID.
- Update active deployment/control-plane docs for file-only GitLab config.

### Out of Scope

- Storing GitLab credentials in Postgres.
- Adding admin UI/API flows for GitLab credential management.
- Changing GitHub App installation-token behavior.
- Changing the agent-runner clone contract.
- Implementing GitLab group-level webhook management.

---

## 4. Verification

I verified this change by:

- [x] Running automated tests
- [ ] Running manual tests
- [x] Checking logs
- [ ] Checking metrics
- [x] Testing error cases
- [x] Testing permissions/security behavior
- [ ] Testing rollback or failure behavior, if relevant

Commands run:

```bash
cargo test -p control-plane gitlab -- --nocapture
cargo check -p control-plane
cargo clippy -p control-plane --all-targets -- -D warnings
rg -n 'std::env::var\("GITLAB_|GitlabClient::from_env|gitlab_webhook_secret|gitlab_app_handle|state\.gitlab_app_handle|state\.gitlab_webhook_secret' services/control-plane/src
```

Results:

```text
cargo test -p control-plane gitlab -- --nocapture
test result: ok. 23 passed; 0 failed; 0 ignored; 0 measured; 278 filtered out

cargo check -p control-plane
Finished `dev` profile [unoptimized + debuginfo]

cargo clippy -p control-plane --all-targets -- -D warnings
Finished `dev` profile [unoptimized + debuginfo]

GitLab env/source sweep:
No direct GitLab env reads, GitlabClient::from_env, gitlab_webhook_secret, or gitlab_app_handle references remain in services/control-plane/src.
```

---

## 5. Screenshots / Evidence

Add evidence here:

- Screenshot: N/A
- Logs: N/A
- Metrics: N/A
- Recording: N/A

---

## 6. Risk Assessment

Risk level:

- [ ] Low
- [x] Medium
- [ ] High

Potential risks:

- Existing deployments that only set `GITLAB_*` env vars will no longer configure GitLab.
- A malformed `gitlab.projects[]` entry can prevent GitLab from accepting webhooks or generating clone URLs.
- GitLab webhook verification now parses `project.id` before token verification to choose the correct secret.

Mitigation:

- Documented file-only `gitlab.projects[]` deployment config and rollout order.
- Added config validation for enabled-without-projects, duplicate project IDs, empty secrets, and invalid header values.
- The pre-verification GitLab payload parse is used only to select/cross-check the configured project; no delivery is persisted and no task is created until the project-specific webhook token is verified.

---

## 7. AI Usage Declaration

AI was used for:

- [x] Understanding existing code
- [x] Generating code
- [x] Refactoring
- [x] Generating tests
- [x] Drafting documentation
- [x] Reviewing the diff
- [ ] Not used

Human verification:

- [ ] I understand every meaningful change in this PR
- [ ] I checked generated code manually
- [ ] I checked generated tests manually
- [ ] I removed unsupported AI assumptions
- [ ] I accept responsibility for this PR

---

## 8. Reviewer Focus

Please focus your review on:

- [x] Correctness
- [x] Architecture
- [x] Security
- [ ] Performance
- [x] Tests
- [x] Maintainability
- [x] Product intent
- [x] Edge cases
