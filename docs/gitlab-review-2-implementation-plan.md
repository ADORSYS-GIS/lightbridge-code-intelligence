# GitLab Integration — Review & Implementation Plan

> **Scope:** Review-2.txt findings + merge with `main` (ADR-0072 number collision, multi-line review comments) + remaining credible issues from Review-1.txt that were not yet addressed.

---

## 1. Review-2.txt Findings — Status Assessment

### P0 — `review_feedback.github_comment_id` not renamed (CRITICAL)

**Status:** ✅ Fixed (commit 68ea4f7, migration 0025). Now merged into migration 0024.

The initial migration 0024 renamed `review_comments.github_comment_id` but missed `review_feedback.github_comment_id`. The application code (`db.rs`: `rejected_findings_for_repo`, `get_feedback`, `record_feedback`) queries `f.platform_comment_id`, so without this rename every feedback lookup fails with `column f.platform_comment_id does not exist`.

**Action taken:** Merged migration 0025 into 0024. Deleted 0025. The single migration now handles both `review_comments` and `review_feedback` column renames.

---

### P1 — MR-then-issue probe mis-posts to wrong noteable (CRITICAL)

**Status:** ❌ Not fixed. **Credible — needs fixing.**

`post_comment` and `add_reaction` probe MR notes first, then fall back to issue notes on 404. GitLab MRs and issues have **independent iid sequences** (both start at 1), so `!1` and `#1` coexist. When a reply targets issue `#1`, the MR probe succeeds on MR `!1` and the reply goes to the wrong noteable.

The ADR-0072 rationale ("one 404 on the wrong type; acceptable") is a **false premise** — the probe doesn't 404, it succeeds on the wrong resource.

**Fix:** Carry `target_type` ("review" → MR, "ask" → issue) through the outbox payload. The reconciler passes it to `post_comment` / `add_reaction` so the GitLab client hits the correct endpoint directly — no probe.

---

### P1 — Empty GitHub secret forges webhooks in GitLab-only deploys

**Status:** ✅ Already fixed. `verify_signature()` at `webhook.rs:1155` has `if secret.is_empty() { return false; }` — fail-closed on empty secret.

---

### P2 — Per-method token minting increases GitHub API calls

**Status:** ❌ Not fixed. **Credible — worth optimizing.**

`GithubApp::token_for()` calls `self.installation_token()` on every trait method call. `deliver_review` calls `post_review` + `list_review_comments` + `add_labels` — 3 token mints instead of 1. `installation_token()` has no internal TTL cache.

**Fix:** Add a `Mutex<HashMap<i64, (String, Instant)>>` token cache to `GithubApp` with a 50-minute TTL (GitHub installation tokens last 1 hour). `token_for()` checks the cache before minting.

---

### P2 — `@`-passthrough may trigger on non-credential URLs

**Status:** ❌ Not fixed. **Credible but low-risk.**

`authenticated_clone_url()` checks `rest.contains('@')` anywhere in the URL. `@` is legal in URL paths (RFC 3986). A public repo URL with `@` in the path would be passed through without token splicing.

**Fix:** Check for `@` only between scheme and first `/` after host: `rest.split('/').next().contains('@')`.

---

### P2 — `outbox_delivery` metric inflates `posted` on silently-skipped rows

**Status:** ❌ Not fixed. **Credible but minor.**

`failure_notice` dedup gate (`has_responded_or_pending_content`) silently consumes the row and marks it `posted`, inflating the metric.

**Fix:** Add a `"skipped"` outcome: if the dedup gate fires, record `outbox_delivery(platform, kind, "skipped")` instead of `"posted"`.

---

### P2 — GitLab token baked into `clone_url` (Debug-loggable)

**Status:** ❌ Not fixed. **Credible — latent security risk.**

`TaskContextResponse` derives `Debug` and for GitLab the `clone_url` field contains `oauth2:{token}@host`. A `tracing::debug!(?response)` would leak the token.

**Fix:** Remove `Debug` derive from `TaskContextResponse` or implement a custom `Debug` that redacts `clone_url` and `token`.

---

### P2 — `api_headers` silently drops invalid `PRIVATE-TOKEN`

**Status:** ❌ Not fixed. **Credible — should fail fast.**

`api_headers()` uses `if let Ok(v) = HeaderValue::from_str(&self.token)` — silently skips the header on error. Every API call goes out unauthenticated.

**Fix:** Validate the token in `from_env()` — fail construction if `HeaderValue::from_str(&token)` fails.

---

### P2 — `clone_url` doesn't URL-encode token

**Status:** ❌ Not fixed. **Credible but low-risk.**

GitLab tokens are typically URL-safe (alphanumeric + dashes). Fix: `urlencoding::encode(&self.token)` — but adds a dependency. **Defer** unless a token with special characters is observed.

---

### P2 — `owner_repo` splits on first `/` — wrong for nested groups

**Status:** ❌ Not fixed. **Credible but latent.**

`owner_repo()` uses `split_once('/')` → `group/subgroup/repo` → `("group", "subgroup/repo")`. GitLab doesn't use `owner_repo()` (uses `project_encoded()`), and GitHub doesn't have nested groups.

**Fix:** Use `rsplit_once('/')` so name is always the last segment.

---

### P2 — Outbox drain index not explicitly renamed

**Status:** ✅ Not a defect. PostgreSQL auto-renames indexes on table rename.

---

## 2. Merge with `main` — Conflicts & Resolution Plan

### ADR-0072 number collision

Main branch added `ADR-0072: multi-line (range) inline review comments`. Our branch added `ADR-0072: platform-abstraction layer`.

**Resolution:** Renumber our ADR to **ADR-0072** (platform-abstraction layer). Main's ADR-0072 stays as-is. Update:
- `docs/adr/0071-platform-abstraction-layer.md` → `docs/adr/0072-platform-abstraction-layer.md`
- `docs/adr/README.md` — add ADR-0072 after ADR-0072
- All references to "ADR-0072" in our changed files → "ADR-0072"
- Migration 0024 header comment → "ADR-0072"

### `reconciler.rs` — `ReviewComment` → `InlineComment` + `start_line`/`start_side`

Main added `start_line` and `start_side` fields to `ReviewComment` for multi-line inline comments. Our branch renamed `ReviewComment` to `InlineComment` (platform trait type).

**Resolution:** Add `start_line: Option<u32>` and `start_side: Option<&'static str>` to `InlineComment` in `platform.rs`. Update `deliver_review()` in `reconciler.rs` to pass these fields through. Update `GithubApp::post_review()` to include them in the GitHub API payload. Update `GitlabClient::post_review()` to use them in the GitLab `position` object (`line_range`).

### Other auto-merged files

- `client.rs` — both branches modified, auto-merged cleanly
- `internal.rs` — both branches modified, auto-merged cleanly
- `github.rs` — both branches modified, auto-merged cleanly
- `outbox.rs` — both branches modified, auto-merged cleanly

---

## 3. Implementation Plan — Remaining Fixes

### Phase A — P1: Carry `target_type` through outbox (MR-then-issue probe fix)

**Files:**
- `outbox.rs` — Add `target_type: Option<String>` to `ReviewPayload`, `ReplyPayload`, `FailureNoticePayload`
- `reconciler.rs` — Pass `target_type` to `post_comment()` / `add_reaction()`
- `platform.rs` — Add `noteable_type: Option<&str>` param to `post_comment()` and `add_reaction()` (or use `ReactionTarget` enum variant)
- `gitlab.rs` — Use `noteable_type` to select MR vs issue endpoint directly (no probe)
- `github.rs` — No change needed (GitHub uses a unified issues/comments endpoint)

### Phase B — P2: Token cache for `GithubApp`

**Files:**
- `github.rs` — Add `Mutex<HashMap<i64, (String, Instant)>>` to `GithubApp`, TTL 50min. `token_for()` checks cache before minting.

### Phase C — P2: `@`-passthrough guard

**Files:**
- `client.rs` — Change `rest.contains('@')` to `rest.split('/').next().map_or(false, |h| h.contains('@'))`

### Phase D — P2: Redact `clone_url` in Debug

**Files:**
- `internal.rs` — Remove `#[derive(Debug)]` from `TaskContextResponse`, add manual `Debug` impl that redacts `clone_url` and `token`

### Phase E — P2: Validate token in `from_env`

**Files:**
- `gitlab.rs` — In `from_env()`, validate `HeaderValue::from_str(&token)` — return `None` if invalid

### Phase F — P2: `owner_repo` use `rsplit_once`

**Files:**
- `platform.rs` — Change `owner_repo()` to use `rsplit_once('/')`

### Phase G — P2: `outbox_delivery` skipped outcome

**Files:**
- `reconciler.rs` — Add `"skipped"` outcome when `has_responded_or_pending_content` fires

### Phase H — Merge resolution

**Files:**
- `docs/adr/0071-platform-abstraction-layer.md` → rename to `0072-platform-abstraction-layer.md`
- `docs/adr/README.md` — add ADR-0072
- All source files — replace "ADR-0072" with "ADR-0072" in comments
- `platform.rs` — Add `start_line`/`start_side` to `InlineComment`
- `reconciler.rs` — Merge `InlineComment` with main's `start_line` fields
- `github.rs` — Pass `start_line`/`start_side` in `post_review()`
- `gitlab.rs` — Use `start_line`/`start_side` in `position` object

---

## 4. Priority & Sequencing

| Phase | Priority | Risk if skipped | Effort |
|---|---|---|---|
| H (merge) | **Blocker** | Can't merge PR | Medium |
| A (P1 probe) | **High** | Replies go to wrong MR/issue | Medium |
| B (token cache) | Medium | 3x GitHub API rate limit consumption | Low |
| D (Debug redact) | Medium | Latent token leak in logs | Low |
| E (token validate) | Medium | Silent auth failure | Low |
| C (@ guard) | Low | Theoretical mis-auth | Low |
| F (owner_repo) | Low | Theoretical wrong split | Low |
| G (skipped metric) | Low | Cosmetic metric inflation | Low |

---

## 5. What's Already Fixed (No Action Needed)

| Issue | Fix | Where |
|---|---|---|
| Hardcoded GitHub clone_url in internal.rs | Platform-aware dispatch via `CodePlatform` trait | `internal.rs:148-186` |
| Double-scheme in GitLab clone_url | Strip protocol from base | `gitlab.rs:494` |
| Runner assumed GitHub basic auth | `@` detection in `authenticated_clone_url()` | `client.rs:122-130` |
| Stale repo dir on retry | `remove_dir_all` before `create_dir_all` | `clone.rs:18-24` |
| Empty GitHub secret forges webhooks | Fail-closed on empty secret | `webhook.rs:1155` |
| `review_feedback.github_comment_id` not renamed | Merged into migration 0024 | `0024_platform_abstraction.sql` |
| Webhook secret read from env per call | Cached in `GitlabClient` struct | `gitlab.rs:from_env()` |
| `post_comment`/`add_reaction` fall back on any error | Only fall back on 404 | `gitlab.rs` |
| `list_review_comments` missing pagination | Added `per_page=100` | `gitlab.rs` |
| Bot detection missed triggerer username | Also check `payload["user"]["username"]` | `webhook.rs` |
| Migration used DROP INDEX on constraint | Changed to DROP CONSTRAINT | `0024_platform_abstraction.sql` |