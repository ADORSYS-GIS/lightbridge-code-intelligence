# GitLab Integration Report — Adding GitLab support alongside GitHub

> **Status:** Architectural analysis and integration plan. No GitLab code exists in the codebase
> today. This report is based on a full study of the current codebase (post-`git pull`, including
> recent ADRs 0063–0070) and identifies every GitHub coupling point and what a GitLab integration
> would require.

---

## Table of contents

1. [Executive summary](#1-executive-summary)
2. [Current architecture: where GitHub lives](#2-current-architecture-where-github-lives)
3. [GitHub vs GitLab concept comparison](#3-github-vs-gitlab-concept-comparison)
4. [Detailed coupling analysis](#4-detailed-coupling-analysis)
5. [Database schema changes required](#5-database-schema-changes-required)
6. [Proposed architecture: platform-abstraction layer](#6-proposed-architecture-platform-abstraction-layer)
7. [Implementation plan (phased)](#7-implementation-plan-phased)
8. [New env vars and config](#8-new-env-vars-and-config)
9. [Webhook event mapping (detailed)](#9-webhook-event-mapping-detailed)
10. [API call mapping (detailed)](#10-api-call-mapping-detailed)
11. [What does NOT change](#11-what-does-not-change)
12. [Risks and open questions](#12-risks-and-open-questions)

---

## 1. Executive summary

Lightbridge Code Intelligence is a **GitHub-native** system. GitHub is not a pluggable backend —
it is woven into the webhook handler, the integration module, the database schema, the outbox, the
reconciler, and the review renderer. However, the **core intelligence** (agent-runner: tree-sitter
chunking, embeddings, Graphify, SAST, review agent loop) is **entirely platform-agnostic** — it
operates on a Git checkout and talks to the control plane via a platform-neutral internal API.

**The good news:** ~60% of the system needs no changes. The agent-runner, indexer, SAST, review
agent, internal API, K8s launcher, OIDC/JWT, config crate, and dispatcher are all platform-neutral.

**The work:** The remaining ~40% — the webhook handler, integration module, outbox, reconciler,
database schema, and review renderer — is deeply GitHub-specific and needs to be abstracted into a
**platform-abstraction layer** with a GitHub implementation and a new GitLab implementation.

**Effort estimate:** Medium-large. The abstraction is conceptually clean (the system already has a
clear separation between "inbound webhook → task" and "task → outbound post"), but the database
schema migration is the hardest part — it touches every table that stores GitHub IDs.

---

## 2. Current architecture: where GitHub lives

```
                    ┌─────────────────────────────────────────────────────┐
                    │                  CONTROL PLANE                        │
                    │                                                      │
   GitHub ──webhook──▶ │ http/webhook.rs ◀── GITHUB-SPECIFIC (coupling #1)   │
                    │         │                                            │
                    │         ▼                                            │
                    │      tasks table (DB) ── GITHUB-SPECIFIC cols (#4)    │
                    │         │                                            │
                    │    queue/dispatcher.rs ── PLATFORM-AGNOSTIC            │
                    │         │                                            │
                    │         ▼                                            │
   K8s Job ◀─── dispatch │  agent-runner (PLATFORM-AGNOSTIC)                 │
   │                  │         │                                            │
   │                  │         ▼                                            │
   │  ──internal API──▶ │ http/internal.rs ◀── PLATFORM-AGNOSTIC             │
   │                  │         │                                            │
   │                  │    review.rs ── GITHUB-SPECIFIC rendering (#5)       │
   │                  │         │                                            │
   │                  │    outbox.rs ── GITHUB-SPECIFIC outbox (#3)           │
   │                  │         │                                            │
   │                  │    queue/reconciler.rs ── GITHUB-SPECIFIC (#3)       │
   │                  │         │                                            │
   GitHub ◀──API───── │ integrations/github.rs ◀── GITHUB-SPECIFIC (#2)     │
                    │                                                      │
                    └─────────────────────────────────────────────────────┘
```

### The five GitHub coupling points

| # | Component | File(s) | What's GitHub-specific |
|---|-----------|---------|------------------------|
| 1 | **Webhook handler** | `http/webhook.rs` (805 lines) | Route `POST /github/webhook`, HMAC-SHA256 signature, `X-GitHub-Delivery` dedup, GitHub event types, GitHub payload parsing |
| 2 | **Integration module** | `integrations/github.rs` (544 lines) | GitHub App JWT auth, installation tokens, all GitHub REST API calls |
| 3 | **Outbox + reconciler** | `outbox.rs` (218 lines), `queue/reconciler.rs` (431 lines) | `github_outbox` table, installation token minting, GitHub API posting, feedback polling via GitHub reactions |
| 4 | **Database schema** | `db.rs` (2400+ lines), migrations | `github_repo_id`, `github_delivery_id`, `installation_id`, `github_comment_id`, `github_review_id`, `github_deliveries` table, `github_outbox` table |
| 5 | **Review rendering** | `review.rs` | GitHub-specific badges, disclosure blocks, fast-tier framing, `GITHUB_APP_HANDLE` in text |

---

## 3. GitHub vs GitLab concept comparison

### Authentication model

| Concept | GitHub (current) | GitLab (target) |
|---------|------------------|-----------------|
| **App model** | GitHub App (installed per-repo/org) | No direct equivalent. Options: bot user + project/group access token, or OAuth application |
| **Auth credential** | App private key (PEM, RS256) | Access token (project, group, or personal) |
| **Token minting** | App JWT → installation access token (short-lived, ~1h) per installation | Access token is static (has an expiry date, but no per-request minting). Can be rotated. |
| **Installation scope** | `installation_id` (per org/user) → can access multiple repos | Project access token → per-project; Group access token → per-group (all projects in group) |
| **Webhook secret** | `GITHUB_WEBHOOK_SECRET` → HMAC-SHA256 (`X-Hub-Signature-256`) | `GITLAB_WEBHOOK_SECRET` → plain token comparison (`X-Gitlab-Token` header) |
| **Bot identity** | App handle (e.g. `@lightbridge-assistant`) + `user.type == "Bot"` detection | Bot user account (e.g. `@lightbridge-bot`) — no special `type` field; detect by username or configured bot ID |

### Webhook model

| Concept | GitHub | GitLab |
|---------|--------|--------|
| **Webhook URL** | Per-App (one URL for all installations) | Per-project or per-group (configured in project/group settings) |
| **Delivery ID** | `X-GitHub-Delivery` (UUID) | `X-Gitlab-Event-UUID` (UUID) |
| **Event type header** | `X-GitHub-Event` | `X-Gitlab-Event` |
| **Signature header** | `X-Hub-Signature-256` = `sha256=<HMAC>` | `X-Gitlab-Token` = `<plain secret>` |
| **Signature scheme** | HMAC-SHA256 over raw body | Plain string comparison (no HMAC) |
| **Installation events** | `installation`, `installation_repositories` | System hooks: `project_created`, `project_destroyed`, `project_update` |
| **Dedup** | `github_deliveries` table (PRIMARY KEY on delivery UUID) | Same pattern: `gitlab_deliveries` table (PRIMARY KEY on event UUID) |

### Code review model

| Concept | GitHub | GitLab |
|---------|--------|--------|
| **Code review unit** | Pull request (PR) | Merge request (MR) |
| **PR/MR number** | `pull_request.number` (integer) | `merge_request.iid` (integer, project-scoped) |
| **Inline comment** | Review comment (path, line, side: SIDE/START/END) | Discussion thread (position: base_sha, head_sha, start_sha, old_line, new_line, position_type: text) |
| **Review object** | `POST /repos/{o}/{r}/pulls/{pr}/reviews` (event: COMMENT/APPROVE) | No direct equivalent. GitLab has "approvals" (`POST /projects/{id}/merge_requests/{iid}/approve`) but no "review" object with inline comments. Inline comments are posted as discussions. |
| **Issue comment** | `POST /repos/{o}/{r}/issues/{issue}/comments` | `POST /projects/{id}/issues/{iid}/notes` (or `merge_requests/{iid}/notes` for MR notes) |
| **Reactions** | `POST /repos/{o}/{r}/issues/{issue}/reactions` (content: `+1`, `-1`, `eyes`, etc.) | `POST /projects/{id}/issues/{iid}/award_emoji` (name: `thumbsup`, `thumbsdown`, `eyes`) |
| **Labels** | `POST /repos/{o}/{r}/issues/{issue}/labels` | `PUT /projects/{id}/issues/{iid}?labels=comma,separated` or `POST /projects/{id}/issues/{iid}/labels` |
| **PR/MR files (diff)** | `GET /repos/{o}/{r}/pulls/{pr}/files` | `GET /projects/{id}/merge_requests/{iid}/changes` (includes full diff) |
| **PR/MR SHAs** | `GET /repos/{o}/{r}/pulls/{pr}` → `.base.sha`, `.head.sha` | `GET /projects/{id}/merge_requests/{iid}` → `.diff_refs.base_sha`, `.diff_refs.head_sha`, `.diff_refs.start_sha` |
| **Default branch** | `GET /repos/{o}/{r}` → `.default_branch` | `GET /projects/{id}` → `.default_branch` |
| **Project ID** | `repository.id` (numeric, GitHub-global) | `project.id` (numeric, GitLab-instance-global) |

### Repository identification

| Concept | GitHub | GitLab |
|---------|--------|--------|
| **Full name** | `owner/repo` (e.g. `adorsys-gis/lightbridge`) | `namespace/path` (e.g. `adorsys-gis/lightbridge`) — same format |
| **Numeric ID** | `repository.id` (GitHub's global numeric ID) | `project.id` (GitLab's instance-local numeric ID) |
| **Clone URL** | `https://github.com/{owner}/{repo}.git` | `https://gitlab.example.com/{namespace}/{path}.git` |
| **Web URL** | `repository.html_url` | `project.web_url` |

---

## 4. Detailed coupling analysis

### Coupling #1: Webhook handler (`http/webhook.rs`)

**Current state:**

The handler is a single function `github_webhook` registered at `POST /github/webhook` (in
`main.rs` line 231). It runs a fixed pipeline:

1. **Verify signature** — `verify_signature()` does constant-time HMAC-SHA256 over the raw body
   with `GITHUB_WEBHOOK_SECRET`, compares to `X-Hub-Signature-256` header (`sha256=<hex>`).
   Empty secret → reject all (fail-closed).

2. **Dedup** — reads `X-GitHub-Delivery` (UUID), inserts into `github_deliveries` table with
   `ON CONFLICT DO NOTHING`. If `rows_affected() == 0` → `202 duplicate delivery`. Without a DB
   (`ALLOW_NO_DB`), uses in-memory `HashSet`.

3. **Event routing** — reads `X-GitHub-Event` header:
   - `pull_request` → `handle_pull_request` (opened → fast review, closed → cancel)
   - `push` → `handle_push` (default branch → re-index)
   - `issue_comment` → `handle_issue_comment` (@mention → deep review/answer)
   - `installation` → `handle_installation` (created → pending, deleted → disable+purge)
   - `installation_repositories` → `handle_installation_repositories` (added → pending, removed → disable+purge)

4. **Payload parsing** — deeply GitHub-specific JSON paths:
   - `payload.repository.owner.login`, `payload.repository.name`
   - `payload.repository.id` (GitHub numeric repo ID)
   - `payload.pull_request.number`, `.base.sha`, `.head.sha`, `.user.type`, `.user.login`
   - `payload.installation.id`
   - `payload.comment.id`, `.body`, `.user.type`, `.user.login`
   - `payload.ref` (for push events)
   - `payload.action` (for PR/comment events)

5. **Bot detection** — `user.type == "Bot"` or login ends with `[bot]` (RFC-0003: skip auto
   fast-tier review on bot-authored PRs).

**What GitLab needs:**

- New route: `POST /gitlab/webhook` (or a unified `POST /webhook` with platform detection)
- New signature verification: plain string comparison of `X-Gitlab-Token` header against
  `GITLAB_WEBHOOK_SECRET` (no HMAC)
- New dedup: `X-Gitlab-Event-UUID` → `gitlab_deliveries` table (or generalized `webhook_deliveries`)
- New event routing based on `X-Gitlab-Event` header:
  - `Merge Request Hook` → handle merge_request opened/closed
  - `Push Hook` → handle push to default branch
  - `Note Hook` → handle @mention on MR/Issue
  - System hooks → handle project_created/project_destroyed
- New payload parsing: GitLab webhook payloads have different JSON structure
- New bot detection: GitLab has no `user.type == "Bot"` field; detect by configured bot username

### Coupling #2: Integration module (`integrations/github.rs`)

**Current state:**

`GithubApp` struct holds the App ID + private key. Methods:
- `from_env()` — reads `GITHUB_APP_ID` + `GITHUB_APP_PRIVATE_KEY`
- `installation_token()` — mints App JWT (RS256, ~9min, backdated 60s), exchanges for
  installation access token via `POST /app/installations/{id}/access_tokens`
- API calls (all to `api.github.com` with `X-GitHub-Api-Version: 2022-11-28`):
  - `list_pr_files(installation_id, owner, repo, pr)` → `GET /repos/{o}/{r}/pulls/{pr}/files`
  - `create_pr_review(installation_id, owner, repo, pr, ...)` → `POST /repos/{o}/{r}/pulls/{pr}/reviews`
  - `create_issue_comment(installation_id, owner, repo, issue, body)` → `POST /repos/{o}/{r}/issues/{issue}/comments`
  - `list_review_comments(...)` → `GET /repos/{o}/{r}/pulls/{pr}/reviews/{review_id}/comments`
  - `list_comment_reactions(...)` → `GET /repos/{o}/{r}/{kind}/comments/{id}/reactions`
  - `repository_default_branch(...)` → `GET /repos/{o}/{r}`
  - `pull_request_shas(...)` → `GET /repos/{o}/{r}/pulls/{pr}`
  - `add_reaction(...)` → `POST /repos/{o}/{r}/issues/{issue}/reactions`
  - `add_comment_reaction(...)` → `POST /repos/{o}/{r}/issues/comments/{comment_id}/reactions`
  - `add_labels(...)` → `POST /repos/{o}/{r}/issues/{issue}/labels`

**What GitLab needs:**

A new `integrations/gitlab.rs` module with a `GitlabClient` struct:
- `from_env()` — reads `GITLAB_API_URL` + `GITLAB_API_TOKEN`
- No token minting (the access token is static, just used as `PRIVATE-TOKEN` header)
- API calls (all to `{GITLAB_API_URL}` with `PRIVATE-TOKEN` header):
  - `list_mr_changes(project_id, mr_iid)` → `GET /projects/{id}/merge_requests/{iid}/changes`
  - `create_mr_note(project_id, mr_iid, body)` → `POST /projects/{id}/merge_requests/{iid}/notes`
  - `create_mr_discussion(project_id, mr_iid, position, body)` → `POST /projects/{id}/merge_requests/{iid}/discussions` (with `position` object for inline comments)
  - `create_issue_note(project_id, issue_iid, body)` → `POST /projects/{id}/issues/{iid}/notes`
  - `list_notes(project_id, mr_iid)` → `GET /projects/{id}/merge_requests/{iid}/notes`
  - `list_award_emoji(project_id, note_id)` → `GET /projects/{id}/issues/{iid}/notes/{note_id}/award_emoji` (or merge_requests variant)
  - `project_default_branch(project_id)` → `GET /projects/{id}`
  - `merge_request_shas(project_id, mr_iid)` → `GET /projects/{id}/merge_requests/{iid}` → `.diff_refs`
  - `add_award_emoji(project_id, note_id, name)` → `POST /projects/{id}/issues/{iid}/notes/{note_id}/award_emoji`
  - `add_labels(project_id, issue_iid, labels)` → `PUT /projects/{id}/issues/{iid}?labels=...`

**Key difference:** GitLab has no "review" object. Inline comments are posted as **discussion
threads** with a `position` object. A "review" in Lightbridge terms maps to:
1. One or more discussion threads (inline comments) — `POST /projects/{id}/merge_requests/{iid}/discussions`
2. A summary note on the MR — `POST /projects/{id}/merge_requests/{iid}/notes`

### Coupling #3: Outbox + reconciler (`outbox.rs` + `queue/reconciler.rs`)

**Current state:**

`outbox.rs` produces intents into the `github_outbox` table:
- `enqueue_review(...)` — review with inline comments + body
- `enqueue_reply(...)` — issue/PR comment
- `enqueue_reaction(...)` — emoji reaction (👀, 👍, 👎)
- `enqueue_failure_notice(...)` — failure comment

Each intent stores: `installation_id`, `owner`, `repo`, `kind`, `payload` (JSON), `dedup_key`.

`queue/reconciler.rs` drains the outbox:
1. Claims rows from `github_outbox` (single-replica, ordered by `created_at, id`)
2. Mints an installation token per `installation_id`
3. Dispatches by `kind`:
   - `review` → `create_pr_review`
   - `reply` → `create_issue_comment`
   - `reaction` → `add_reaction` or `add_comment_reaction`
   - `failure_notice` → `create_issue_comment`
4. Marks row as `posted` or `failed`
5. Also runs a feedback poller: reads 👍/👎 reactions via `list_comment_reactions` and stores
   them in `review_feedback`

**What GitLab needs:**

- Either generalize `github_outbox` → `outbox` (add a `platform` column) or add a parallel
  `gitlab_outbox` table
- New reconciler logic (or a parallel reconciler) that:
  - Uses `GITLAB_API_TOKEN` instead of minting installation tokens
  - Dispatches by `kind`:
    - `review` → `create_mr_discussion` (for inline comments) + `create_mr_note` (for body)
    - `reply` → `create_mr_note` or `create_issue_note`
    - `reaction` → `add_award_emoji`
    - `failure_notice` → `create_mr_note` or `create_issue_note`
  - Feedback poller: reads award emoji via `list_award_emoji`

**Key difference:** GitHub's "review" object (with inline comments) has no GitLab equivalent.
GitLab posts inline comments as discussion threads. The outbox `review` kind needs to be
decomposed into multiple GitLab API calls.

### Coupling #4: Database schema

**Current state (GitHub-specific columns/tables):**

| Table | Column | GitHub meaning |
|-------|--------|----------------|
| `repositories` | `github_repo_id` | GitHub's numeric repo ID |
| `repositories` | `installation_id` | GitHub App installation ID |
| `tasks` | `github_delivery_id` | GitHub webhook delivery UUID |
| `tasks` | `installation_id` | GitHub App installation ID (for token minting) |
| `tasks` | `trigger_comment_id` | GitHub comment ID (for @mention tasks) |
| `reviews` | `github_review_id` | GitHub review ID (for correlation) |
| `review_comments` | `github_comment_id` | GitHub comment ID (for feedback polling) |
| `review_feedback` | (table) | Stores 👍/👎 reactions from GitHub |
| `github_deliveries` | (table) | Webhook delivery dedup (PRIMARY KEY on delivery UUID) |
| `github_outbox` | (table) | GitHub egress queue |

**What GitLab needs:**

Two approaches:

**Approach A: Generalize existing columns (add `platform` discriminator)**

| Table | Change | New schema |
|-------|--------|------------|
| `repositories` | Rename `github_repo_id` → `platform_repo_id` + add `platform` column | `platform TEXT NOT NULL DEFAULT 'github'`, `platform_repo_id BIGINT` |
| `repositories` | Rename `installation_id` → `platform_installation_id` | `platform_installation_id BIGINT` (GitLab: project ID or group ID) |
| `tasks` | Rename `github_delivery_id` → `webhook_delivery_id` | `webhook_delivery_id UUID` |
| `tasks` | Rename `installation_id` → `platform_installation_id` | `platform_installation_id BIGINT` |
| `tasks` | `trigger_comment_id` stays | (GitLab note ID is also numeric) |
| `reviews` | Rename `github_review_id` → `platform_review_id` | `platform_review_id BIGINT` (GitLab: discussion ID or note ID) |
| `review_comments` | Rename `github_comment_id` → `platform_comment_id` | `platform_comment_id BIGINT` |
| `github_deliveries` | Rename → `webhook_deliveries` + add `platform` column | `platform TEXT`, `delivery_id UUID` |
| `github_outbox` | Rename → `outbox` + add `platform` column | `platform TEXT`, ... |
| `review_feedback` | Add `platform` column | `platform TEXT` |

**Approach B: Add parallel GitLab columns/tables**

Keep all `github_*` columns/tables as-is, add `gitlab_*` equivalents. Simpler migration but
duplicates schema and complicates queries.

**Recommendation: Approach A** (generalize). It's a bigger migration but cleaner long-term.

### Coupling #5: Review rendering (`review.rs`)

**Current state:**

`review.rs` renders the review body and inline comments:
- `render_fast_body` — fast-tier framing ("🅵 quick pass — mention @handle for a deeper review")
- `GITHUB_APP_HANDLE` used in rendered text
- Badges (shield.io): `![P0](https://img.shields.io/badge/P0-Critical-red)`
- Disclosure blocks (`<details>` HTML — GitHub supports this in Markdown)
- Diff scoping uses GitHub PR file patches (the `patch` field from `list_pr_files`)

**What GitLab needs:**

- `GITHUB_APP_HANDLE` → generalize to `platform_bot_handle` (or just `BOT_HANDLE`)
- `<details>` HTML — GitLab also supports this in Markdown (GLFM is similar to GFM)
- Shield.io badges — GitLab also renders these in Markdown
- Diff scoping — GitLab's `GET /merge_requests/{iid}/changes` returns diffs in a different
  format (`diff` field with `new_path`, `old_path`, `new_line`, `old_line`, `diff` text). The
  diff scoping logic needs to parse GitLab's diff format.
- Fast-tier framing — same concept, just use the GitLab bot handle

---

## 5. Database schema changes required

### Migration plan (Approach A: generalize)

```sql
-- 1. Add platform column to repositories
ALTER TABLE repositories ADD COLUMN platform TEXT NOT NULL DEFAULT 'github';
ALTER TABLE repositories RENAME COLUMN github_repo_id TO platform_repo_id;
ALTER TABLE repositories RENAME COLUMN installation_id TO platform_installation_id;

-- 2. Add platform column to tasks
ALTER TABLE tasks RENAME COLUMN github_delivery_id TO webhook_delivery_id;
ALTER TABLE tasks RENAME COLUMN installation_id TO platform_installation_id;

-- 3. Rename reviews column
ALTER TABLE reviews RENAME COLUMN github_review_id TO platform_review_id;

-- 4. Rename review_comments column
ALTER TABLE review_comments RENAME COLUMN github_comment_id TO platform_comment_id;

-- 5. Rename github_deliveries → webhook_deliveries
ALTER TABLE github_deliveries RENAME TO webhook_deliveries;
ALTER TABLE webhook_deliveries ADD COLUMN platform TEXT NOT NULL DEFAULT 'github';

-- 6. Rename github_outbox → outbox
ALTER TABLE github_outbox RENAME TO outbox;
ALTER TABLE outbox ADD COLUMN platform TEXT NOT NULL DEFAULT 'github';

-- 7. Add platform to review_feedback
ALTER TABLE review_feedback ADD COLUMN platform TEXT NOT NULL DEFAULT 'github';
```

### New indexes

```sql
-- Ensure uniqueness per platform (a GitHub repo ID and a GitLab project ID could collide)
CREATE UNIQUE INDEX repositories_platform_repo_id_unique
    ON repositories (platform, platform_repo_id);
```

---

## 6. Proposed architecture: platform-abstraction layer

### Trait-based abstraction

```rust
// services/control-plane/src/integrations/platform.rs (new)

#[async_trait]
pub trait CodePlatform: Send + Sync {
    /// Verify a webhook signature (HMAC for GitHub, plain token for GitLab)
    fn verify_webhook(&self, headers: &HeaderMap, body: &[u8]) -> Result<bool>;

    /// Dedup key from webhook headers (delivery UUID)
    fn delivery_id(&self, headers: &HeaderMap) -> Option<String>;

    /// Parse a webhook body into a normalized platform event
    fn parse_event(&self, event_type: &str, body: &Value) -> Result<PlatformEvent>;

    /// List changed files in a PR/MR (for diff scoping)
    async fn list_changed_files(&self, repo: &RepoRef, pr_number: i64) -> Result<Vec<ChangedFile>>;

    /// Get the default branch of a repository
    async fn default_branch(&self, repo: &RepoRef) -> Result<String>;

    /// Get the base/head SHAs of a PR/MR
    async fn pr_shas(&self, repo: &RepoRef, pr_number: i64) -> Result<(String, String)>;

    /// Post a review (inline comments + body) — returns platform review ID
    async fn post_review(&self, repo: &RepoRef, pr_number: i64, review: &ReviewPayload) -> Result<i64>;

    /// Post a comment on an issue/PR
    async fn post_comment(&self, repo: &RepoRef, issue_number: i64, body: &str) -> Result<i64>;

    /// Add a reaction (emoji) to an issue/PR or comment
    async fn add_reaction(&self, repo: &RepoRef, target: ReactionTarget, emoji: &str) -> Result<()>;

    /// Add labels to an issue/PR
    async fn add_labels(&self, repo: &RepoRef, issue_number: i64, labels: &[String]) -> Result<()>;

    /// List reactions on a comment (for feedback polling)
    async fn list_comment_reactions(&self, repo: &RepoRef, comment_id: i64) -> Result<Vec<Reaction>>;

    /// Clone URL for a repository (with credentials)
    fn clone_url(&self, repo: &RepoRef) -> String;
}

pub enum PlatformEvent {
    PullRequestOpened { repo: RepoRef, pr_number: i64, base_sha: String, head_sha: String, author: Author },
    PullRequestClosed { repo: RepoRef, pr_number: i64 },
    Push { repo: RepoRef, ref_name: String, before: String, after: String },
    Mention { repo: RepoRef, target: MentionTarget, comment_id: i64, body: String, author: Author },
    InstallationCreated { repos: Vec<RepoRef> },
    InstallationDeleted { repos: Vec<RepoRef> },
}

pub struct RepoRef {
    pub platform: Platform,
    pub full_name: String,      // "owner/repo" or "namespace/path"
    pub platform_id: i64,       // GitHub repo ID or GitLab project ID
    pub installation_id: Option<i64>,  // GitHub installation ID or GitLab project ID
}

pub enum Platform {
    GitHub,
    GitLab,
}
```

### Implementations

```
services/control-plane/src/integrations/
├── platform.rs          # trait + types (new)
├── github.rs            # impl CodePlatform for GithubApp (refactor existing)
├── gitlab.rs            # impl CodePlatform for GitlabClient (new)
├── k8s.rs               # unchanged
└── neo4j.rs             # unchanged
```

### Webhook routing

```rust
// main.rs — unified webhook router

// Instead of a single hardcoded /github/webhook route:
.route("/github/webhook", post(webhook::github_webhook))
.route("/gitlab/webhook", post(webhook::gitlab_webhook))

// Or a unified route with platform detection:
.route("/webhook", post(webhook::webhook_router))
// where webhook_router detects the platform from headers
```

### Outbox generalization

```rust
// outbox.rs — generalized
pub async fn enqueue_review(
    db: &PgPool,
    platform: Platform,      // new
    repo: &RepoRef,
    review: ReviewPayload,
) -> Result<()>;

// The outbox table gets a `platform` column
// The reconciler dispatches to the right CodePlatform impl
```

### Reconciler generalization

```rust
// queue/reconciler.rs — dispatch by platform
match outbox_row.platform {
    Platform::GitHub => github_client.post_review(...).await,
    Platform::GitLab => gitlab_client.post_review(...).await,
}
```

---

## 7. Implementation plan (phased)

### Phase 0: Preparation (no behavior change)

1. **Introduce the `CodePlatform` trait** (`integrations/platform.rs`) with types
2. **Refactor `github.rs`** to implement `CodePlatform` (move existing logic behind the trait)
3. **Refactor `webhook.rs`** to parse into `PlatformEvent` (normalized) instead of acting directly
4. **Refactor `outbox.rs`** to accept a `Platform` parameter (default `GitHub`)
5. **Refactor `reconciler.rs`** to dispatch via `CodePlatform` trait
6. **All existing behavior unchanged** — GitHub still works exactly as before

### Phase 1: Database migration (no behavior change)

7. **Run the schema migration** (rename columns, add `platform` column, rename tables)
8. **Update all SQL queries** in `db.rs` to use the new column names
9. **All existing behavior unchanged** — GitHub still works exactly as before

### Phase 2: GitLab integration module

10. **Create `integrations/gitlab.rs`** — implement `CodePlatform` for GitLab
11. **Add GitLab webhook route** — `POST /gitlab/webhook`
12. **Add GitLab webhook handler** — parse GitLab webhook payloads into `PlatformEvent`
13. **Add GitLab env vars** — `GITLAB_API_URL`, `GITLAB_API_TOKEN`, `GITLAB_WEBHOOK_SECRET`, `GITLAB_BOT_HANDLE`
14. **Update outbox + reconciler** to handle `Platform::GitLab`
15. **Update review rendering** to use the platform's bot handle (not `GITHUB_APP_HANDLE`)

### Phase 3: Testing & documentation

16. **Add integration tests** for GitLab webhook handling
17. **Add integration tests** for GitLab API calls (mock or against a local GitLab instance)
18. **Update `explain.md`** with GitLab setup instructions
19. **Add an ADR** for the platform-abstraction layer
20. **Update `local-dev-guide.md`** with local GitLab setup

### Phase 4: Polish

21. **Bot detection** — GitLab has no `user.type == "Bot"` field; detect by configured bot username
22. **Diff format** — GitLab's diff format differs from GitHub's; update diff scoping in `review.rs`
23. **Review posting** — GitLab has no "review" object; decompose into discussions + note
24. **Feedback polling** — GitLab uses award emoji instead of reactions; update the poller

---

## 8. New env vars and config

### Control plane (serve role)

```bash
# --- GitLab (new) ---
GITLAB_API_URL=https://gitlab.example.com/api/v4     # GitLab API base URL
GITLAB_API_TOKEN=<bot-access-token>                  # Project or group access token
GITLAB_WEBHOOK_SECRET=<shared-secret>                # Plain token (X-Gitlab-Token header)
GITLAB_BOT_HANDLE=<bot-username>                     # e.g. lightbridge-bot (for @mention detection)
GITLAB_BOT_USER_ID=<bot-user-id>                     # Optional: numeric user ID for bot detection
```

### AppState changes (`main.rs`)

```rust
pub struct AppState {
    // ... existing fields ...

    // GitHub (existing, now optional alongside GitLab)
    github: Option<github::GithubApp>,
    github_webhook_secret: Arc<String>,
    github_app_handle: Arc<String>,

    // GitLab (new)
    gitlab: Option<gitlab::GitlabClient>,
    gitlab_webhook_secret: Arc<String>,
    gitlab_bot_handle: Arc<String>,
}
```

### Config file changes (`control-plane.json`)

```json
{
  "platform": {
    "github": {
      "enabled": true,
      "app_id": "{env:GITHUB_APP_ID}",
      "app_handle": "{env:GITHUB_APP_HANDLE:-lightbridge-assistant}"
    },
    "gitlab": {
      "enabled": "{env:GITLAB_ENABLED:-false}",
      "api_url": "{env:GITLAB_API_URL}",
      "bot_handle": "{env:GITLAB_BOT_HANDLE:-lightbridge-bot}"
    }
  },
  "agent": { ... },
  "dispatcher": { ... },
  "review": { ... },
  "embeddings": { ... }
}
```

---

## 9. Webhook event mapping (detailed)

### GitHub → GitLab event mapping

| GitHub event | GitHub action | GitHub handler | GitLab event | GitLab trigger | GitLab handler (new) |
|---|---|---|---|---|---|
| `pull_request` | `opened` | `handle_pull_request` → fast review | `Merge Request Hook` | `object_attributes.action == "open"` | `handle_merge_request` → fast review |
| `pull_request` | `closed` | `handle_pull_request` → cancel | `Merge Request Hook` | `object_attributes.action == "close"` | `handle_merge_request` → cancel |
| `push` | (default branch) | `handle_push` → re-index | `Push Hook` | `ref == refs/heads/<default>` | `handle_push` → re-index |
| `issue_comment` | `created` + @mention | `handle_issue_comment` → deep review/answer | `Note Hook` | `object_attributes.noteable_type == "MergeRequest" \|\| "Issue"` + @mention | `handle_note` → deep review/answer |
| `installation` | `created` | `handle_installation` → register pending | System hook: `project_created` | (when bot is added to project) | `handle_project_created` → register pending |
| `installation` | `deleted` | `handle_installation` → disable + purge | System hook: `project_destroyed` | | `handle_project_destroyed` → disable + purge |
| `installation_repositories` | `added` | `handle_installation_repositories` → pending | System hook: `project_added` | | `handle_project_added` → pending |
| `installation_repositories` | `removed` | `handle_installation_repositories` → purge | System hook: `project_removed` | | `handle_project_removed` → purge |

### GitLab webhook payload structure (key fields)

**Merge Request Hook:**
```json
{
  "object_kind": "merge_request",
  "event_uuid": "...",
  "object_attributes": {
    "action": "open",
    "iid": 42,
    "source_branch": "feature",
    "target_branch": "main",
    "last_commit": { "id": "abc123..." },
    "source": { "default_branch": "main" }
  },
  "project": { "id": 123, "path_with_namespace": "group/repo", "default_branch": "main" },
  "user": { "username": "someone" }
}
```

**Push Hook:**
```json
{
  "object_kind": "push",
  "event_uuid": "...",
  "ref": "refs/heads/main",
  "before": "abc123...",
  "after": "def456...",
  "project": { "id": 123, "path_with_namespace": "group/repo", "default_branch": "main" }
}
```

**Note Hook:**
```json
{
  "object_kind": "note",
  "event_uuid": "...",
  "object_attributes": {
    "id": 456,
    "note": "@lightbridge-bot please review",
    "noteable_type": "MergeRequest",
    "author_id": 789
  },
  "merge_request": { "iid": 42, "last_commit": { "id": "abc..." } },
  "project": { "id": 123, "path_with_namespace": "group/repo" },
  "user": { "username": "someone" }
}
```

---

## 10. API call mapping (detailed)

| Lightbridge action | GitHub API | GitLab API |
|---|---|---|
| List changed files | `GET /repos/{o}/{r}/pulls/{pr}/files` | `GET /projects/{id}/merge_requests/{iid}/changes` |
| Get default branch | `GET /repos/{o}/{r}` → `.default_branch` | `GET /projects/{id}` → `.default_branch` |
| Get PR/MR SHAs | `GET /repos/{o}/{r}/pulls/{pr}` → `.base.sha`, `.head.sha` | `GET /projects/{id}/merge_requests/{iid}` → `.diff_refs.base_sha`, `.diff_refs.head_sha`, `.diff_refs.start_sha` |
| Post review (inline comments + body) | `POST /repos/{o}/{r}/pulls/{pr}/reviews` (event: COMMENT, comments: [{path, line, side, body}]) | **Multiple calls:** `POST /projects/{id}/merge_requests/{iid}/discussions` (one per inline comment, with `position` object) + `POST /projects/{id}/merge_requests/{iid}/notes` (body) |
| Post issue/PR comment | `POST /repos/{o}/{r}/issues/{issue}/comments` | `POST /projects/{id}/merge_requests/{iid}/notes` (for MR) or `POST /projects/{id}/issues/{iid}/notes` (for issue) |
| Add reaction (emoji) | `POST /repos/{o}/{r}/issues/{issue}/reactions` (content: `+1`, `-1`, `eyes`) | `POST /projects/{id}/issues/{iid}/award_emoji` (name: `thumbsup`, `thumbsdown`, `eyes`) or `POST /projects/{id}/merge_requests/{iid}/award_emoji` |
| Add reaction to comment | `POST /repos/{o}/{r}/issues/comments/{id}/reactions` | `POST /projects/{id}/issues/{iid}/notes/{id}/award_emoji` or `POST /projects/{id}/merge_requests/{iid}/notes/{id}/award_emoji` |
| Add labels | `POST /repos/{o}/{r}/issues/{issue}/labels` | `PUT /projects/{id}/issues/{iid}?labels=comma,separated` or `POST /projects/{id}/issues/{iid}/labels` |
| List comment reactions (feedback) | `GET /repos/{o}/{r}/{kind}/comments/{id}/reactions` | `GET /projects/{id}/issues/{iid}/notes/{id}/award_emoji` or `GET /projects/{id}/merge_requests/{iid}/notes/{id}/award_emoji` |
| Clone URL | `https://github.com/{owner}/{repo}.git` (with installation token as bearer) | `https://gitlab.example.com/{namespace}/{path}.git` (with `gitlab-ci-token` or access token) |

### GitLab inline comment (discussion) position object

```json
{
  "position": {
    "base_sha": "abc123...",
    "head_sha": "def456...",
    "start_sha": "abc123...",
    "position_type": "text",
    "new_path": "src/main.rs",
    "new_line": 42,
    "old_path": "src/main.rs",
    "old_line": null
  },
  "body": "P1: This function lacks error handling..."
}
```

---

## 11. What does NOT change

These components are **entirely platform-agnostic** and need **zero changes**:

| Component | Location | Why it's platform-agnostic |
|-----------|----------|---------------------------|
| **Agent-runner** (all of it) | `services/agent-runner/src/` | Clones a Git URL, indexes, runs SAST, runs review agent. Talks to control plane via internal API. Never talks to GitHub/GitLab directly. |
| **Tree-sitter chunker** | `indexer/chunker.rs` | Parses source code, not platform-specific |
| **Embeddings client** | `indexer/embeddings.rs` | Talks to OpenAI-compatible API |
| **Graphify integration** | `indexer/graph.rs` | Spawns Graphify CLI on a checkout |
| **SAST (opengrep)** | `sast/mod.rs` | Runs opengrep on changed files |
| **Review agent loop** | `review/native/agent.rs` | Multi-turn chat with mediated tools |
| **Review tools** | `review/native/tools.rs` | Vector search, graph search, read_file, add_review_comment — all go through the internal API |
| **Chat client** | `review/native/chat.rs` | Talks to OpenAI-compatible LLM API |
| **Internal API** | `http/internal.rs` | Task context, chunk ingest, graph ingest, search, review buffer, finalize — all platform-neutral |
| **K8s launcher** | `integrations/k8s.rs` | Generates Job manifests with env vars — platform-neutral |
| **OIDC/JWT** | `jwt.rs` | Keycloak token validation — platform-neutral |
| **Config crate** | `services/config/` | JSON config loader — platform-neutral |
| **Dispatcher** | `queue/dispatcher.rs` | Claims tasks and launches Jobs — platform-neutral |
| **xtask** | `xtask/` | CI automation — platform-neutral |
| **Web console** | `apps/web/` | Talks to control plane API — platform-neutral (may need UI updates to show GitLab vs GitHub) |
| **Docker images** | `Dockerfile` | Build control-plane and agent-runner images — platform-neutral |
| **CI/CD workflows** | `.github/workflows/` | Build/test/lint — platform-neutral |

---

## 12. Risks and open questions

### Risks

| Risk | Impact | Mitigation |
|------|--------|-----------|
| **Database migration** touches every table | High — could break existing GitHub functionality | Phase 0 (trait) + Phase 1 (migration) are separate; migration is pure rename + add column with defaults; test thoroughly |
| **GitLab has no "review" object** | Medium — the outbox `review` kind needs to be decomposed into multiple GitLab API calls | The `CodePlatform::post_review` trait method encapsulates this; the reconciler doesn't need to know |
| **GitLab diff format differs** | Medium — diff scoping in `review.rs` parses GitHub's `patch` field | Add a diff-format abstraction or parse GitLab's diff format in the GitLab integration |
| **Bot detection differs** | Low — GitLab has no `user.type == "Bot"` field | Detect by configured bot username or user ID |
| **GitLab webhook configuration is per-project** | Medium — no "install App" flow; each project needs webhook configured manually | Document the setup; consider a GitLab system hook for auto-registration |
| **GitLab token is static** | Low — no per-request token minting like GitHub App | Simpler, but token rotation is manual. Document rotation procedure. |
| **Two webhook secrets** | Low — `GITHUB_WEBHOOK_SECRET` and `GITLAB_WEBHOOK_SECRET` | Separate routes (`/github/webhook`, `/gitlab/webhook`) each read their own secret |
| **Reconciler complexity** | Medium — needs to handle two platforms | The `CodePlatform` trait dispatches; the reconciler loop stays the same |
| **Feedback polling** | Low — GitLab uses award emoji instead of reactions | The `list_comment_reactions` trait method abstracts this |

### Open questions

1. **Should the webhook route be unified or separate?**
   - Separate (`/github/webhook`, `/gitlab/webhook`) — simpler, each handler knows its platform
   - Unified (`/webhook` with platform detection from headers) — more elegant but harder to debug
   - **Recommendation:** Separate routes. Simpler and each handler is self-contained.

2. **Should the outbox table be unified or separate?**
   - Unified (`outbox` table with `platform` column) — single reconciler, simpler
   - Separate (`github_outbox`, `gitlab_outbox`) — two reconcilers, more complex
   - **Recommendation:** Unified. One reconciler, one drain loop, dispatch by `platform` column.

3. **Should we support GitLab self-hosted and GitLab.com?**
   - GitLab.com uses `https://gitlab.com/api/v4`
   - Self-hosted uses `https://gitlab.example.com/api/v4`
   - `GITLAB_API_URL` env var handles both — no code difference

4. **How to handle GitLab's lack of an "install App" flow?**
   - GitHub: install App → `installation` webhook → repos appear as pending
   - GitLab: no equivalent. Each project needs webhook configured manually.
   - Options: (a) require manual repo approval in the console (same as GitHub's approval gate),
     (b) use GitLab system hooks for auto-registration, (c) provide a setup script
   - **Recommendation:** Start with (a) — manual webhook + manual approval. Add system hooks later.

5. **Should the agent-runner know about the platform?**
   - Currently: no. It clones a Git URL and talks to the internal API.
   - The clone URL needs a token. Currently the control plane provides a GitHub installation
     token via the internal API. For GitLab, it would provide the GitLab access token.
   - The clone module (`clone.rs`) just needs a URL + token — it doesn't care about the platform.
   - **Answer:** No changes needed in the agent-runner. The control plane provides the right
     clone URL + token via the internal API.

6. **How to handle GitLab's per-project webhook configuration at scale?**
   - GitHub: one App installation covers all repos in an org
   - GitLab: webhooks are per-project (or per-group with GitLab Premium+)
   - For large GitLab instances, group-level webhooks (Premium+) or system hooks can help
   - **Recommendation:** Support per-project webhooks initially; document group-level webhooks as
     a Premium+ option.

---

## Summary

| Aspect | GitHub (current) | GitLab (target) | Effort |
|--------|------------------|-----------------|--------|
| Auth model | GitHub App (JWT → installation tokens) | Bot user + access token | New module |
| Webhook signature | HMAC-SHA256 (`X-Hub-Signature-256`) | Plain token (`X-Gitlab-Token`) | New handler |
| Webhook dedup | `X-GitHub-Delivery` → `github_deliveries` | `X-Gitlab-Event-UUID` → `webhook_deliveries` | Schema migration |
| Event types | `pull_request`, `push`, `issue_comment`, `installation` | `Merge Request Hook`, `Push Hook`, `Note Hook`, system hooks | New handler |
| Review posting | `POST /pulls/{pr}/reviews` (review object) | `POST /merge_requests/{iid}/discussions` + `/notes` | New API calls |
| Reactions | `POST /issues/{issue}/reactions` (content: `+1`) | `POST /issues/{iid}/award_emoji` (name: `thumbsup`) | New API calls |
| Labels | `POST /issues/{issue}/labels` | `PUT /issues/{iid}?labels=...` | New API calls |
| Clone auth | Installation token (short-lived) | Access token (static) | Control plane provides token |
| DB schema | `github_*` columns/tables | Generalized `platform_*` columns/tables | Migration |
| Agent-runner | No changes | No changes | **Zero** |
| Indexer | No changes | No changes | **Zero** |
| SAST | No changes | No changes | **Zero** |
| Review agent | No changes | No changes | **Zero** |
| Internal API | No changes | No changes | **Zero** |
| K8s launcher | No changes | No changes | **Zero** |
| OIDC/JWT | No changes | No changes | **Zero** |
| Config crate | No changes | No changes | **Zero** |
| Dispatcher | No changes | No changes | **Zero** |
| Web console | No changes (may add platform indicator) | Minor UI update | **Low** |