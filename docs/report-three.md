# Review-3 Report — Credibility Analysis & Implementation Plan

## Summary

This report evaluates 16 findings from `review-3.txt` against the actual source code. Each finding is classified as **Credible** or **Not credible**, with a fix plan for credible findings.

---

## P1 Findings

### P1 #1 — `types.rs:38` field rename breaks API JSON contract — **CREDIBLE**

**Finding:** The `Task` struct field `github_delivery_id` was renamed to `webhook_delivery_id`, breaking the JSON contract for frontend/CLI clients.

**Verification:** The `types.rs::Task` struct is scaffold code (`#![allow(dead_code)]`, not used anywhere). The **actual** API contract is `db.rs::TaskRow` at line 1070, which has `webhook_delivery_id: Option<String>` — serialized to JSON for the frontend. The frontend TypeScript interface at `apps/web/lib/domain/tasks.ts:12` was updated to use `webhook_delivery_id: string | null`, and `apps/web/app/dashboard/runs/[id]/page.tsx:120` now reads `task.webhook_delivery_id`.

**Impact:** The dashboard run-detail page will show "—" for the delivery field if the frontend is not updated.

**Fix:** This is a controlled breaking change. The frontend was updated to use `webhook_delivery_id`. The backend no longer uses `#[serde(alias)]` for backward compatibility.

---

### P1 #2 — GitHub token cache removed, N mints per drain — **CREDIBLE**

**Finding:** The old reconciler had a per-batch token cache. The refactor removed it. `GithubApp` has no cache field. `deliver_review` calls `post_review` + `list_review_comments` + `add_labels` = 3 token mints per review row.

**Verification:** `GithubApp` at `github.rs:29` has only `app_id`, `key`, `http` — no cache. `installation_token()` at line 79 makes a fresh API call every time (mints App JWT → exchanges for installation token). A token cache was previously implemented on a separate branch but user wants it in THIS branch.

**Fix:** Re-apply the token cache: `CachedToken` struct, `TOKEN_CACHE_TTL_SECS = 50 * 60`, `token_cache: Arc<Mutex<HashMap<i64, CachedToken>>>` field on `GithubApp`, `cached_token()` helper, `installation_token()` checks cache first.

---

## P2 Findings

### P2 #3 — `owner_repo` splits on first / — **CREDIBLE**

`platform.rs:74` uses `split_once('/')` — wrong for nested GitLab groups (`group/sub/repo` → `("group", "sub/repo")`). Fix: use `rsplit_once('/')`.

### P2 #4 — `outbox_delivery` inflates posted on silently-skipped rows — **CREDIBLE**

`failure_notice` dedup gate silently consumes the row and marks it `posted`, inflating the metric. Fix: add `"skipped"` outcome to `outbox_delivery` metric.

### P2 #5 — GitLab token exposed via `TaskContextResponse` Debug derive — **CREDIBLE**

`TaskContextResponse` derives `Debug`. For GitLab, `clone_url` contains `oauth2:{token}@host`. A `tracing::debug!(?response)` would leak the token. Fix: custom Debug impl that redacts `clone_url`.

### P2 #6 — `api_headers` silently drops invalid `PRIVATE-TOKEN` — **CREDIBLE**

`gitlab.rs:77-83` uses `if let Ok(v) = HeaderValue::from_str(&self.token)` — silently skips the header on error. Fix: validate token in `from_env()` instead.

### P2 #7 — `clone_url` doesn't URL-encode token — **CREDIBLE BUT LOW RISK, DEFER**

GitLab tokens are typically URL-safe (alphanumeric + dashes). Defer to a follow-up.

### P2 #8 — `clone_url` host parse breaks with double-slash API URL — **CREDIBLE BUT LOW RISK**

`strip_suffix("/api/v4")` is case-sensitive. If `GITLAB_API_URL` has `//api/v4`, the strip fails. Fix: normalize URL or use a more robust parser. Low priority.

### P2 #9 — `webhook_deliveries` dedup PK not composite with platform — **NOT CREDIBLE**

GitHub and GitLab both use UUIDs for delivery IDs. Collision probability in a 2^128 space is astronomically low. Not worth fixing.

### P2 #10 — Old outbox rows lacking `target_type` default to MR — **CREDIBLE, LOW RISK**

Old in-flight rows from before deploy won't have `target_type` in payload. GitLab defaults to MR. MR is more common than issue for review tasks, so the default is safe. Document as known limitation.

### P2 #11 — Migration assumes atomic schema+code deploy — **CREDIBLE BUT OPERATIONAL**

`RENAME COLUMN` is not backward-compatible during rolling updates. This is an operational concern, not a code bug. Document in the ADR.

### P2 #12 — `list_pollable_comments` includes GitLab comments (no-op poll) — **CREDIBLE BUT MINOR**

GitLab comments are polled every cycle with no-op result. Minor waste. Fix: filter to skip GitLab comments until feedback polling is implemented. Defer.

### P2 #13 — Metric label change may require dashboard updates — **NOT A BUG**

Operational note for Grafana panels. Not a code bug.

### P2 #14 — Misleading `#[allow(dead_code)]` on used platform field — **CREDIBLE, EASY FIX**

`db.rs:541` `OutboxRow.platform` and `db.rs:704` `PollableComment.platform` have `#[allow(dead_code)]` but ARE used in the reconciler. Fix: remove the `#[allow(dead_code)]` attributes.

### P2 #15 — ADR "zero behavior change" contradicts API field rename — **CREDIBLE**

ADR-0072 claims "phases 0–3 are pure refactors with zero behavior change for GitHub" but `TaskRow.webhook_delivery_id` is a wire-format break. Fix: add `#[serde(alias = "github_delivery_id")]` (fixes P1 #1) and update the ADR to acknowledge the wire-format change.

### P2 #16 — README ADR-0072 summary repeats "zero behavior change" — **CREDIBLE**

Same as #15 but in `docs/adr/README.md`. Fix: update the README summary.

---

## Implementation Plan

### Phase 1 — P1 fixes (must do before merge)

| # | Fix | Files |
|---|---|---|
| P1 #1 | Add `#[serde(alias = "github_delivery_id")]` to `TaskRow.webhook_delivery_id` | `db.rs` |
| P1 #2 | Re-apply token cache on `GithubApp` | `github.rs` |

### Phase 2 — P2 fixes (should do before merge)

| # | Fix | Files |
|---|---|---|
| P2 #3 | `owner_repo()` use `rsplit_once('/')` | `platform.rs` |
| P2 #4 | Add `"skipped"` outcome to `outbox_delivery` metric | `metrics.rs`, `reconciler.rs` |
| P2 #5 | Custom Debug for `TaskContextResponse` that redacts `clone_url` | `internal.rs` |
| P2 #6 | Validate token in `GitlabClient::from_env()` | `gitlab.rs` |
| P2 #14 | Remove `#[allow(dead_code)]` from `OutboxRow.platform` and `PollableComment.platform` | `db.rs` |
| P2 #15 | Update ADR-0072 to acknowledge wire-format change | `0072-platform-abstraction-layer.md` |
| P2 #16 | Update README ADR-0072 summary | `docs/adr/README.md` |

### Phase 3 — Deferred (follow-up PRs)

| # | Finding | Reason |
|---|---|---|
| P2 #7 | URL-encode token in `clone_url` | Low risk — GitLab tokens are URL-safe |
| P2 #8 | Double-slash API URL normalization | Low risk — operators set correct URLs |
| P2 #10 | Old outbox rows lacking `target_type` | Low risk — MR default is safe |
| P2 #11 | Migration atomic deploy | Operational concern — document in ADR |
| P2 #12 | GitLab no-op feedback polling | Minor waste — defer until feedback polling is implemented |

### Not credible (no action)

| # | Finding | Reason |
|---|---|---|
| P2 #9 | `webhook_deliveries` PK not composite | UUID collision probability is negligible |
| P2 #13 | Metric label change for dashboards | Operational note, not a code bug |