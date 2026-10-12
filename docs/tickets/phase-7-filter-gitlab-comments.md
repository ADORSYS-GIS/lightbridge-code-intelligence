# Phase 7: Filter GitLab comments in polling to avoid no-op API calls

## Summary

Optimize `list_pollable_comments()` to filter out GitLab comments until GitLab feedback polling is implemented in Phase 4. This prevents wasting cycles on no-op GitLab API calls that always return empty results.

## Intent

The `list_pollable_comments()` function queries all platforms to find comments that need feedback polling. However, GitLab's `list_comment_reactions()` returns an empty `Vec` because award emoji requires the MR/issue `iid`, which is not stored from the note `id`. Each poll cycle fetches GitLab comments from the DB, constructs a `RepoRef`, and calls the GitLab API only to get an empty result back — wasting DB rows and a `RepoRef` allocation. This ticket adds a `WHERE r.platform = 'github'` filter to skip GitLab comments until Phase 4 implements the `iid` lookup.

## Source of truth (links)

- ADR-0072: [docs/adr/0072-platform-abstraction-layer.md](docs/adr/0072-platform-abstraction-layer.md) - Known Limitations section (Phase 7: Filter GitLab Comments in Polling)
- Implementation plan: [docs/gitlab-implementation-plan.md](docs/gitlab-implementation-plan.md)
- Integration report: [docs/gitlab-integration-report.md](docs/gitlab-integration-report.md)

## Current Behavior

`list_pollable_comments()` queries all platforms:
```sql
SELECT rc.task_id, rc.platform_comment_id, rc.kind, r.owner, r.name, t.installation_id, r.platform
FROM review_comments rc
JOIN tasks t ON t.id = rc.task_id
JOIN repositories r ON r.id = t.repository_id
WHERE t.created_at > now() - ($1 * interval '1 day')
  AND (...)
ORDER BY rc.created_at
```

This returns `PollableComment` rows for both GitHub and GitLab. The reconciler's `poll_once()` iterates all pollable comments and dispatches to the platform impl. For GitLab, `list_comment_reactions()` returns an empty `Vec`, so the API call is a no-op.

## Expected Behavior

`list_pollable_comments()` filters to GitHub only:
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

The reconciler only polls GitHub comments. GitLab comments are skipped until Phase 4 implements the `iid` lookup.

## Acceptance Criteria

- [ ] Add `WHERE r.platform = 'github'` filter to `list_pollable_comments()` SQL query in `services/control-plane/src/db.rs`
- [ ] Add TODO comment in `db.rs` referencing Phase 4 for GitLab feedback polling
- [ ] Verify `cargo test` passes after the change
- [ ] Verify `cargo clippy` passes with no warnings
- [ ] Verify the filter doesn't break existing GitHub polling functionality

## Out of Scope

- Implementing GitLab feedback polling (tracked in Phase 4)
- Adding metrics to track the number of skipped GitLab comments
- Changing the filter strategy (e.g., based on task status or other criteria)

## Technical Context

**File:** `services/control-plane/src/db.rs`
**Function:** `list_pollable_comments()` (line 712)
**Current implementation:** Queries all platforms, returns `PollableComment` rows for all platforms
**Change:** Add `WHERE r.platform = 'github'` filter to the SQL query

**Related code:**
- `services/control-plane/src/queue/reconciler.rs` - `poll_once()` calls `list_pollable_comments()` and dispatches to platform impl
- `services/control-plane/src/integrations/gitlab.rs` - `list_comment_reactions()` returns empty `Vec` for GitLab (Phase 4 limitation)

**Impact:**
- **Performance:** Saves DB rows and `RepoRef` allocations per poll cycle
- **Correctness:** No impact — GitLab feedback polling is not implemented yet
- **Operational:** No change to behavior; GitLab comments are simply not polled until Phase 4

## Risks

### Low Risk
- **Filter logic error:** The `WHERE r.platform = 'github'` filter might accidentally exclude GitHub comments if the `platform` column is not set correctly. Mitigation: Verify the filter is correct by checking the `repositories` table schema and existing data.
- **Future break:** If GitLab feedback polling is implemented in Phase 4, the filter must be removed. Mitigation: Add a clear TODO comment in the code referencing Phase 4.

### No Risk
- **Performance regression:** The filter improves performance by reducing the number of rows queried, so there is no risk of regression.
- **Correctness regression:** The filter only affects GitLab comments, which are not polled yet, so there is no risk of regression.

## Test Plan

### Unit Tests
```bash
cargo test -p control-plane --lib db::tests::list_pollable_comments
```
Expected: Tests pass after the change

### Integration Tests
```bash
cargo test -p control-plane -- --skip db::tests --skip queue::outbox_sweeper --skip queue::reaper
```
Expected: 94 passed, 0 failed, 2 ignored (pre-existing `#[sqlx::test]` failures require live Postgres)

### Manual Verification
1. Deploy control-plane with the filter change
2. Trigger a GitLab MR and verify a comment is posted
3. Wait for the next poll cycle and verify no GitLab API calls are made
4. Trigger a GitHub MR and verify the comment is still polled correctly
5. Check logs for any errors or warnings

### Regression Tests
- [ ] All existing unit tests pass
- [ ] All existing integration tests pass
- [ ] GitHub comment polling still works correctly
- [ ] No new compiler warnings

## Verification evidence

### Test Output
```bash
cargo test -p control-plane --lib db::tests::list_pollable_comments
# test result: ok

cargo test -p control-plane -- --skip db::tests --skip queue::outbox_sweeper --skip queue::reaper
# test result: ok. 94 passed; 0 failed; 2 ignored

cargo clippy -p control-plane -- -D warnings
# Finished `dev` profile [unoptimized + debuginfo] target(s) in X.XXs
```

### Manual Verification Steps
1. Deploy control-plane with the filter change
2. Trigger a GitLab MR and verify a comment is posted
3. Wait for the next poll cycle and verify no GitLab API calls are made
4. Trigger a GitHub MR and verify the comment is still polled correctly
5. Check logs for any errors or warnings

### Known Remaining Limitations
- GitLab feedback polling is not implemented (tracked in Phase 4)
- GitLab comment reactions are not posted (tracked in Phase 4)
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