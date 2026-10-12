# GitLab File-Only Multi-Project Configuration — Detailed Implementation Plan

## 1. Goal

Refactor the control-plane GitLab integration so GitLab is configured **only** through
`control-plane.json`, with no direct `GITLAB_*` environment-variable configuration, no legacy fallback,
and no deprecated single-project mode.

The target state is:

- GitLab projects are configured in `control-plane.json`.
- Each GitLab project has its own access token, webhook secret, GitLab API URL, and optional bot
  handle.
- The control plane does not read `GITLAB_API_TOKEN`, `GITLAB_API_URL`,
  `GITLAB_WEBHOOK_SECRET`, or `GITLAB_BOT_HANDLE`.
- There is no compatibility path that silently accepts the old env-only GitLab configuration.
- GitHub remains unchanged.
- The agent runner remains unchanged.
- The database schema should remain unchanged unless implementation proves a very specific need.

This plan intentionally removes the old env-based GitLab path instead of deprecating it. That gives one
source of truth, avoids ambiguous credential precedence, and prevents a misconfigured pod from quietly
using stale process env secrets.

## 2. Why Remove GitLab Env Configuration Completely?

The current GitLab implementation is process-wide:

```rust
// services/control-plane/src/integrations/gitlab.rs
pub fn from_env() -> Option<Self> {
    let token = std::env::var("GITLAB_API_TOKEN").unwrap_or_default();
    if token.is_empty() {
        return None;
    }

    let api_url = std::env::var("GITLAB_API_URL")
        .unwrap_or_else(|_| "https://gitlab.com/api/v4".to_string())
        .trim_end_matches('/')
        .to_string();

    let webhook_secret = std::env::var("GITLAB_WEBHOOK_SECRET").unwrap_or_default();
    // ...
}
```

That model is not suitable for multiple GitLab projects because:

1. **One token for all projects is too broad.** A single PAT/group token encourages over-permissioning.
2. **One webhook secret for all projects weakens isolation.** If one project's webhook secret leaks, all
   GitLab webhooks accepted by this control plane are affected.
3. **Env fallback creates ambiguous precedence.** If both file config and env vars exist, operators must
   know which wins. Removing env fallback makes startup behavior deterministic.
4. **File config matches the repo's direction.** The control plane already uses
   `/etc/lightbridge/control-plane.json` for structured config, with strict `deny_unknown_fields`.
5. **Project-specific routing already has a key.** GitLab tasks already use
   `installation_id = project_id`; we can route credentials by project ID without changing the task
   model.

Important nuance: this plan removes **direct GitLab env configuration** from the control-plane code.
For production secret delivery, prefer mounting a Kubernetes Secret or ExternalSecret as the
`control-plane.json` file. Do not rely on `GITLAB_*` process env vars for GitLab.

## 3. Current GitLab Env Seams To Remove

Remove all GitLab-specific env reads and env documentation:

| Location | Current behavior | Target behavior |
|---|---|---|
| `services/control-plane/src/integrations/gitlab.rs` | `GitlabClient::from_env()` reads `GITLAB_API_TOKEN`, `GITLAB_API_URL`, `GITLAB_WEBHOOK_SECRET`. | Delete `from_env()` and construct clients only from `config::GitlabProjectConfig`. |
| `services/control-plane/src/main.rs` | Calls `gitlab::GitlabClient::from_env()`. | Build a `GitlabRegistry` from `file_config.gitlab.projects`. |
| `services/control-plane/src/main.rs` | Stores `gitlab_webhook_secret`. | Remove field; webhook secrets are per project in registry. |
| `services/control-plane/src/main.rs` | Reads `GITLAB_WEBHOOK_SECRET` for readiness. | Readiness checks configured GitLab project count and project validation, not env. |
| `services/control-plane/src/main.rs` | Reads `GITLAB_BOT_HANDLE`. | Bot handle is global default or per-project value in file config. |
| `services/control-plane/src/http/webhook.rs` | Verifies every GitLab webhook against `state.gitlab_webhook_secret`. | Parse `payload.project.id`, find configured project, verify against that project's secret. |
| `services/control-plane/src/http/webhook.rs` | Uses `state.gitlab_app_handle` for mentions. | Resolve bot handle from the configured GitLab project. |
| `services/control-plane/src/http/internal.rs` | Uses one `state.gitlab` client for all GitLab clone URLs. | Resolve GitLab client by `context.installation_id`. |
| `services/control-plane/src/queue/reconciler.rs` | Has one GitLab platform implementation. | Use a registry-backed GitLab platform router that delegates by `RepoRef.installation_id`. |
| `docs/kubernetes-deployment.md` | Documents `GITLAB_*` env vars. | Replace with `control-plane.json` GitLab config documentation. |

## 4. Target `control-plane.json`

Add a top-level `gitlab` section:

```json
{
  "gitlab": {
    "enabled": true,
    "default_api_url": "https://gitlab.com/api/v4",
    "default_bot_handle": "lightbridge-bot",
    "projects": [
      {
        "project_id": 1001,
        "access_token": "glpat-project-a-token",
        "webhook_secret": "project-a-webhook-secret"
      },
      {
        "project_id": 1002,
        "api_url": "https://gitlab.internal.example/api/v4",
        "access_token": "glpat-project-b-token",
        "webhook_secret": "project-b-webhook-secret",
        "bot_handle": "lb-reviewer"
      }
    ]
  }
}
```

Field rules:

| Field | Required | Notes |
|---|---:|---|
| `gitlab.enabled` | no | Defaults to `false`. If `true`, at least one project must be configured. |
| `default_api_url` | no | Defaults to `https://gitlab.com/api/v4` when a project omits `api_url`. |
| `default_bot_handle` | no | Defaults to `lightbridge-bot` when a project omits `bot_handle`. |
| `projects[].project_id` | yes | Numeric GitLab project ID; registry lookup key. |
| `projects[].api_url` | no | Per-project GitLab API URL; supports self-hosted GitLab. |
| `projects[].access_token` | yes | Token sent as `PRIVATE-TOKEN`. |
| `projects[].webhook_secret` | yes | Expected `X-Gitlab-Token` value for that project. |
| `projects[].bot_handle` | no | Mention handle for that project. |

Recommended production secret delivery:

- Store the rendered `control-plane.json` in a Kubernetes Secret or ExternalSecret-managed Secret.
- Mount that Secret as `/etc/lightbridge/control-plane.json`.
- Do not expose GitLab credentials as process env vars.

## 5. Implementation Phases

### Phase 1 — Add file-config types

Update `services/control-plane/src/config.rs`.

Add `gitlab` to `FileConfig`:

```rust
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    pub agent: AgentSection,
    pub dispatcher: DispatcherSection,
    pub review: ReviewSection,
    pub embeddings: EmbeddingsSection,
    pub knowledge_tools: KnowledgeToolsSection,
    pub egress: EgressSection,
    pub gitlab: GitlabSection,
}
```

Add GitLab config structs:

```rust
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitlabSection {
    pub enabled: bool,
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

Add a resolver method so defaults are not duplicated across startup code:

```rust
impl GitlabSection {
    pub fn default_api_url(&self) -> &str {
        self.default_api_url
            .as_deref()
            .unwrap_or("https://gitlab.com/api/v4")
    }

    pub fn default_bot_handle(&self) -> &str {
        self.default_bot_handle
            .as_deref()
            .unwrap_or("lightbridge-bot")
    }
}
```

Validation rules:

- `enabled = true` with empty `projects` is invalid.
- Duplicate `project_id` is invalid.
- Empty `access_token` is invalid.
- Empty `webhook_secret` is invalid.
- Empty resolved `api_url` is invalid.
- Empty resolved `bot_handle` is invalid.
- Invalid HTTP header bytes in `access_token` or `webhook_secret` are invalid.

Suggested validation helper:

```rust
impl GitlabSection {
    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.projects.is_empty() {
            anyhow::bail!("gitlab.enabled=true requires at least one gitlab.projects[] entry");
        }

        let mut seen = std::collections::HashSet::new();
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
            if reqwest::header::HeaderValue::from_str(&project.access_token).is_err() {
                anyhow::bail!("GitLab project {} access_token is not a valid header value", project.project_id);
            }
            if reqwest::header::HeaderValue::from_str(&project.webhook_secret).is_err() {
                anyhow::bail!("GitLab project {} webhook_secret is not a valid header value", project.project_id);
            }
        }

        Ok(())
    }
}
```

### Phase 2 — Replace `GitlabClient::from_env()` with config construction

Update `services/control-plane/src/integrations/gitlab.rs`.

Remove:

```rust
pub fn from_env() -> Option<Self> {
    // delete this entire method
}
```

Add:

```rust
impl GitlabClient {
    pub fn new(api_url: String, token: String, webhook_secret: String) -> anyhow::Result<Self> {
        if reqwest::header::HeaderValue::from_str(&token).is_err() {
            anyhow::bail!("GitLab access token contains invalid header bytes");
        }
        if reqwest::header::HeaderValue::from_str(&webhook_secret).is_err() {
            anyhow::bail!("GitLab webhook secret contains invalid header bytes");
        }

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
}
```

Update module docs to remove references to `GITLAB_API_TOKEN` and `GITLAB_WEBHOOK_SECRET`:

```rust
//! Auth is configured per project from `control-plane.json`.
//! Each configured project has its own access token and webhook secret.
```

### Phase 3 — Add `GitlabRegistry`

Add a project-aware registry in `services/control-plane/src/integrations/gitlab.rs`.

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
```

Build it from file config:

```rust
impl GitlabRegistry {
    pub fn from_config(section: &crate::config::GitlabSection) -> anyhow::Result<Option<Self>> {
        section.validate()?;

        if !section.enabled {
            return Ok(None);
        }

        let mut by_project_id = std::collections::HashMap::new();
        for project in &section.projects {
            let api_url = project
                .api_url
                .clone()
                .unwrap_or_else(|| section.default_api_url().to_string());
            let bot_handle = project
                .bot_handle
                .clone()
                .unwrap_or_else(|| section.default_bot_handle().to_string());

            let client = GitlabClient::new(
                api_url,
                project.access_token.clone(),
                project.webhook_secret.clone(),
            )?;

            by_project_id.insert(
                project.project_id,
                GitlabProject {
                    project_id: project.project_id,
                    bot_handle,
                    client,
                },
            );
        }

        Ok(Some(Self { by_project_id }))
    }

    pub fn get(&self, project_id: i64) -> Option<&GitlabProject> {
        self.by_project_id.get(&project_id)
    }

    pub fn client_for_project(&self, project_id: i64) -> Option<&GitlabClient> {
        self.get(project_id).map(|project| &project.client)
    }

    pub fn client_for_repo(&self, repo: &RepoRef) -> Option<&GitlabClient> {
        self.client_for_project(repo.installation_id)
    }

    pub fn bot_handle(&self, project_id: i64) -> Option<&str> {
        self.get(project_id).map(|project| project.bot_handle.as_str())
    }
}
```

### Phase 4 — Refactor `AppState`

Update `services/control-plane/src/main.rs`.

Remove GitLab env-backed fields:

```rust
pub gitlab_webhook_secret: Arc<String>, // remove
pub gitlab: Option<gitlab::GitlabClient>, // replace
pub gitlab_app_handle: Arc<String>, // remove
```

Use:

```rust
pub gitlab: Option<gitlab::GitlabRegistry>,
```

Build it from `file_config`:

```rust
let gitlab_section = file_config
    .as_ref()
    .map(|config| config.gitlab.clone())
    .unwrap_or_default();

let gitlab = gitlab::GitlabRegistry::from_config(&gitlab_section)?;
```

Remove this startup call completely:

```rust
let gitlab = gitlab::GitlabClient::from_env();
```

Register the registry-backed router in the platform dispatch table:

```rust
if let Some(registry) = gitlab.clone() {
    platforms.insert(
        integrations::platform::Platform::GitLab,
        Arc::new(gitlab::GitlabPlatformRouter::new(registry))
            as Arc<dyn integrations::platform::CodePlatform>,
    );
}
```

Readiness should no longer mention `GITLAB_WEBHOOK_SECRET`:

```rust
let github_configured = !state.github_webhook_secret.is_empty();
let gitlab_configured = state.gitlab.is_some();

if !github_configured && !gitlab_configured {
    return Err((
        StatusCode::SERVICE_UNAVAILABLE,
        "no webhook platform configured",
    ));
}
```

### Phase 5 — Add registry-backed `CodePlatform` router

The reconciler and internal finalize paths already dispatch through:

```rust
HashMap<Platform, Arc<dyn CodePlatform>>
```

Keep that stable by adding a GitLab adapter that implements `CodePlatform` and delegates to the
project-specific `GitlabClient`.

```rust
#[derive(Clone)]
pub struct GitlabPlatformRouter {
    registry: GitlabRegistry,
}

impl GitlabPlatformRouter {
    pub fn new(registry: GitlabRegistry) -> Self {
        Self { registry }
    }

    fn client<'a>(&'a self, repo: &RepoRef) -> anyhow::Result<&'a GitlabClient> {
        self.registry
            .client_for_repo(repo)
            .ok_or_else(|| anyhow::anyhow!(
                "GitLab project {} is not configured",
                repo.installation_id
            ))
    }
}
```

Delegate trait methods:

```rust
#[async_trait::async_trait]
impl CodePlatform for GitlabPlatformRouter {
    fn name(&self) -> &'static str {
        "gitlab"
    }

    fn verify_webhook(&self, _headers: &axum::http::HeaderMap, _body: &[u8]) -> bool {
        false
    }

    fn delivery_id(&self, headers: &axum::http::HeaderMap) -> Option<String> {
        headers.get("x-gitlab-event-uuid")?.to_str().ok().map(str::to_string)
    }

    fn event_type(&self, headers: &axum::http::HeaderMap) -> Option<String> {
        headers.get("x-gitlab-event")?.to_str().ok().map(str::to_string)
    }

    async fn list_changed_files(&self, repo: &RepoRef, pr_number: i64) -> anyhow::Result<Vec<ChangedFile>> {
        self.client(repo)?.list_changed_files(repo, pr_number).await
    }

    async fn default_branch(&self, repo: &RepoRef) -> anyhow::Result<String> {
        self.client(repo)?.default_branch(repo).await
    }

    async fn pr_shas(&self, repo: &RepoRef, pr_number: i64) -> anyhow::Result<(Option<String>, Option<String>)> {
        self.client(repo)?.pr_shas(repo, pr_number).await
    }

    async fn post_review(&self, repo: &RepoRef, review: &ReviewPost) -> anyhow::Result<PostedReview> {
        self.client(repo)?.post_review(repo, review).await
    }

    fn clone_url(&self, repo: &RepoRef) -> String {
        self.registry
            .client_for_repo(repo)
            .map(|client| client.clone_url(repo))
            .unwrap_or_default()
    }

    // Delegate post_comment, add_reaction, add_labels, list_review_comments,
    // and list_comment_reactions the same way.
}
```

`verify_webhook()` intentionally returns `false` on the router because GitLab webhook verification needs
the payload's `project.id` to choose the secret. The webhook handler should call a registry-specific
method instead of this trait method.

### Phase 6 — Make GitLab webhook verification project-aware

Update `services/control-plane/src/http/webhook.rs`.

Current flow verifies before parsing JSON. GitLab per-project secrets require the reverse order for
GitLab only: parse the body enough to find `project.id`, then verify the secret.

Target flow:

```rust
let payload: serde_json::Value = match serde_json::from_slice(&body) {
    Ok(payload) => payload,
    Err(error) => {
        tracing::error!(%error, %platform, "webhook payload is not valid JSON");
        return (StatusCode::BAD_REQUEST, "invalid json payload").into_response();
    }
};

let valid = match platform {
    Platform::GitHub => verify_signature(
        state.github_webhook_secret.as_bytes(),
        &body,
        &header(&headers, "x-hub-signature-256"),
    ),
    Platform::GitLab => verify_gitlab_project_webhook(&state, &headers, &body, &payload),
};

if !valid {
    crate::http::metrics::webhook_signature_failure(&platform.to_string());
    tracing::warn!(%platform, "invalid webhook signature");
    return (StatusCode::UNAUTHORIZED, "invalid signature").into_response();
}
```

Add helper:

```rust
fn verify_gitlab_project_webhook(
    state: &AppState,
    headers: &HeaderMap,
    body: &[u8],
    payload: &serde_json::Value,
) -> bool {
    let Some(project_id) = payload["project"]["id"].as_i64() else {
        tracing::warn!("GitLab webhook missing project.id");
        return false;
    };

    let Some(registry) = state.gitlab.as_ref() else {
        tracing::warn!(project_id, "GitLab webhook received but GitLab is not configured");
        return false;
    };

    let Some(project) = registry.get(project_id) else {
        tracing::warn!(project_id, "GitLab webhook for unconfigured project");
        return false;
    };

    project.client.verify_webhook(headers, body)
}
```

Security rule:

- Before verification, the code may parse `project.id` only to select the configured project secret.
- It must not record delivery, create tasks, post metrics as accepted, or use any other payload fields
  until after verification succeeds.

### Phase 7 — Route GitLab webhook handlers through registry

Replace direct `state.gitlab.as_ref()` usage as a concrete `GitlabClient`.

Current pattern:

```rust
if let Some(gitlab) = state.gitlab.as_ref() {
    match gitlab.pr_shas(&repo_ref, mr_iid).await {
        // ...
    }
}
```

Target:

```rust
let Some(registry) = state.gitlab.as_ref() else {
    tracing::warn!(delivery_id, "GitLab registry not configured");
    return;
};

let Some(client) = registry.client_for_project(project_id) else {
    tracing::warn!(delivery_id, project_id, "GitLab project not configured");
    return;
};

match client.pr_shas(&repo_ref, mr_iid).await {
    Ok((api_base, api_head)) => {
        if base_sha.is_none() {
            base_sha = api_base;
        }
        if head_sha.is_none() {
            head_sha = api_head;
        }
    }
    Err(error) => {
        tracing::warn!(%error, delivery_id, mr = mr_iid, "GitLab MR SHA fetch failed");
    }
}
```

For note mentions, replace:

```rust
if !mentions_handle(body, &state.gitlab_app_handle) {
    return;
}
```

with:

```rust
let Some(project_id) = payload["project"]["id"].as_i64() else {
    tracing::warn!(delivery_id, "GitLab note payload missing project.id");
    return;
};

let Some(handle) = state
    .gitlab
    .as_ref()
    .and_then(|registry| registry.bot_handle(project_id))
else {
    tracing::warn!(delivery_id, project_id, "GitLab bot handle not configured");
    return;
};

if !mentions_handle(body, handle) {
    return;
}
```

### Phase 8 — Route runner clone URLs by project

Update `services/control-plane/src/http/internal.rs`.

Current:

```rust
Platform::GitLab => {
    let Some(gitlab) = state.gitlab.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "gitlab client not configured",
        )
            .into_response();
    };
    (gitlab.clone_url(&repo_ref), String::new())
}
```

Target:

```rust
Platform::GitLab => {
    let Some(registry) = state.gitlab.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "gitlab not configured").into_response();
    };

    let Some(client) = registry.client_for_repo(&repo_ref) else {
        tracing::warn!(
            task_id = %id,
            project_id = context.installation_id,
            "GitLab project config missing for task context"
        );
        return (StatusCode::SERVICE_UNAVAILABLE, "gitlab project not configured").into_response();
    };

    (client.clone_url(&repo_ref), String::new())
}
```

This guarantees a task for project A cannot accidentally receive project B's clone credentials.

### Phase 9 — Update review rendering bot handle lookup

Find platform-specific bot-handle selection in `services/control-plane/src/http/internal.rs`.

Current seam:

```rust
crate::integrations::platform::Platform::GitLab => state.gitlab_app_handle.as_str(),
```

Target: use the task context's GitLab project ID.

```rust
crate::integrations::platform::Platform::GitLab => state
    .gitlab
    .as_ref()
    .and_then(|registry| registry.bot_handle(context.installation_id))
    .unwrap_or("lightbridge-bot"),
```

Stricter option: fail closed if the project is missing instead of falling back:

```rust
let gitlab_handle = match context.platform {
    Platform::GitLab => {
        let Some(handle) = state
            .gitlab
            .as_ref()
            .and_then(|registry| registry.bot_handle(context.installation_id))
        else {
            tracing::error!(
                task_id = %id,
                project_id = context.installation_id,
                "GitLab bot handle missing for finalize"
            );
            return (StatusCode::SERVICE_UNAVAILABLE, "gitlab project not configured").into_response();
        };
        handle
    }
    Platform::GitHub => state.app_handle.as_str(),
};
```

Prefer the stricter option so missing file config does not silently render the wrong mention handle.

### Phase 10 — Remove GitLab env docs and update deployment docs

Update documentation:

- `docs/kubernetes-deployment.md`
- `services/control-plane/README.md`
- `docs/gitlab-multi-project-config-report.md` if it remains linked from tickets
- any ticket/docs that describe `GITLAB_API_TOKEN`, `GITLAB_API_URL`, `GITLAB_WEBHOOK_SECRET`, or
  `GITLAB_BOT_HANDLE` as active configuration

Replace env table:

```md
| `GITLAB_API_TOKEN` | ... |
| `GITLAB_API_URL` | ... |
| `GITLAB_WEBHOOK_SECRET` | ... |
| `GITLAB_BOT_HANDLE` | ... |
```

with file-only documentation:

````md
GitLab is configured only through `control-plane.json`:

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
      }
    ]
  }
}
```
````

Document explicitly:

> `GITLAB_API_TOKEN`, `GITLAB_API_URL`, `GITLAB_WEBHOOK_SECRET`, and `GITLAB_BOT_HANDLE` are no longer
> read by the control plane. Configure GitLab only in `control-plane.json`.

### Phase 11 — Remove or update tests that depend on GitLab env vars

Search:

```bash
rg -n "GITLAB_|gitlab_webhook_secret|gitlab_app_handle|GitlabClient::from_env" services/control-plane docs
```

Expected after implementation:

- No production code reads `GITLAB_*`.
- No docs describe `GITLAB_*` as active config.
- Tests may mention old env names only in migration/removal assertions, if needed.

Add tests:

```rust
#[test]
fn gitlab_config_rejects_enabled_without_projects() {
    let json = r#"{"gitlab":{"enabled":true,"projects":[]}}"#;
    let config: FileConfig = serde_json::from_str(json).unwrap();
    let err = config.gitlab.validate().unwrap_err();
    assert!(err.to_string().contains("requires at least one"));
}
```

```rust
#[test]
fn gitlab_config_rejects_duplicate_project_ids() {
    let json = r#"{
      "gitlab": {
        "enabled": true,
        "projects": [
          {
            "project_id": 1,
            "access_token": "token-a",
            "webhook_secret": "secret-a"
          },
          {
            "project_id": 1,
            "access_token": "token-b",
            "webhook_secret": "secret-b"
          }
        ]
      }
    }"#;
    let config: FileConfig = serde_json::from_str(json).unwrap();
    let err = config.gitlab.validate().unwrap_err();
    assert!(err.to_string().contains("duplicate"));
}
```

```rust
#[tokio::test]
async fn gitlab_webhook_rejects_wrong_project_secret() {
    // Configure project 1001 with secret-a and project 1002 with secret-b.
    // Send a project 1001 payload with X-Gitlab-Token: secret-b.
    // Assert 401 and no task creation.
}
```

## 6. Files To Change

| File | Required change |
|---|---|
| `services/control-plane/src/config.rs` | Add `GitlabSection`, `GitlabProjectConfig`, defaults, validation tests. |
| `services/control-plane/src/integrations/gitlab.rs` | Remove `from_env()`, add `new()`, `GitlabRegistry`, `GitlabPlatformRouter`, update module docs. |
| `services/control-plane/src/main.rs` | Remove `gitlab_webhook_secret`, `gitlab_app_handle`, and `GitlabClient::from_env()`; build registry from file config. |
| `services/control-plane/src/http/webhook.rs` | Verify GitLab webhook by `payload.project.id`; route API fallback and mention handle through registry. |
| `services/control-plane/src/http/internal.rs` | Use registry for GitLab clone URLs and GitLab bot handle rendering. |
| `services/control-plane/src/queue/reconciler.rs` | Keep existing platform dispatch if using `GitlabPlatformRouter`; otherwise resolve project client per row. |
| `docs/kubernetes-deployment.md` | Replace GitLab env table with file-only config docs. |
| `services/control-plane/README.md` | Update control-plane configuration summary. |
| `docs/tickets/gitlab-multi-project-config.md` | Align ticket text if it still mentions legacy fallback. |

## 7. Acceptance Criteria

- [ ] `GitlabClient::from_env()` is removed.
- [ ] `services/control-plane/src/main.rs` no longer reads `GITLAB_API_TOKEN`,
      `GITLAB_API_URL`, `GITLAB_WEBHOOK_SECRET`, or `GITLAB_BOT_HANDLE`.
- [ ] `AppState` no longer stores `gitlab_webhook_secret` or `gitlab_app_handle`.
- [ ] GitLab is configured only through `FileConfig.gitlab`.
- [ ] `gitlab.enabled=true` requires at least one valid `gitlab.projects[]` entry.
- [ ] Duplicate GitLab `project_id` values fail startup.
- [ ] Empty GitLab access tokens and webhook secrets fail startup.
- [ ] GitLab webhook verification selects the expected secret by `payload.project.id`.
- [ ] GitLab API calls and clone URLs select the project client by `installation_id = project_id`.
- [ ] GitLab reconciler egress selects the project client by `RepoRef.installation_id`.
- [ ] Missing GitLab project config fails closed with `503` or `401`, depending on path.
- [ ] GitHub behavior remains unchanged.
- [ ] Documentation says GitLab is file-only and no longer documents `GITLAB_*` env vars as active
      config.

## 8. Test Plan

Run focused tests first:

```bash
cargo test -p control-plane gitlab
cargo test -p control-plane webhook
cargo test -p control-plane get_context
cargo test -p control-plane reconciler
```

Then broader checks:

```bash
cargo test -p control-plane
cargo clippy -p control-plane --all-targets -- -D warnings
```

Search checks:

```bash
rg -n "GITLAB_API_TOKEN|GITLAB_API_URL|GITLAB_WEBHOOK_SECRET|GITLAB_BOT_HANDLE|GitlabClient::from_env|gitlab_webhook_secret|gitlab_app_handle" services/control-plane docs
```

Expected:

- No production code contains direct GitLab env configuration.
- Documentation references old env names only to say they are no longer supported, if at all.

Manual verification:

- [ ] Start control plane with `gitlab.enabled=false`; GitLab is disabled and GitHub still works.
- [ ] Start control plane with `gitlab.enabled=true` and no projects; startup fails.
- [ ] Start control plane with two valid GitLab projects; startup succeeds.
- [ ] Send project A webhook with project A secret; accepted.
- [ ] Send project A webhook with project B secret; rejected.
- [ ] Fetch task context for project A; clone URL uses project A token and logs redact it.
- [ ] Reconciler posts for project A using project A client.

## 9. Rollout Notes

Because this intentionally removes GitLab env fallback, deploy ordering matters:

1. Prepare the new `control-plane.json` GitLab section in the deployment secret/config source.
2. Deploy the new control-plane binary that understands `gitlab.projects[]`.
3. Remove `GITLAB_*` env vars from deployment manifests.
4. Verify startup logs show configured GitLab projects by ID/count only, never tokens.
5. Verify GitLab webhooks use `/webhook` with per-project secret tokens.

Do not deploy the new binary into an environment that only has the old `GITLAB_*` env vars; GitLab
will be disabled or startup will fail depending on `gitlab.enabled`.

## 10. Final Shape

After this refactor, GitLab configuration has one authoritative source:

```text
/etc/lightbridge/control-plane.json
```

The control plane should not have any active GitLab-specific env configuration path. That makes
multi-project behavior explicit, reviewable, and testable:

- webhook secret selection is by `project.id`;
- clone/API token selection is by `installation_id = project_id`;
- reconciler egress selection is by `RepoRef.installation_id`;
- missing project config fails closed instead of falling back to stale process env.
