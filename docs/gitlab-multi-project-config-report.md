# GitLab Multi-Project Configuration Report

## Executive Summary

The control plane already has a solid platform abstraction for GitHub and GitLab, but the current
GitLab implementation is still configured as **one GitLab API token + one webhook secret + one bot
handle for the whole process**.

That means it can receive webhooks from multiple GitLab projects only if every project uses the same
`GITLAB_WEBHOOK_SECRET`, and it can call GitLab APIs or build clone URLs only with the same
`GITLAB_API_TOKEN`. To support multiple GitLab projects with distinct access tokens and webhook
tokens, the smallest safe refactor is to add a GitLab project registry to the existing
`control-plane.json` file config, then route GitLab webhook verification and GitLab API/clone calls
through that registry by `project.id`.

Recommended direction:

1. Add a `gitlab.projects[]` section to `services/control-plane/src/config.rs`.
2. Replace the single `AppState.gitlab` / `gitlab_webhook_secret` lookup path with a
   project-aware `GitlabRegistry`.
3. Keep the database schema mostly unchanged: GitLab already stores `project.id` as
   `repositories.platform_repo_id` and `tasks.installation_id`.
4. Refactor only GitLab-specific call sites in `main.rs`, `http/webhook.rs`, `http/internal.rs`,
   `queue/reconciler.rs`, and `integrations/gitlab.rs`.

## Current GitLab Implementation

### Existing platform abstraction

GitLab support is not a side route; it is already integrated through the platform abstraction:

- `services/control-plane/src/integrations/platform.rs` defines `Platform::{GitHub, GitLab}`,
  `RepoRef`, and the `CodePlatform` trait.
- `services/control-plane/src/integrations/gitlab.rs` implements `CodePlatform` for `GitlabClient`.
- `services/control-plane/src/http/webhook.rs` uses one `/webhook` route and detects GitHub vs GitLab
  from headers.
- `services/control-plane/src/queue/reconciler.rs` dispatches outbound writes by `row.platform`.
- `services/control-plane/src/http/internal.rs` creates the platform-aware clone context for the
  agent runner.

The important thing: we do not need to redesign task processing. GitLab is already first-class enough
that this change can stay narrow.

### Current GitLab configuration is single-tenant

`AppState` currently stores one GitLab client and one webhook secret:

```rust
// services/control-plane/src/main.rs
pub struct AppState {
    pub gitlab_webhook_secret: Arc<String>,
    pub gitlab: Option<gitlab::GitlabClient>,
    pub gitlab_app_handle: Arc<String>,
}
```

Startup reads the GitLab values directly from env:

```rust
// services/control-plane/src/main.rs
let gitlab = gitlab::GitlabClient::from_env();

gitlab_webhook_secret: Arc::new(
    std::env::var("GITLAB_WEBHOOK_SECRET").unwrap_or_default(),
),

gitlab_app_handle: Arc::new(
    std::env::var("GITLAB_BOT_HANDLE")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| "lightbridge-bot".to_string()),
),
```

The `GitlabClient` also reads env directly:

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

    Some(Self {
        api_url,
        token,
        webhook_secret,
        http,
    })
}
```

This is the main limitation: one process-wide API token and one process-wide webhook secret.

### Existing JSON config pattern to reuse

The project already uses `/etc/lightbridge/control-plane.json` for structured config:

```rust
// services/control-plane/src/config.rs
pub struct FileConfig {
    pub agent: AgentSection,
    pub dispatcher: DispatcherSection,
    pub review: ReviewSection,
    pub embeddings: EmbeddingsSection,
    pub knowledge_tools: KnowledgeToolsSection,
    pub egress: EgressSection,
}
```

It is loaded once at startup:

```rust
// services/control-plane/src/config.rs
pub fn load_file_config() -> anyhow::Result<Option<FileConfig>> {
    let path =
        std::env::var("CONTROL_PLANE_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_string());
    let path = Path::new(&path);
    if !path.exists() {
        return Ok(None);
    }
    lci_config::load::<FileConfig>(path).map(Some)
}
```

This is exactly the pattern we should use for multiple GitLab projects. It already supports
`{env:VAR:-default}` substitution, so secrets can remain in Kubernetes Secrets / ExternalSecrets
while the JSON file holds the shape.

Example pattern already used:

```json
{
  "knowledge_tools": {
    "mcp_servers": [
      {
        "name": "context7",
        "url": "http://context7.converse-mcp.svc.cluster.local:8080/mcp"
      }
    ]
  }
}
```

The same style can support:

```json
{
  "gitlab": {
    "projects": [
      {
        "project_id": 123456,
        "api_url": "https://gitlab.com/api/v4",
        "access_token": "{env:GITLAB_PROJECT_123456_TOKEN}",
        "webhook_secret": "{env:GITLAB_PROJECT_123456_WEBHOOK_SECRET}",
        "bot_handle": "lightbridge-bot"
      }
    ]
  }
}
```

## Current Webhook Flow

The unified webhook route detects GitLab by `X-Gitlab-Event`:

```rust
// services/control-plane/src/http/webhook.rs
fn detect_platform(headers: &HeaderMap) -> Option<Platform> {
    if headers.contains_key("x-github-event") {
        Some(Platform::GitHub)
    } else if headers.contains_key("x-gitlab-event") {
        Some(Platform::GitLab)
    } else {
        None
    }
}
```

GitLab verification currently uses the single `AppState.gitlab_webhook_secret`:

```rust
// services/control-plane/src/http/webhook.rs
let valid = match platform {
    Platform::GitHub => verify_signature(
        state.github_webhook_secret.as_bytes(),
        &body,
        &header(&headers, "x-hub-signature-256"),
    ),
    Platform::GitLab => verify_gitlab_token(
        &state.gitlab_webhook_secret,
        &header(&headers, "x-gitlab-token"),
    ),
};
```

After verification, GitLab handlers parse the project from the JSON body:

```rust
// services/control-plane/src/http/webhook.rs
fn gitlab_project_identity(project: &serde_json::Value) -> Option<(i64, &str, &str, &str)> {
    let project_id = project["id"].as_i64()?;
    let path = project["path_with_namespace"].as_str()?;
    let default_branch = project["default_branch"].as_str().unwrap_or("main");
    let (owner, name) = gitlab_path_split(path)?;
    Some((project_id, owner, name, default_branch))
}
```

That ordering is the core problem for per-project webhook tokens: the current code verifies before it
parses the body, but the project ID needed to choose the right webhook token is inside the body.

## Current Token / Clone Flow

When the agent runner asks for task context, GitLab clone URLs are built from the one process-wide
token:

```rust
// services/control-plane/src/http/internal.rs
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

`clone_url()` embeds that token directly:

```rust
// services/control-plane/src/integrations/gitlab.rs
fn clone_url(&self, repo: &RepoRef) -> String {
    let base = self
        .api_url
        .strip_suffix("/api/v4")
        .unwrap_or(&self.api_url);
    let host = base.split_once("://").map(|(_, h)| h).unwrap_or(base);
    format!(
        "https://oauth2:{}@{}/{}.git",
        self.token, host, repo.full_name
    )
}
```

For multiple GitLab projects, this must select the client/token by `RepoRef.installation_id`. In this
codebase GitLab already uses `installation_id = project_id`, so this is a clean seam.

## Current Database Fit

The database is already platform-aware because of migration `0024_platform_abstraction.sql`:

```sql
ALTER TABLE repositories ADD COLUMN IF NOT EXISTS platform TEXT NOT NULL DEFAULT 'github';
ALTER TABLE repositories RENAME COLUMN github_repo_id TO platform_repo_id;

CREATE UNIQUE INDEX IF NOT EXISTS repositories_platform_repo_id_unique
    ON repositories (platform, platform_repo_id);

ALTER TABLE tasks RENAME COLUMN github_delivery_id TO webhook_delivery_id;
ALTER TABLE github_deliveries RENAME TO webhook_deliveries;
ALTER TABLE webhook_deliveries ADD COLUMN IF NOT EXISTS platform TEXT NOT NULL DEFAULT 'github';
```

GitLab handlers already store:

```rust
// services/control-plane/src/http/webhook.rs
let installation_id = project_id;

crate::db::upsert_repository(
    pool,
    Platform::GitLab,
    project_id,
    owner,
    name,
    default_branch,
    Some(installation_id),
)
.await
```

So for per-project GitLab configuration, the database can mostly stay as-is. The existing
`repositories.platform_repo_id` and `tasks.installation_id` are enough to select the configured
GitLab project.

## Recommended Configuration Shape

Add a new top-level `gitlab` section to `control-plane.json`:

```json
{
  "gitlab": {
    "default_api_url": "https://gitlab.com/api/v4",
    "default_bot_handle": "lightbridge-bot",
    "projects": [
      {
        "project_id": 123456,
        "api_url": "https://gitlab.com/api/v4",
        "access_token": "{env:GITLAB_SERVICE_ONE_TOKEN}",
        "webhook_secret": "{env:GITLAB_SERVICE_ONE_WEBHOOK_SECRET}",
        "bot_handle": "lightbridge-bot"
      },
      {
        "project_id": 789012,
        "api_url": "https://gitlab.example.com/api/v4",
        "access_token": "{env:GITLAB_SERVICE_TWO_TOKEN}",
        "webhook_secret": "{env:GITLAB_SERVICE_TWO_WEBHOOK_SECRET}",
        "bot_handle": "lightbridge-reviewer"
      }
    ]
  }
}
```

Field semantics:

| Field | Required | Purpose |
|---|---:|---|
| `project_id` | yes | GitLab numeric project ID; used as lookup key. |
| `api_url` | no | Per-project GitLab API base URL; defaults to `default_api_url`. |
| `access_token` | yes | Project/group/PAT token sent as `PRIVATE-TOKEN`; supplied by the mounted file/Secret. |
| `webhook_secret` | yes | Plain GitLab webhook token expected in `X-Gitlab-Token`; supplied by the mounted file/Secret. |
| `bot_handle` | no | Per-project mention handle; defaults to `default_bot_handle`. |

There is intentionally no legacy `GITLAB_*` env fallback. GitLab is configured only through
`control-plane.json`, preferably mounted from a Kubernetes Secret or ExternalSecret-managed Secret.

## Minimal Refactor Plan

### 1. Extend file config

Add a GitLab section:

```rust
// services/control-plane/src/config.rs
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

Validation should reject:

- duplicate `project_id`
- empty `access_token`
- empty `webhook_secret`
- invalid header bytes in `access_token` or `webhook_secret`
- `api_url` values without `/api/v4` only if the current implementation still assumes that suffix for
  clone URL generation

### 2. Split `GitlabClient` construction from env and remove env construction

Keep `GitlabClient`, make it constructible from config, and remove `from_env()` entirely:

```rust
// services/control-plane/src/integrations/gitlab.rs
impl GitlabClient {
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
}
```

No `GITLAB_API_TOKEN`, `GITLAB_API_URL`, `GITLAB_WEBHOOK_SECRET`, or `GITLAB_BOT_HANDLE` process env
vars should be read by the GitLab integration.

### 3. Add a project-aware registry

Create a small registry instead of changing the whole platform trait:

```rust
// services/control-plane/src/integrations/gitlab.rs
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

    pub fn verify_project_webhook(
        &self,
        project_id: i64,
        headers: &axum::http::HeaderMap,
        body: &[u8],
    ) -> bool {
        self.get(project_id)
            .map(|project| project.client.verify_webhook(headers, body))
            .unwrap_or(false)
    }

    pub fn bot_handle(&self, project_id: i64) -> Option<&str> {
        self.get(project_id).map(|project| project.bot_handle.as_str())
    }
}
```

This preserves the existing `CodePlatform` implementation for each concrete client, but gives the
application a project-aware lookup layer.

### 4. Refactor `AppState`

Replace the single GitLab fields:

```rust
pub gitlab_webhook_secret: Arc<String>,
pub gitlab: Option<gitlab::GitlabClient>,
pub gitlab_app_handle: Arc<String>,
```

with:

```rust
pub gitlab: Option<gitlab::GitlabRegistry>,
```

Remove `gitlab_app_handle`; GitLab mention handles come from the project registry.

Build the registry from file config:

```rust
// services/control-plane/src/main.rs
let gitlab_config = file_config
    .as_ref()
    .map(|config| config.gitlab.clone())
    .unwrap_or_default();
let gitlab = gitlab::GitlabRegistry::from_config(&gitlab_config)?;
```

### 5. Change GitLab webhook verification order only

For GitHub, keep the current order: verify HMAC before parsing JSON.

For GitLab, parse the body enough to extract `project.id`, choose the configured project, then verify
`X-Gitlab-Token` against that project's secret:

```rust
// services/control-plane/src/http/webhook.rs
let payload: serde_json::Value = serde_json::from_slice(&body)
    .map_err(|_| (StatusCode::BAD_REQUEST, "invalid json payload"))?;

let valid = match platform {
    Platform::GitHub => verify_signature(
        state.github_webhook_secret.as_bytes(),
        &body,
        &header(&headers, "x-hub-signature-256"),
    ),
    Platform::GitLab => {
        let project_id = payload["project"]["id"].as_i64();
        match (project_id, state.gitlab.as_ref()) {
            (Some(project_id), Some(registry)) => {
                registry.verify_project_webhook(project_id, &headers, &body)
            }
            _ => false,
        }
    }
};
```

Security note: parsing unsigned JSON before verification is acceptable here only for reading
`project.id` and selecting the candidate secret. The handler must not trust or act on the payload
until after verification. This keeps fail-closed behavior while enabling per-project secrets.

```rust
let Some(project) = state.gitlab.as_ref().and_then(|r| r.get(project_id)) else {
    return (StatusCode::UNAUTHORIZED, "unknown project").into_response();
};
if !project.client.verify_webhook(headers, body) {
    return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
}
```

### 6. Route GitLab API fallback calls by project

GitLab webhook handlers currently call `state.gitlab.as_ref()` directly when they need MR SHAs. Change
those to resolve by `project_id`:

```rust
let Some(registry) = state.gitlab.as_ref() else {
    tracing::warn!(delivery_id, "GitLab registry not configured");
    return;
};
let Some(project) = registry.get(project_id) else {
    tracing::warn!(delivery_id, project_id, "GitLab project not configured");
    return;
};

match project.client.pr_shas(&repo_ref, mr_iid).await {
    Ok((api_base, api_head)) => { /* unchanged */ }
    Err(error) => { /* unchanged */ }
}
```

### 7. Route clone URL creation by project

In `http/internal.rs`, resolve the client with `context.installation_id`:

```rust
Platform::GitLab => {
    let Some(registry) = state.gitlab.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "gitlab not configured").into_response();
    };
    let Some(client) = registry.client_for_repo(&repo_ref) else {
        tracing::warn!(
            task_id = %id,
            project_id = context.installation_id,
            "no GitLab config for task project"
        );
        return (StatusCode::SERVICE_UNAVAILABLE, "gitlab project not configured").into_response();
    };
    (client.clone_url(&repo_ref), String::new())
}
```

This is the most important runtime change: without it, the runner would still clone every GitLab repo
using the old single token.

### 8. Route reconciler calls by project

The reconciler currently gets one `Arc<dyn CodePlatform>` for `Platform::GitLab`. With multiple GitLab
clients, use a registry-backed adapter:

```rust
pub struct GitlabPlatformRouter {
    registry: GitlabRegistry,
}

#[async_trait::async_trait]
impl CodePlatform for GitlabPlatformRouter {
    fn name(&self) -> &'static str {
        "gitlab"
    }

    async fn post_review(&self, repo: &RepoRef, review: &ReviewPost) -> anyhow::Result<PostedReview> {
        let client = self.registry.client_for_repo(repo)
            .ok_or_else(|| anyhow::anyhow!("GitLab project {} not configured", repo.installation_id))?;
        client.post_review(repo, review).await
    }

    fn clone_url(&self, repo: &RepoRef) -> String {
        self.registry.client_for_repo(repo)
            .map(|client| client.clone_url(repo))
            .unwrap_or_default()
    }

    // Delegate every other CodePlatform method similarly.
}
```

Then `AppState.platforms` can still remain:

```rust
HashMap<Platform, Arc<dyn CodePlatform>>
```

That avoids touching the outbox model and keeps the reconciler architecture unchanged.

## Files To Touch

### Must touch

| File | Change |
|---|---|
| `services/control-plane/src/config.rs` | Add `GitlabSection` and `GitlabProjectConfig`. |
| `services/control-plane/src/integrations/gitlab.rs` | Add `GitlabClient::new`, `GitlabRegistry`, and optional `GitlabPlatformRouter`. |
| `services/control-plane/src/main.rs` | Build registry from file config; update readiness; update `platforms` registration. |
| `services/control-plane/src/http/webhook.rs` | For GitLab only: parse project ID before verification; use project webhook secret and project bot handle. |
| `services/control-plane/src/http/internal.rs` | Resolve GitLab clone URL by project ID / `installation_id`. |
| `services/control-plane/src/queue/reconciler.rs` | Either no change if using `GitlabPlatformRouter`, or small client lookup changes if not. |
| `docs/kubernetes-deployment.md` | Document `gitlab.projects[]` and deprecate single GitLab env config. |

### Probably do not touch

| Area | Reason |
|---|---|
| Database schema | Existing `platform_repo_id` and GitLab `installation_id = project_id` are sufficient. |
| Agent runner | It already accepts pre-authenticated GitLab clone URLs. |
| GitHub integration | GitHub App installation-token model is unrelated. |
| Review shaping | Platform-aware bot handle rendering already exists; only lookup source changes. |
| Outbox payload format | Existing outbox rows carry `installation_id`, owner/repo, and platform. |

## Multiple GitLab Project Setup

For each GitLab project:

1. Create a project access token, group access token, or bot PAT with enough permissions to:
   - read repository
   - read merge requests
   - read/write notes/discussions
   - read/write labels if label feedback is enabled
   - read/write award emoji if reactions are enabled
2. Configure a GitLab project webhook pointing to:

```text
https://<control-plane-public-url>/webhook
```

3. Enable at least:
   - Merge request events
   - Push events
   - Comments / note events
4. Set the webhook secret token to the per-project `webhook_secret`.
5. Add the project to `control-plane.json`.
6. Approve the repository through the existing admin approval flow before reviews or indexing run.

Example:

```json
{
  "gitlab": {
    "default_api_url": "https://gitlab.com/api/v4",
    "default_bot_handle": "lightbridge-bot",
    "projects": [
      {
        "project_id": 1001,
        "access_token": "{env:GITLAB_SERVICE_A_TOKEN}",
        "webhook_secret": "{env:GITLAB_SERVICE_A_WEBHOOK_SECRET}"
      },
      {
        "project_id": 1002,
        "api_url": "https://gitlab.internal.example/api/v4",
        "access_token": "{env:GITLAB_SERVICE_B_TOKEN}",
        "webhook_secret": "{env:GITLAB_SERVICE_B_WEBHOOK_SECRET}",
        "bot_handle": "lb-reviewer"
      }
    ]
  }
}
```

## Compatibility Strategy

There is no GitLab env compatibility path. Rollout must install the new `gitlab.projects[]`
configuration before relying on GitLab traffic:

1. Render `gitlab.projects[]` into the mounted `control-plane.json` Secret.
2. Deploy the binary that understands the new section.
3. Remove old `GITLAB_*` env vars from manifests.
4. Verify startup logs show only configured project counts/IDs, never token values.

If only old `GITLAB_*` env vars are present, GitLab is not configured.

## Main Design Choice

There are two possible approaches:

### Option A — per-project registry, no DB secret storage

Store GitLab project credentials only in mounted config/env-backed secrets.

Pros:

- Minimal database change.
- Keeps secrets out of Postgres.
- Matches the current config style.
- Easy to operate with ExternalSecrets.

Cons:

- Adding a project requires a deploy/restart.
- Project approval and project credential configuration remain separate steps.

This is the recommended option.

### Option B — store GitLab credentials in DB

Add a `gitlab_project_credentials` table and manage project tokens through admin APIs.

Pros:

- Dynamic project onboarding without deploy.
- Admin UI could eventually manage GitLab credentials.

Cons:

- Requires encryption at rest, key rotation, audit, and new admin surfaces.
- Much larger trust-boundary change.
- More difficult to verify safely.

This is not recommended for the current goal.

## Risks And Mitigations

| Risk | Mitigation |
|---|---|
| Unsigned GitLab JSON must be parsed before token verification | Parse only `project.id`, do not act on payload until verification succeeds. |
| Project ID spoofing in body | Verify token selected by `project.id`; an attacker still needs that project's webhook secret. |
| Token leakage through logs | Keep existing redaction; never log `clone_url` or token values. |
| Wrong token used for clone/write-back | Resolve every GitLab client by `RepoRef.installation_id` / task `installation_id`. |
| Duplicate project IDs in config | Fail startup on duplicates. |
| Missing project config for an approved repo | Return 503 and log `project_id`; do not fall back to another project token. |
| `deny_unknown_fields` deploy ordering | Ship binary first, chart second, values last. |

## Suggested Tests

Add unit tests for:

- `GitlabRegistry` rejects duplicate project IDs.
- `GitlabRegistry` rejects empty tokens/secrets.
- `webhook_router` accepts project A with secret A and rejects secret B.
- `get_context` builds a GitLab clone URL with the token for the task's project ID.
- `GitlabPlatformRouter` delegates `post_review` to the client for `repo.installation_id`.

Add integration-style tests around existing webhook fixtures if available:

```rust
#[tokio::test]
async fn gitlab_webhook_uses_project_specific_secret() {
    // configure project 1001 secret "a" and project 1002 secret "b"
    // send project 1001 payload with X-Gitlab-Token: a => 202
    // send project 1001 payload with X-Gitlab-Token: b => 401
}
```

## Final Recommendation

Implement `gitlab.projects[]` in `control-plane.json` and introduce a `GitlabRegistry` keyed by
GitLab `project.id`. Do not refactor the whole platform abstraction or database schema. The codebase
already chose a useful convention — GitLab `installation_id = project_id` — so use that as the routing
key everywhere:

- webhook verification: `payload.project.id`
- task creation: existing `installation_id = project_id`
- runner clone URL: `context.installation_id`
- reconciler write-back: `RepoRef.installation_id`

This gives multiple GitLab projects distinct access tokens and webhook secrets while keeping the
change surgical and aligned with the existing architecture.
