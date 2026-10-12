# Frontend Platform Links — Report & Implementation Plan

> Repo links, PR/MR links, trigger labels, and the GitLab base-URL source. Covers the current
> `frontend/gitlab-repo-url` state, the gaps found, and a detailed plan that **removes the
> `GITLAB_URL` env var** and sources the GitLab base URL from the **control plane** instead.

---

## 1. Executive summary

The branch already made the dashboard **platform-aware** for GitHub vs GitLab at the data level:
`repoUrl`, `targetUrl`, and `triggerLabel` in `apps/web/lib/domain/tasks.ts` branch on
`task.repo_platform`, and `apps/web/lib/domain/repos.ts` does the same for `Repository`. The GitLab
base URL is currently read from the **`GITLAB_URL` environment variable** via
`gitlabBaseUrl()` in `apps/web/lib/utils/config.ts`.

**What's missing / wrong:**
1. Two link labels are **hardcoded "View on GitHub"** even for GitLab repos (`repo-list.tsx:96`,
   `review-output.tsx:65`).
2. The GitLab base URL is **env-coupled** to the web app — it must instead come from the control
   plane (which already knows it via `GitlabSection.default_api_url` / `GitlabRegistry.api_url`).
3. A **`target_type` mapping risk**: `targetUrl` assumes GitLab MRs arrive as `target_type:
   "pull_request"`; this must be verified against the webhook task creation.
4. `review-output.tsx` has no platform context to label its permalink correctly.

**Plan:** add a small `GET /config` endpoint to the control plane that returns the GitLab web base
URL, fetch it in the web server layer, thread it through the pages (exactly as today), fix the two
hardcoded labels, and delete the `GITLAB_URL` env usage.

---

## 2. Current state (what the branch implemented)

### 2.1 Task domain — `apps/web/lib/domain/tasks.ts`

```ts
// tasks.ts:27 — new field on the Task payload
repo_platform: "github" | "gitlab" | null;

// tasks.ts:105 — repo deep link, branches on platform
export function repoUrl(task: Task, gitlabBaseUrl: string): string | null {
  if (!task.repo_owner || !task.repo_name) return null;
  switch (task.repo_platform) {
    case "gitlab":  return `${gitlabBaseUrl}/${task.repo_owner}/${task.repo_name}`;
    default:        return `https://github.com/${task.repo_owner}/${task.repo_name}`;
  }
}

// tasks.ts:117 — PR/MR/issue deep link
export function targetUrl(task: Task, gitlabBaseUrl: string): string | null {
  const base = repoUrl(task, gitlabBaseUrl);
  if (!base) return null;
  switch (task.target_type) {
    case "pull_request":
      return task.repo_platform === "gitlab"
        ? `${base}/-/merge_requests/${task.target_id}`
        : `${base}/pull/${task.target_id}`;
    case "issue":
      return task.repo_platform === "gitlab"
        ? `${base}/-/issues/${task.target_id}`
        : `${base}/issues/${task.target_id}`;
    default: return null;
  }
}

// tasks.ts:135 — "review · PR #123" (GitHub) vs "review · MR #15" (GitLab)
export function triggerLabel(task: Task): string { /* ... */ }
```

### 2.2 Repo domain — `apps/web/lib/domain/repos.ts`

```ts
// repos.ts:32
export function repoUrl(repo: Repository, gitlabBaseUrl: string): string {
  switch (repo.platform) {
    case "gitlab": return `${gitlabBaseUrl}/${repo.owner}/${repo.name}`;
    default:       return `https://github.com/${repo.owner}/${repo.name}`;
  }
}
```

### 2.3 GitLab base URL source — `apps/web/lib/utils/config.ts` (the part to remove)

```ts
// config.ts:16 — reads the env var; this is what we replace with a backend fetch
export function gitlabBaseUrl(): string {
  const url = process.env.GITLAB_URL || "https://gitlab.com";
  return url.replace(/\/+$/, "");
}
```

### 2.4 Consumers (already wired, but with the two label bugs)

- `apps/web/app/dashboard/repositories/page.tsx:45` — `gitlabBaseUrl()` passed to `<RepoList>`.
- `apps/web/components/repos/repo-list.tsx:91` — `repoUrl(repo, gitlabBaseUrl)`; **label hardcoded
  "View on GitHub" at L96**.
- `apps/web/app/dashboard/runs/[id]/page.tsx:118,120` — `repoUrl(task, gitlabBaseUrl())` and
  `targetUrl(task, gitlabBaseUrl())` (correct); `triggerLabel(task)` at L78 (correct).
- `apps/web/components/runs/review-output.tsx:58-67` — renders `review.review_url` with **hardcoded
  "View on GitHub" at L65**.

---

## 3. Gaps & risks

| # | Issue | Where | Impact |
|---|---|---|---|
| G1 | Hardcoded "View on GitHub" label | `repo-list.tsx:96` | GitLab repos show a GitHub label on a GitLab link — wrong/misleading. |
| G2 | Hardcoded "View on GitHub" label | `review-output.tsx:65` | Same, on the review permalink. |
| G3 | GitLab base URL from env (`GITLAB_URL`) | `config.ts:16` | Couples web config to a deploy env var; the control plane already owns this value. |
| G4 | `target_type` assumption | `tasks.ts:121` | `targetUrl` maps `pull_request`→MR for GitLab. If GitLab tasks store `target_type:"merge_request"`, the link returns `null`. **Must verify.** |
| G5 | `review_url` label has no platform context | `review-output.tsx` | Needs the platform to label correctly (pass it in, or infer from host). |
| G6 | `GITLAB_URL` may also be set in `compose.yaml` / chart | deploy | Removing the env usage must also drop it from deployment manifests to avoid dead config. |

---

## 4. Backend analysis — can we get the GitLab URL from the control plane?

**Yes.** The control plane already holds the GitLab base URL:

- `services/control-plane/src/config.rs:36` — `GitlabSection { enabled, default_api_url, projects }`.
- `config.rs:63` — `GitlabSection::default_api_url()` returns the configured API URL, defaulting to
  `https://gitlab.com/api/v4`.
- `services/control-plane/src/integrations/gitlab.rs:35` — `GitlabRegistry { api_url, ... }`, built
  from config via `from_config` (gitlab.rs:173). `api_url` is the resolved default (e.g.
  `https://gitlab.example.com/api/v4`).
- `services/control-plane/src/main.rs` — `AppState` already carries `gitlab: Option<GitlabRegistry>`
  (used by `webhook.rs` as `state.gitlab.as_ref()`).

The **web** base URL is the API URL with `/api/v4` (and any trailing slash) stripped:
`https://gitlab.example.com/api/v4` → `https://gitlab.example.com`.

**Gap:** there is **no HTTP endpoint** that exposes this to the frontend (grep of `http/` found only
gitlab *webhook* handling, no config/deployment read endpoint). So we add one.

---

## 5. Implementation plan

### Step A — Backend: derive the web base URL on `GitlabRegistry`

`services/control-plane/src/integrations/gitlab.rs` — add a method that strips `/api/v4`:

```rust
// gitlab.rs — inside `impl GitlabRegistry`
/// Web (non-API) base URL for building repo/MR deep links, derived from the configured API URL.
/// `https://gitlab.example.com/api/v4` -> `https://gitlab.example.com`. Falls back to SaaS.
pub fn web_base_url(&self) -> String {
    self.api_url
        .trim_end_matches('/')
        .trim_end_matches("/api/v4")
        .to_string()
}
```

(If `AppState` does not expose the raw `GitlabSection`, this method on the registry is the cleanest
source. If the config section is already in `AppState`, an equivalent `GitlabSection::web_base_url()`
helper is fine — pick one.)

### Step B — Backend: add a `GET /config` endpoint

New file `services/control-plane/src/http/config.rs`:

```rust
//! Deployment/config read endpoint for the web console (ADR-0016). Exposes non-sensitive
//! deployment settings the frontend needs to render platform-correct links — currently the GitLab
//! web base URL (sourced from the GitLab config, not an env var).

use axum::Json;
use serde::Serialize;
use crate::AppState;

#[derive(Debug, Serialize)]
pub struct DeploymentConfig {
    pub gitlab_base_url: String,
}

/// `GET /config` — non-sensitive deployment settings for the console.
pub async fn deployment_config(State(state): State<AppState>) -> Json<DeploymentConfig> {
    let gitlab_base_url = state
        .gitlab
        .as_ref()
        .map(|g| g.web_base_url())
        .unwrap_or_else(|| "https://gitlab.com".to_string());
    Json(DeploymentConfig { gitlab_base_url })
}
```

Register it in `services/control-plane/src/main.rs` (near the other public routes, ~L316):

```rust
.route("/config", get(config::deployment_config))
```

and add `mod config;` to the `http` module declarations. The endpoint reuses the existing auth
middleware pattern (or is left open — it exposes only a non-sensitive base URL; match the
`/healthz` vs authed style already in the router).

### Step C — Frontend API client: fetch the config

`apps/web/lib/server/api.ts` — add a fetch mirroring `listRepositories`:

```ts
export interface DeploymentConfig { gitlab_base_url: string }

export async function getDeploymentConfig(): Promise<ApiResult<DeploymentConfig>> {
  try {
    const res = await authedFetch("/config");
    if (!res) return { ok: false, reason: "unauthenticated" };
    if (!res.ok) return { ok: false, reason: classify(res.status), status: res.status };
    return { ok: true, data: (await res.json()) as DeploymentConfig };
  } catch {
    return { ok: false, reason: "unavailable" };
  }
}
```

### Step D — `config.ts`: drop the env read, keep a pure normalizer

`apps/web/lib/utils/config.ts` — replace `gitlabBaseUrl()` with a pure helper (no env):

```ts
/**
 * Normalize a GitLab web base URL: strip trailing slashes so `${base}/owner/repo` never
 * double-slashes. `null`/empty (backend unavailable) falls back to SaaS `https://gitlab.com`.
 * The value is fetched from the control plane (`getDeploymentConfig`), not an env var.
 */
export function normalizeGitlabBaseUrl(url: string | null | undefined): string {
  const base = (url && url.trim()) || "https://gitlab.com";
  return base.replace(/\/+$/, "");
}
```

Delete `gitlabBaseUrl()` and the `GITLAB_URL` reference.

### Step E — Pages: fetch config once, pass the string down

`apps/web/app/dashboard/repositories/page.tsx`:

```ts
import { getDeploymentConfig } from "@/lib/server/api";
import { normalizeGitlabBaseUrl } from "@/lib/utils/config";

export default async function Repositories() {
  const result = await listRepositories();
  const cfg = await getDeploymentConfig();
  const gitlabBaseUrl = normalizeGitlabBaseUrl(cfg.ok ? cfg.data.gitlab_base_url : null);
  // ...
  <RepoList repos={result.data} now={now} gitlabBaseUrl={gitlabBaseUrl} />
}
```

`apps/web/app/dashboard/runs/[id]/page.tsx` — same: fetch `getDeploymentConfig()`, compute
`gitlabBaseUrl`, and use `repoUrl(task, gitlabBaseUrl)` / `targetUrl(task, gitlabBaseUrl)` (replace
the `gitlabBaseUrl()` calls at L118/L120).

### Step F — Fix the hardcoded labels (G1, G2)

`apps/web/components/repos/repo-list.tsx` — make the link label platform-aware:

```tsx
// repo-list.tsx — RepoCard
const label = repo.platform === "gitlab" ? "View on GitLab" : "View on GitHub";
// ...
<a href={repoUrl(repo, gitlabBaseUrl)} ...>{label}<ExternalLink .../></a>
```

`apps/web/components/runs/review-output.tsx` — the permalink needs platform context. Pass it in:

```tsx
// review-output.tsx — signature
export function ReviewOutput({ review, repoPlatform }: { review: Review; repoPlatform?: "github" | "gitlab" | null }) {
  // ...
  {review.review_url && (
    <a href={review.review_url} ...>
      {repoPlatform === "gitlab" ? "View on GitLab" : "View on GitHub"}
      <ExternalLink .../>
    </a>
  )}
}
```

And in `runs/[id]/page.tsx`, pass `repoPlatform={task.repo_platform}` into `<ReviewOutput>`.
(Alternative if you don't want to thread the prop: infer from host —
`review.review_url.includes("gitlab")` — but passing the platform is robust for self-hosted.)

### Step G — Verify the `target_type` mapping (G4)

Confirm what `target_type` GitLab MR tasks are stored with. Check
`services/control-plane/src/http/webhook.rs` `handle_gitlab_merge_request` (the `NewTask` it builds —
look for `target_type`). If it stores `"merge_request"`, extend `targetUrl` in `tasks.ts`:

```ts
switch (task.target_type) {
  case "pull_request":
  case "merge_request":   // GitLab
    return task.repo_platform === "gitlab"
      ? `${base}/-/merge_requests/${task.target_id}`
      : `${base}/pull/${task.target_id}`;
  // ...
}
```

If it already stores `"pull_request"` (the branch's assumption), no change is needed — but **verify
before merging**, since a wrong assumption yields broken MR links.

### Step H — Remove the env var from deployment (G6)

- `compose.yaml` — drop `GITLAB_URL:` under the `web` service (if present).
- Helm chart / `deploy/` values — drop the `web.env.GITLAB_URL` key.
- Any `.env.example` / docs referencing `GITLAB_URL` for the web app — update to point at the control
  plane config instead.

---

## 6. Verification

1. **Backend compiles + endpoint works:**
   ```bash
   cargo check -p control-plane
   # smoke: curl the endpoint (needs auth) -> {"gitlab_base_url":"https://gitlab.example.com"}
   ```
2. **Frontend typechecks + lints:**
   ```bash
   pnpm --filter web lint && pnpm --filter web build
   ```
3. **Manual:** with a GitLab repo connected, open the Repositories card and a run detail page —
   links must point at the self-hosted GitLab host (not `gitlab.com`) and labels must read
   "View on GitLab" / "MR #<iid>". With GitLab unconfigured, links fall back to `https://gitlab.com`
   and GitHub repos are unaffected.
4. **`target_type` check (Step G):** confirm MR deep links resolve to the correct GitLab MR.

---

## 7. File reference

| Concern | File | Lines |
|---|---|---|
| Task link logic | `apps/web/lib/domain/tasks.ts` | `repoUrl` 105, `targetUrl` 117, `triggerLabel` 135, `repo_platform` 27 |
| Repo link logic | `apps/web/lib/domain/repos.ts` | `repoUrl` 32 |
| GitLab URL (env — to remove) | `apps/web/lib/utils/config.ts` | `gitlabBaseUrl` 16 |
| Repo page consumer | `apps/web/app/dashboard/repositories/page.tsx` | 45 |
| Repo card (label bug G1) | `apps/web/components/repos/repo-list.tsx` | `repoUrl` 91, label 96 |
| Run detail consumer | `apps/web/app/dashboard/runs/[id]/page.tsx` | `repoUrl` 118, `targetUrl` 120, `triggerLabel` 78 |
| Review permalink (label bug G2) | `apps/web/components/runs/review-output.tsx` | `review_url` 58, label 65 |
| Web API client | `apps/web/lib/server/api.ts` | `listRepositories` 99, `controlPlaneUrl` 13 |
| Backend GitLab config | `services/control-plane/src/config.rs` | `GitlabSection` 36, `default_api_url` 40/63 |
| Backend GitLab registry | `services/control-plane/src/integrations/gitlab.rs` | `GitlabRegistry` 168, `api_url` 35, `from_config` 173 |
| Backend route registration | `services/control-plane/src/main.rs` | routes 301–323 |
| GitLab MR task creation (verify G4) | `services/control-plane/src/http/webhook.rs` | `handle_gitlab_merge_request` 316 |
