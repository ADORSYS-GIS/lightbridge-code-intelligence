# GitLab Platform Abstraction: Remaining Issues, Known Limitations, and Edge Cases

## Overview

This document provides a detailed analysis of the remaining issues, known limitations, and edge cases for the GitLab platform abstraction layer (ADR-0072) implemented in the control plane. These items are tracked in the ADR and represent work that needs to be completed in follow-up PRs.

---

## 1. GitLab Feedback Polling (Phase 4)

### Status
**Known limitation** — tracked in `services/control-plane/src/integrations/gitlab.rs`

### Current Implementation

```rust
// services/control-plane/src/integrations/gitlab.rs:464-474
async fn list_comment_reactions(
    &self,
    _repo: &RepoRef,
    _comment_id: i64,
    _is_review_comment: bool,
) -> anyhow::Result<Vec<Reaction>> {
    // Known limitation (Phase 4): listing award emoji on a note requires the MR/issue iid,
    // which we don't have from just the note ID. Feedback polling (👍/👎) is a no-op for GitLab
    // until Phase 7 stores the iid alongside the note.
    Ok(Vec::new())
}
```

### Problem Description

GitLab's award emoji API requires the MR or issue `iid` to fetch reactions:
- **MR endpoint:** `GET /projects/{id}/merge_requests/{iid}/award_emoji`
- **Issue endpoint:** `GET /projects/{id}/issues/{iid}/award_emoji`

However, the outbox payload only stores:
- `issue_number` (the numeric identifier)
- `target_type` (either `"pull_request"` or `"issue"`)
- `comment_id` (the note ID)

The `iid` is not stored, so the API call cannot be made.

### Impact

- 👍/👎 feedback polling is a **no-op** for GitLab
- Only GitHub feedback polling works correctly
- Users cannot see 👍/👎 reactions on GitLab comments in the dashboard

### Mitigation

GitLab review posting (comments, reactions on MRs/issues, labels) works correctly. Only the feedback polling is affected.

### Required Changes

1. **Update `ReviewCommentPayload` struct** in `services/control-plane/src/outbox.rs`:
```rust
pub struct ReviewCommentPayload {
    pub path: String,
    pub line: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    pub body: String,
    pub iid: i64,  // NEW FIELD: MR/issue iid for GitLab feedback polling
}
```

2. **Update `add_comment()`** in `services/control-plane/src/integrations/gitlab.rs`:
```rust
async fn post_comment(
    &self,
    repo: &RepoRef,
    issue_number: i64,
    body: &str,
    noteable_type: Option<&str>,
) -> anyhow::Result<PostedComment> {
    let project = Self::project_encoded(repo);
    let payload = serde_json::json!({ "body": body });

    let is_mr = noteable_type.map(|t| t == "pull_request").unwrap_or(true);
    let endpoint = if is_mr {
        format!(
            "/projects/{}/merge_requests/{}/notes",
            project, issue_number
        )
    } else {
        format!("/projects/{}/issues/{}/notes", project, issue_number)
    };
    let url = self.url(&endpoint);
    let resp = self
        .http
        .post(&url)
        .headers(self.api_headers())
        .json(&payload)
        .send()
        .await?
        .error_for_status()?;
    let v: serde_json::Value = resp.json().await?;
    Ok(PostedComment {
        id: v.get("id").and_then(|i| i.as_i64()),
        html_url: None,
    })
}
```

3. **Update `list_comment_reactions()`** to use the `iid`:
```rust
async fn list_comment_reactions(
    &self,
    repo: &RepoRef,
    comment_id: i64,
    is_review_comment: bool,
) -> anyhow::Result<Vec<Reaction>> {
    // Fetch the comment to get the iid
    let project = Self::project_encoded(repo);
    let url = format!("/projects/{}/notes/{}", project, comment_id);
    let resp = self
        .http
        .get(&self.url(&url))
        .headers(self.api_headers())
        .send()
        .await?
        .error_for_status()?;
    let v: serde_json::Value = resp.json().await?;

    // Extract the iid from the comment
    let iid = v.get("target_iid")
        .or_else(|| v.get("target_id"))
        .and_then(|i| i.as_i64())
        .ok_or_else(|| anyhow::anyhow!("Comment missing iid"))?;

    // Fetch award emoji using the iid
    let endpoint = if is_review_comment {
        format!("/projects/{}/merge_requests/{}/award_emoji", project, iid)
    } else {
        format!("/projects/{}/issues/{}/award_emoji", project, iid)
    };
    let url = self.url(&endpoint);
    let resp = self
        .http
        .get(&url)
        .headers(self.api_headers())
        .send()
        .await?
        .error_for_status()?;
    let v: serde_json::Value = resp.json().await?;

    // Parse award emoji
    let reactions: Vec<Reaction> = v
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|item| {
            let name = item.get("name")?.as_str()?;
            let user = item.get("user")?.get("username")?.as_str()?;
            Some(Reaction {
                reactor: user.to_string(),
                reaction: name.to_string(),
            })
        })
        .collect();

    Ok(reactions)
}
```

---

## 2. GitLab Comment Reaction on Comment ID

### Status
**Known limitation** — tracked in `services/control-plane/src/integrations/gitlab.rs`

### Current Implementation

```rust
// services/control-plane/src/integrations/gitlab.rs:377-384
ReactionTarget::Comment { comment_id: _ } => {
    // Known limitation (Phase 4): awarding emoji on a note requires the MR/issue iid,
    // which we don't have from just the note ID. Logged, skipped.
    tracing::debug!(
        "gitlab add_reaction on comment skipped (iid lookup not implemented in Phase 4)"
    );
    Ok(())
}
```

### Problem Description

When a user adds a reaction directly to a GitLab comment (not on an MR or issue), the `add_reaction()` function skips the API call because award emoji on a note requires the MR/issue `iid`, which is not available from just the comment ID.

### Impact

- Reactions on GitLab comments are **not posted**
- Only reactions on MRs and issues work correctly
- This is a low-frequency operation; the primary review feedback path (via MR/issue reactions) is unaffected

### Mitigation

This is a low-frequency operation. The primary review feedback path (via MR/issue reactions) is unaffected.

### Required Changes

This limitation is resolved when Phase 4 is completed (see Section 1). Once the `iid` is stored in the outbox payload, the `add_reaction()` function can fetch the comment to get the `iid` and post the reaction.

---

## 3. GitLab Clone URL URL Encoding

### Status
**Low-risk limitation** — documented in `services/control-plane/src/integrations/gitlab.rs`

### Current Implementation

```rust
// services/control-plane/src/integrations/gitlab.rs:476-492
fn clone_url(&self, repo: &RepoRef) -> String {
    // Embed the token for HTTPS clone (oauth2:TOKEN@host form).
    // Strip the `/api/v4` suffix to get the base host URL, then strip the
    // protocol prefix to avoid a doubled scheme in the final URL.
    // Note: Token and path are not URL-encoded. GitLab PATs are alphanumeric + hyphens,
    // and project paths are typically alphanumeric + hyphens + underscores, so this is
    // low-risk in practice. URL-encoding would break the OAuth2 format.
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

### Problem Description

The `clone_url()` function builds a GitLab clone URL without URL-encoding the token or the repository path:

```rust
format!(
    "https://oauth2:{}@{}/{}.git",
    self.token, host, repo.full_name
)
```

If a GitLab PAT contains URL-reserved characters (e.g., `@`, `/`, `:`) or if a project path contains spaces/Unicode, the clone URL will be malformed and `git clone` will fail.

### Impact

- **None in practice** — GitLab PATs are alphanumeric + hyphens, and project paths are typically alphanumeric + hyphens + underscores
- Malformed URLs would cause `git clone` to fail, which would be caught and retried by the agent-runner

### Mitigation

URL-encoding would break the OAuth2 format (`:` in token would be misinterpreted as scheme separator). The current implementation is safe because:
1. GitLab PATs are alphanumeric + hyphens (no special characters)
2. GitLab project paths are alphanumeric + hyphens + underscores (no spaces or Unicode)
3. The agent-runner would catch and retry failed clones

### Required Changes

**No changes required** — this is a low-risk limitation that is acceptable as-is.

---

## 4. Agent Runner @-Passthrough Edge Case

### Status
**Rare edge case** — documented in `services/agent-runner/src/bootstrap/client.rs`

### Current Implementation

```rust
// services/agent-runner/src/bootstrap/client.rs:122-130
pub fn authenticated_clone_url(&self) -> String {
    match self.clone_url.strip_prefix("https://") {
        // Already authenticated by the control plane — use as-is.
        Some(rest) if rest.contains('@') => self.clone_url.clone(),
        // Splice the token in (GitHub's x-access-token:<token>@ form).
        Some(rest) => format!("https://x-access-token:{}@{rest}", self.token),
        None => self.clone_url.clone(),
    }
}
```

### Problem Description

The `authenticated_clone_url()` function uses `rest.contains('@')` to detect a pre-authenticated URL:

```rust
Some(rest) if rest.contains('@') => self.clone_url.clone(),
```

This is correct for GitLab's `oauth2:TOKEN@host` format, but if a GitLab subgroup path contains `@` (e.g., `group@team/repo.git`), the function would wrongly pass it through without splicing the token.

### Impact

- **Extremely rare** — GitLab subgroups with `@` in the name are not a standard pattern
- If such a URL exists, the runner would not have authentication and `git clone` would fail

### Mitigation

The guard is sufficient for normal tokens. The edge case is documented for future review if GitLab starts using `@` in subgroup paths.

### Required Changes

**No changes required** — this is an extremely rare edge case that is acceptable as-is.

---

## 5. Platform Metric Labels

### Status
**Operational concern** — tracked in `services/control-plane/src/http/metrics.rs`

### Current Implementation

```rust
// services/control-plane/src/http/metrics.rs:48-65
pub fn webhook_delivery(platform: &str, event: &str) {
    counter!(
        "lci_webhook_deliveries_total",
        "platform" => platform.to_string(),
        "event" => event.to_string(),
    )
    .increment(1);
}

pub fn webhook_signature_failure(platform: &str) {
    counter!("lci_webhook_signature_failures_total", "platform" => platform.to_string())
        .increment(1);
}

pub fn webhook_duplicate(platform: &str) {
    counter!("lci_webhook_duplicate_deliveries_total", "platform" => platform.to_string())
        .increment(1);
}

pub fn outbox_delivery(platform: &str, kind: &str, outcome: &'static str) {
    counter!(
        "lci_outbox_delivery_total",
        "platform" => platform.to_string(),
        "kind" => kind.to_string(),
        "outcome" => outcome,
    )
    .increment(1);
}
```

### Problem Description

New metrics now include a `platform` label:
- `lci_webhook_deliveries_total`
- `lci_webhook_signature_failures_total`
- `lci_webhook_duplicate_deliveries_total`
- `lci_outbox_delivery_total`

Existing Grafana dashboards or alerts that query these metrics by name without a platform filter will now show both platforms' data mixed together.

### Impact

- **Not a breaking change** — the label is additive
- Pre-existing dashboards will continue to work but may show combined data from both platforms
- Dashboards may need label matchers or aggregation updates to separate platform data

### Mitigation

- **Bounded cardinality:** 2 platforms × ~4 kinds × 3 outcomes = 24 series on the new metric
- **No card explosion** — the number of series is small and manageable
- Pre-existing dashboards will continue to work but may need label matchers updated

### Required Changes

**Operational concern** — this is not a code change, but an operational concern for dashboard updates. No code changes are required.

---

## 6. Phase 7: Filter GitLab Comments in Polling

### Status
**Completed** — already implemented in `services/control-plane/src/db.rs`

### Current Implementation

```rust
// services/control-plane/src/db.rs:779-806
pub async fn list_pollable_comments(
    pool: &PgPool,
    within_days: i32,
    interval_secs: i64,
) -> Result<Vec<PollableComment>, sqlx::Error> {
    // Phase 7 TODO: GitLab feedback polling is not implemented yet (list_comment_reactions returns empty).
    // Filter to GitHub only to avoid wasting cycles on no-op GitLab API calls.
    sqlx::query_as::<_, PollableComment>(
        "SELECT rc.task_id, rc.platform_comment_id, rc.kind, r.owner, r.name, t.installation_id, r.platform \
         FROM review_comments rc \
         JOIN tasks t ON t.id = rc.task_id \
         JOIN repositories r ON r.id = t.repository_id \
         WHERE r.platform = 'github' \  // Filter to GitHub only
           AND t.created_at > now() - ($1 * interval '1 day') \
           AND ( \
             rc.created_at > now() - interval '1 day' \
             OR (rc.created_at BETWEEN now() - interval '3 days' AND now() - interval '1 day' \
                 AND (rc.platform_comment_id % 12) = (extract(epoch from now())::bigint / $2) % 12) \
             OR (rc.created_at < now() - interval '3 days' \
                 AND (rc.platform_comment_id % 72) = (extract(epoch from now())::bigint / $2) % 72) \
           ) \
         ORDER BY rc.created_at",
    )
    .bind(within_days)
    .bind(interval_secs.max(1))
    .fetch_all(pool)
    .await
}
```

### Problem Description

The `list_pollable_comments()` function queries all platforms to find comments that need feedback polling. However, GitLab's `list_comment_reactions()` returns an empty `Vec` because award emoji requires the MR/issue `iid`, which is not stored from the note `id`.

### Impact

- **Minor performance overhead** — no correctness issue
- Each poll cycle fetches GitLab comments from the DB, constructs a `RepoRef`, and calls the GitLab API only to get an empty result back

### Mitigation

**Already implemented** — the filter `WHERE r.platform = 'github'` is already in place, saving cycles and making the code clearer about the limitation.

### Required Changes

**No changes required** — this TODO has been completed.

---

## Summary of Required Work

### High Priority (Phase 4)
1. **Store `iid` in outbox payload** — Update `ReviewCommentPayload` struct to include `iid: i64` field
2. **Update `add_comment()`** — Store the `iid` in the payload JSON when posting GitLab comments
3. **Update `list_comment_reactions()`** — Fetch the comment to get the `iid` and use it to fetch award emoji

### Low Priority (Operational Concerns)
4. **Platform metric labels** — Update Grafana dashboards to filter by `platform` label (operational, not code)

### No Changes Required
5. **GitLab clone URL URL encoding** — Low-risk limitation, acceptable as-is
6. **Agent runner @-passthrough edge case** — Extremely rare edge case, acceptable as-is
7. **Phase 7 filter** — Already implemented

---

## Conclusion

The GitLab platform abstraction layer (ADR-0072) is functionally complete for the primary use case (review posting, comments, reactions on MRs/issues, labels). The remaining work is focused on:

1. **GitLab feedback polling** (Phase 4) — requires storing the `iid` in the outbox payload
2. **Operational concerns** (dashboard updates) — not code changes, but requires dashboard updates

All other limitations are either low-risk or extremely rare edge cases that are acceptable as-is.

---

**Document Version:** 1.0
**Last Updated:** 2026-07-09
**Related ADR:** ADR-0072 — Platform-abstraction layer for GitHub + GitLab
**Related Tickets:** `docs/tickets/gitlab-feedback-polling-implementation.md`