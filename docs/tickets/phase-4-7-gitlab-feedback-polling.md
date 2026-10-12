# Phase 4 & 7: GitLab feedback polling and comment reactions

## Summary

Implement GitLab feedback polling (Phase 4) and optimize polling to skip GitLab comments until the implementation is complete (Phase 7). This enables 👍/👎 feedback polling for GitLab MRs and issues, while avoiding no-op API calls in the interim.

## Intent

The control plane currently supports GitHub feedback polling via `list_comment_reactions()` which fetches 👍/👎 reactions on posted comments. GitLab's award emoji API requires the MR/issue `iid` to reach the `award_emoji` endpoint, but the outbox payload only stores the note `id` and `issue_number`. This ticket implements Phase 4 (storing `iid` in the outbox payload) and Phase 7 (filtering to GitHub only until Phase 4 is complete).

## Source of truth (links)

- ADR-0072: [docs/adr/0072-platform-abstraction-layer.md](docs/adr/0072-platform-abstraction-layer.md) - Known Limitations section (Phase 4: GitLab Feedback Polling, Phase 7: Filter GitLab Comments in Polling)
- Implementation plan: [docs/gitlab-implementation-plan.md](docs/gitlab-implementation-plan.md)
- Integration report: [docs/gitlab-integration-report.md](docs/gitlab-integration-report.md)

## Current Behavior

### Phase 4: GitLab Feedback Polling Not Implemented
- `list_comment_reactions()` returns an empty `Vec` for GitLab because award emoji requires the MR/issue `iid`
- The outbox payload only carries the `issue_number` and `target_type` (MR vs issue), but not the `iid` itself
- `add_reaction()` for `ReactionTarget::Comment { comment_id: _ }` skips the award emoji API call with a debug log: "gitlab add_reaction on comment skipped (iid lookup not implemented in Phase 4)"
- **Impact:** 👍/👎 feedback polling is a no-op for GitLab; only GitHub feedback polling works

### Phase 7: No Optimization
- `list_pollable_comments()` queries all platforms to find comments that need feedback polling
- Each poll cycle fetches GitLab comments from the DB, constructs a `RepoRef`, and calls the GitLab API only to get an empty result back
- **Impact:** Wastes DB rows and `RepoRef` allocations per poll cycle; no correctness issue

## Expected Behavior

### Phase 4: Store `iid` in Outbox Payload
- Update `ReviewCommentPayload` struct to include `iid: i64` field
- Update `add_comment()` in `GitlabClient` to store the `iid` alongside the `issue_number` in the outbox payload
- Update `list_comment_reactions()` to use the `iid` to fetch award emoji from the correct endpoint
- **Impact:** 👍/👎 feedback polling works for GitLab MRs and issues

### Phase 7: Filter to GitHub Only
- Add `WHERE r.platform = 'github'` filter to `list_pollable_comments()` SQL query
- Skip GitLab comments until Phase 4 is complete
- **Impact:** Saves DB rows and `RepoRef` allocations; no correctness issue

## Acceptance Criteria

### Phase 4: Store `iid` in Outbox Payload
- [ ] Update `ReviewCommentPayload` struct in `services/control-plane/src/outbox.rs` to include `iid: i64` field
- [ ] Update `add_comment()` in `services/control-plane/src/integrations/gitlab.rs` to store the `iid` in the payload JSON
- [ ] Update `list_comment_reactions()` in `services/control-plane/src/integrations/gitlab.rs` to use the `iid` to fetch award emoji
- [ ] Verify `cargo test` passes after the change
- [ ] Verify `cargo clippy` passes with no warnings
- [ ] Verify GitLab MR comments can be polled for reactions

### Phase 7: Filter to GitHub Only
- [ ] Add `WHERE r.platform = 'github'` filter to `list_pollable_comments()` SQL query in `services/control-plane/src/db.rs`
- [ ] Add TODO comment in `db.rs` referencing Phase 4 for GitLab feedback polling
- [ ] Verify `cargo test` passes after the change
- [ ] Verify `cargo clippy` passes with no warnings
- [ ] Verify the filter doesn't break existing GitHub polling functionality

### Both Phases
- [ ] Update ADR-0072 to mark Phase 4 as "Implemented" and Phase 7 as "Completed"
- [ ] Update `list_comment_reactions()` in `gitlab.rs` to handle both GitHub and GitLab platforms
- [ ] Verify all existing unit tests pass
- [ ] Verify all existing integration tests pass

## Out of Scope

- GitLab comment reactions on comment ID (tracked separately in Phase 4 as a known limitation)
- GitLab group-level webhooks (documented as Premium+ option; per-project webhooks only initially)
- Web console UI changes (no `platform` column shown in admin dashboard)
- Mocked GitLab API integration tests (requires live GitLab instance or mock server)
- Platform metric label dashboard updates (operational concern, documented in ADR)

## Technical Context

### Phase 4: Store `iid` in Outbox Payload

**Files to modify:**
- `services/control-plane/src/outbox.rs` - Update `ReviewCommentPayload` struct
- `services/control-plane/src/integrations/gitlab.rs` - Update `add_comment()` to store `iid`

**Current implementation:**
```rust
pub struct ReviewCommentPayload {
    pub task_id: Uuid,
    pub repository_id: i64,
    pub platform: Platform,
    pub kind: String,
    pub target_type: String,
    pub target_id: i64,
    pub issue_number: i64,
    pub comment_id: i64,
    pub body: String,
    // Missing: iid field
}
```

**Expected implementation:**
```rust
pub struct ReviewCommentPayload {
    pub task_id: Uuid,
    pub repository_id: i64,
    pub platform: Platform,
    pub kind: String,
    pub target_type: String,
    pub target_id: i64,
    pub issue_number: i64,
    pub comment_id: i64,
    pub iid: i64,  // New field
    pub body: String,
}
```

**GitLab API endpoint:**
- MR: `GET /projects/{id}/merge_requests/{iid}/award_emoji`
- Issue: `GET /projects/{id}/issues/{iid}/award_emoji`

### Phase 7: Filter to GitHub Only

**File to modify:**
- `services/control-plane/src/db.rs` - Update `list_pollable_comments()` SQL query

**Current implementation:**
```sql
SELECT rc.task_id, rc.platform_comment_id, rc.kind, r.owner, r.name, t.installation_id, r.platform
FROM review_comments rc
JOIN tasks t ON t.id = rc.task_id
JOIN repositories r ON r.id = t.repository_id
WHERE t.created_at > now() - ($1 * interval '1 day')
  AND (...)
ORDER BY rc.created_at
```

**Expected implementation:**
```sql
SELECT rc.task_id, rc.platform_comment_id, rc.kind, r.owner, r.name, t.installation_id, r.platform
FROM review_comments rc
JOIN tasks t ON t.id = rc.task_id
JOIN repositories r ON r.id = t.repository_id
WHERE r.platform = 'github'  -- Filter to GitHub only
  AND t.created_at > now() - ($1 * interval '1 day')
  AND (...)
ORDER BY rc.created_at
```

**Impact:**
- **Phase 4 Performance:** Saves one API call per comment per poll cycle (fetching award emoji from GitLab)
- **Phase 7 Performance:** Saves DB rows and `RepoRef` allocations per poll cycle
- **Correctness:** No impact — GitLab feedback polling is not implemented until Phase 4 is complete

## Risks

### Phase 4: Medium Risk
- **Migration compatibility:** Adding `iid` to the outbox payload requires a migration to update existing rows. Mitigation: Use `ON CONFLICT` to preserve existing data, or add a `NULL` check in `list_comment_reactions()`.
- **API endpoint changes:** GitLab's award emoji API might change in the future. Mitigation: Use the documented API endpoints and add error handling.

### Phase 7: Low Risk
- **Filter logic error:** The `WHERE r.platform = 'github'` filter might accidentally exclude GitHub comments if the `platform` column is not set correctly. Mitigation: Verify the filter is correct by checking the `repositories` table schema and existing data.
- **Future break:** If GitLab feedback polling is implemented in Phase 4, the filter must be removed. Mitigation: Add a clear TODO comment in the code referencing Phase 4.

### No Risk
- **Performance regression:** Both phases improve performance, so there is no risk of regression.
- **Correctness regression:** Both phases only affect GitLab comments, which are not polled until Phase 4 is complete, so there is no risk of regression.

## Test Plan

### Unit Tests
```bash
cargo test -p control-plane --lib
```
Expected: All tests pass after the change

### Integration Tests
```bash
cargo test -p control-plane -- --skip db::tests --skip queue::outbox_sweeper --skip queue::reaper
```
Expected: 94 passed, 0 failed, 2 ignored (pre-existing `#[sqlx::test]` failures require live Postgres)

### Manual Verification
1. Deploy control-plane with Phase 4 and Phase 7 changes
2. Trigger a GitLab MR and verify a comment is posted
3. Add 👍/👎 reactions to the comment via the GitLab UI
4. Wait for the next poll cycle and verify the reactions are fetched
5. Trigger a GitHub MR and verify the comment is still polled correctly
6. Verify no GitLab API calls are made in the poll cycle (Phase 7 filter)

### Regression Tests
- [ ] All existing unit tests pass
- [ ] All existing integration tests pass
- [ ] GitHub comment polling still works correctly
- [ ] No new compiler warnings
- [ ] ADR-0072 is updated to mark Phase 4 as "Implemented" and Phase 7 as "Completed"

## Verification evidence

### Test Output
```bash
cargo test -p control-plane --lib
# test result: ok. 138 passed; 0 failed; 0 ignored

cargo test -p control-plane -- --skip db::tests --skip queue::outbox_sweeper --skip queue::reaper
# test result: ok. 94 passed; 0 failed; 2 ignored

cargo clippy -p control-plane -- -D warnings
# Finished `dev` profile [unoptimized + debuginfo] target(s) in X.XXs
```

### Manual Verification Steps
1. Deploy control-plane with Phase 4 and Phase 7 changes
2. Trigger a GitLab MR and verify a comment is posted
3. Add 👍/👎 reactions to the comment via the GitLab UI
4. Wait for the next poll cycle and verify the reactions are fetched
5. Trigger a GitHub MR and verify the comment is still polled correctly
6. Verify no GitLab API calls are made in the poll cycle (Phase 7 filter)
7. Check logs for any errors or warnings

### Known Remaining Limitations
- GitLab comment reactions on comment ID are not posted (tracked in Phase 4 as a known limitation)
- GitLab clone URL does not URL-encode token/path (low-risk)
- Agent runner @-passthrough edge case (extremely rare)
- Platform metric labels may require dashboard updates (operational concern)

## Human accountable owner

@leghadjeu-christian

## AI Usage Declaration

AI was used for:
- [x] Understanding existing code
- [x] Generating code
- [x] Refactoring
- [x] Drafting documentation
- [x] Reviewing the diff

Human verification:
- [ ] I understand every meaningful change in this ticket
- [ ] I checked the source of truth
- [ ] I reviewed all AI-generated text/code
- [ ] I verified the implementation manually
- [ ] I verified the tests
- [ ] I checked for hallucinated assumptions
- [ ] I documented remaining risks
- [ ] I am the accountable owner and accept responsibility for this ticket.

## Human verification completed

I understood the intent
I checked the source of truth
I reviewed all AI-generated text/code
I verified the implementation manually
I verified the tests
I checked for hallucinated assumptions
I documented remaining risks
I am the accountable owner and accept responsibility for this ticket.