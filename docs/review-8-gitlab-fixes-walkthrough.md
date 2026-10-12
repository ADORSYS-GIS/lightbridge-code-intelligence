# Review 8 GitLab Fixes Walkthrough

## Summary

This follow-up addresses the valid Review 8 comments around the file-only, multi-project GitLab
configuration work.

The main correction is that `path_with_namespace` is no longer part of GitLab configuration or
webhook authentication. The configured `project_id` is enough to select the project, and the
per-project `webhook_secret` authenticates the request.

`path_with_namespace` is still read from GitLab webhook payloads only after routing/verification
logic needs repository identity, for example to derive `owner/name` from GitLab payload metadata.
It is not a configured field and is not used as an auth cross-check.

## Review Decisions

| Review | Validity | Action |
|---|---|---|
| Remove `path_with_namespace` config/auth cross-check | Valid | Removed from config structs, registry entries, examples, and webhook verification. |
| Duplicate header validation in `GitlabClient::new` | Valid | Removed constructor-level token/header checks because `GitlabSection::validate` already validates them with `project_id` context. |
| Parse GitLab JSON before token verification | Valid concern, current approach acceptable | Kept the pre-verification parse limited to `project.id` for secret selection; no task mutation, persistence, or accepted delivery behavior happens before token verification. |
| Startup fails if `gitlab.enabled=true` has invalid config | Valid trade-off, kept intentionally | File-only config is strict and fail-loud; invalid enabled GitLab config should not be silently disabled. |
| Tests covered obsolete `verify_gitlab_token` helper | Valid | Removed the dead helper and replaced tests with registry-driven webhook verifier tests. |
| `clone_url` cannot surface missing project errors cleanly | Valid limitation | Kept the trait-compatible fallback but logs a warning when no configured GitLab project matches the repo. |

## Why `path_with_namespace` Was Removed

GitLab project IDs are stable across project renames and namespace transfers. A configured
`path_with_namespace` equality check adds a failure mode without adding meaningful authentication:

```rust
let project_id = payload["project"]["id"].as_i64();
let project = registry.get(project_id);
project.client.verify_webhook(headers, body);
```

With this model:

- `project.id` selects the configured project entry.
- that project's `webhook_secret` verifies the request.
- project renames or namespace transfers do not break webhook delivery.
- no operator has to keep a duplicate path field in sync with GitLab.

An attacker who spoofs `project.id` still needs the matching configured webhook secret for that
project, so the path check does not materially improve authentication.

## Config Shape

GitLab is configured only through `control-plane.json`; there is no legacy `GITLAB_*` fallback.

```json
{
  "gitlab": {
    "enabled": true,
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

## Implementation Notes

### Config Validation

`GitlabSection::validate` now focuses on fields that are actually configured:

```rust
for project in &self.projects {
    if !seen.insert(project.project_id) {
        anyhow::bail!("duplicate GitLab project_id {}", project.project_id);
    }
    if project.access_token.trim().is_empty() {
        anyhow::bail!("GitLab project {} has empty access_token", project.project_id);
    }
    if project.webhook_secret.trim().is_empty() {
        anyhow::bail!("GitLab project {} has empty webhook_secret", project.project_id);
    }
}
```

### Client Construction

`GitlabClient::new` now trusts validated config and only constructs the HTTP client:

```rust
pub fn new(api_url: String, token: String, webhook_secret: String) -> anyhow::Result<Self> {
    let http = Client::builder()
        .user_agent("lightbridge-code-intelligence")
        .build()?;

    Ok(Self {
        api_url: api_url.trim_end_matches('/').to_string(),
        token,
        webhook_secret,
        http,
    })
}
```

### Webhook Verification

The production verifier is registry-based:

```rust
let Some(project_id) = payload["project"]["id"].as_i64() else {
    return false;
};
let Some(registry) = registry else {
    return false;
};
let Some(project) = registry.get(project_id) else {
    return false;
};

project.client.verify_webhook(headers, body)
```

This keeps behavior fail-closed for missing `project.id`, unknown projects, missing registry, or wrong
webhook token.

## Verification

Commands run:

```bash
cargo test -p control-plane gitlab -- --nocapture
cargo check -p control-plane
cargo clippy -p control-plane --all-targets -- -D warnings
rg -n 'std::env::var\("GITLAB_|GitlabClient::from_env|gitlab_webhook_secret|gitlab_app_handle|state\.gitlab_app_handle|state\.gitlab_webhook_secret' services/control-plane/src
rg -n 'path_with_namespace' services/control-plane/src/config.rs services/control-plane/src/integrations/gitlab.rs
```

Results:

- `cargo test -p control-plane gitlab -- --nocapture`: passed, 23 tests.
- `cargo check -p control-plane`: passed.
- `cargo clippy -p control-plane --all-targets -- -D warnings`: passed.
- GitLab env-config guard search in `services/control-plane/src`: no matches.
- `path_with_namespace` guard search in `config.rs` and `integrations/gitlab.rs`: no matches.
