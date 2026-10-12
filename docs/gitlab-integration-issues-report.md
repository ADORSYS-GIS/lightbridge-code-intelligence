# GitLab Integration Issues Report

## Issue 1: Reaction Not Posted on GitLab Comment

### Problem Description
On GitHub, when the configured @mention is called, a reaction (👀 eyes) is posted BEFORE the review is posted. On GitLab, the review is posted correctly, but the reaction on the comment is NOT made - the eyes 👀 are not seen, even though the review goes through.

### Root Cause Analysis

#### Order of Operations
The reaction and review are posted in separate outbox reconciliation cycles:

1. **Dispatcher** (`services/control-plane/src/queue/dispatcher.rs`):
   - Line 274: Task is launched
   - Line 290: 👀 reaction is enqueued via `enqueue_reaction()`
   - This happens BEFORE the review is posted

2. **Reconciler** (`services/control-plane/src/queue/reconciler.rs`):
   - Line 334: Review is posted via `platform.post_review()`
   - Line 243-249: Reaction is posted via `platform.add_reaction()` when processing a `reaction` outbox row

#### Outbox Row Ordering
The outbox rows are processed in the order they are claimed from the database. The reaction is enqueued AFTER the review is posted, so it appears in a separate reconciliation cycle. This means:
- **GitHub**: Reaction appears first, then review (correct order)
- **GitLab**: Review appears first, then reaction (incorrect order)

#### GitLab API for Comment Reactions
GitLab uses the `/award_emoji` endpoint to post reactions on MRs/issues:
- **MR reactions**: `POST /projects/{id}/merge_requests/{iid}/award_emoji`
- **Issue reactions**: `POST /projects/{id}/issues/{iid}/award_emoji`
- **Comment reactions**: Requires fetching the comment first to get the `iid`, then posting to the MR/issue endpoint

The `add_reaction()` function in `services/control-plane/src/integrations/gitlab.rs` (lines 342-406) correctly handles comment reactions by:
1. Fetching the comment via `/projects/{id}/notes/{comment_id}`
2. Extracting the `target_iid` or `target_id`
3. Posting the award emoji using the iid

### GitLab API Reference
- **Award Emoji API**: `POST /projects/{id}/merge_requests/{iid}/award_emoji`
  - Request body: `{"name": "eyes"}`
  - Returns: Award emoji object with `id`, `user`, `name`, `created_at`

### Recommended Fix
To ensure the reaction appears BEFORE the review on GitLab, we need to:
1. **Option A**: Post the reaction immediately after the review in the same reconciliation cycle
2. **Option B**: Ensure the reaction outbox row is inserted BEFORE the review outbox row
3. **Option C**: Post the reaction synchronously in the dispatcher before enqueuing the review

**Option A** is recommended because it ensures the reaction appears before the review on both platforms.

---

## Issue 2: Review Formatting Issue

### Problem Description
On GitHub, the review title is on a separate line below the tags (e.g., "Po", "correctness"), but on GitLab, the review title is at the same level as the tags, making it look incorrect.

### Root Cause Analysis

#### Review Body Formatting
The review body is formatted using `render_body()` or `render_fast_body()` in `services/control-plane/src/review.rs`:

1. **Inline comments** (line 635-652): The `inline_body()` function formats findings with:
   - Level badges (e.g., `![P1](https://img.shields.io/badge/P1-orange) ![correctness](https://img.shields.io/badge/correctness-blue)`)
   - Bold title (e.g., `**Extra closing brace causes syntax error**`)
   - Body text
   - Suggestion block (if applicable)
   - Resources block (if applicable)
   - Feedback footer

2. **Review body** (line 685-693): The `render_body()` function formats the review summary with:
   - Summary text
   - Deferred findings (if any)
   - Out-of-scope findings (if any)
   - AI governance disclosure

#### GitLab vs GitHub Markdown Rendering
The issue is that GitLab's markdown renderer may not handle the badge images and bold title in the same way as GitHub's renderer:

- **GitHub**: Renders badges and bold title on separate lines, with the title clearly below the badges
- **GitLab**: Renders badges and bold title on the same line, making the title appear at the same level as the badges

#### GitLab API Reference
GitLab supports standard Markdown, including:
- **Image syntax**: `![alt text](url)`
- **Bold text**: `**text**`
- **Code blocks**: ` ```language code ``` `

However, GitLab's markdown renderer may have different rendering behavior for badge images and bold text.

### Recommended Fix
To ensure the review formatting is consistent across both platforms, we need to:

1. **Option A**: Add explicit line breaks between badges and title in the review body
2. **Option B**: Use a different formatting approach that works better with GitLab's markdown renderer
3. **Option C**: Test the review body formatting on GitLab and adjust accordingly

**Option A** is recommended because it ensures the review formatting is consistent across both platforms.

### GitLab Markdown Best Practices
GitLab's markdown renderer supports:
- **Line breaks**: Use two spaces at the end of a line to create a line break
- **Blockquotes**: Use `>` for blockquotes
- **Code blocks**: Use ` ``` ` for code blocks
- **Lists**: Use `- ` or `* ` for bullet lists

---

## Summary of Issues

| Issue | Platform | Root Cause | Recommended Fix |
|-------|----------|------------|-----------------|
| 1 | GitLab | Reaction posted AFTER review in separate reconciliation cycle | Post reaction synchronously after review in same reconciliation cycle |
| 2 | GitLab | Review body formatting not compatible with GitLab's markdown renderer | Add explicit line breaks between badges and title |

## Next Steps

1. **Fix Issue 1**: Modify the reconciler to post the reaction immediately after the review in the same reconciliation cycle
2. **Fix Issue 2**: Adjust the review body formatting to ensure badges and title are on separate lines
3. **Test both fixes** on both GitHub and GitLab to ensure consistency
4. **Update documentation** to reflect the changes

## Files to Modify

1. `services/control-plane/src/queue/reconciler.rs` - Modify `deliver_review()` to post reaction after review
2. `services/control-plane/src/review.rs` - Modify `inline_body()` or `render_body()` to add explicit line breaks
3. `services/control-plane/src/integrations/gitlab.rs` - Verify award emoji API usage

## GitLab API References

- **Award Emoji API**: https://docs.gitlab.com/ee/api/reactions.html
  - `POST /projects/{id}/merge_requests/{iid}/award_emoji`
  - `POST /projects/{id}/issues/{iid}/award_emoji`
  - `GET /projects/{id}/merge_requests/{iid}/award_emoji`
  - `DELETE /projects/{id}/merge_requests/{iid}/award_emoji/{award_id}`

- **GitLab Markdown**: https://docs.gitlab.com/ee/user/markdown.html