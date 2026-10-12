# How Inline Comments are Generated and Posted

This report explains the lifecycle of an inline review comment — from how the AI decides where to comment, to how the Control Plane maps that to the exact lines in the GitHub or GitLab API.

## 1. How the AI Knows the Line Numbers

The LLM is a text predictor; it doesn't natively understand git diffs or file lines. To solve this, the Agent Runner explicitly annotates the diff fed into OpenCode at the start of the loop. 

The diff sent to the AI looks like this:
```diff
--- src/main.rs
+++ src/main.rs
@@ -10,3 +10,4 @@
 10: fn main() {
 11:     println!("Hello");
 12:+    unsafe { do_something_dangerous(); }
 13: }
```
Because the line numbers are literally prefixed in the text, the AI knows that the dangerous code was added exactly at line `12`.

When the AI decides to leave a comment, it formats a JSON array of `ReviewComment` objects.
```json
[
  {
    "path": "src/main.rs",
    "line": 12,
    "body": "This unsafe block needs a safety doc comment explaining why the invariants hold."
  }
]
```

## 2. Diff Alignment Check (Quality Gate)

Before the comment leaves the isolated Agent Runner pod, the **Diff Alignment** Quality Gate verifies it. 
If the AI hallucinated a line number (e.g. line 999) or tried to comment on an unchanged line outside the diff context, the plugin will:
1. Attempt to realign it to the nearest modified line.
2. If it can't, it downgrades the inline comment to a general top-level summary comment so the forge API doesn't reject the entire review with an HTTP 422 Unprocessable Entity.

## 3. Posting to the Forge API (Control Plane Reconciler)

Once the validated review leaves the Agent Runner, it arrives at the **Control Plane Reconciler**, which holds the credentials to talk to the forge APIs.

The Control Plane converts the AI's `{ path, line }` into the exact payload the forge API requires.

### GitLab Implementation
GitLab requires highly specific `position` parameters. The control plane first fetches the MR's `diff_refs` (the base, head, and start SHAs) and uses them to anchor the comment.

**Code Snippet (`services/control-plane/src/integrations/gitlab.rs`):**
```rust
async fn post_review(
    &self,
    repo: &RepoRef,
    review: &ReviewPost,
) -> anyhow::Result<PostedReview> {
    let project = Self::project_encoded(repo);
    let mr_iid = review.pr_number;

    // Fetch diff_refs for the position object on inline comments.
    let refs = self.fetch_mr_diff_refs(&project, mr_iid).await?;

    // Post each inline comment as a discussion thread with a position object.
    for c in &review.comments {
        let position = serde_json::json!({
            "base_sha": refs.base_sha,
            "head_sha": refs.head_sha,
            "start_sha": refs.start_sha,
            "position_type": "text",
            "new_path": c.path,
            "old_path": c.path,
            "new_line": c.line,
        });
        let body = serde_json::json!({
            "body": c.body,
            "position": position,
        });
        
        let url = self.url(&format!(
            "/projects/{}/merge_requests/{}/discussions",
            project, mr_iid
        ));
        
        self.http.post(&url)
            .headers(self.api_headers())
            .json(&body)
            .send()
            .await?;
    }
    
    // ... post the general summary note
}
```

### GitHub Implementation
GitHub is simpler, as its Pull Request Review API natively accepts a `comments` array with just the `path` and the `line`.

**Code Snippet (`services/control-plane/src/integrations/github.rs`):**
```rust
async fn post_review(
    &self,
    repo: &RepoRef,
    review: &ReviewPost,
) -> anyhow::Result<PostedReview> {
    let url = self.url(&format!(
        "/repos/{}/pulls/{}/reviews",
        repo.full_name, review.pr_number
    ));

    // Convert our internal `ReviewComment` list into GitHub's expected format.
    // GitHub handles the diff mapping internally given the path and line.
    let gh_comments: Vec<_> = review
        .comments
        .iter()
        .map(|c| {
            serde_json::json!({
                "path": c.path,
                "line": c.line,
                "body": c.body,
            })
        })
        .collect();

    let payload = serde_json::json!({
        "body": review.body,
        "event": "COMMENT",
        "comments": gh_comments,
    });

    self.http.post(&url)
        .bearer_auth(self.token(repo).await?)
        .json(&payload)
        .send()
        .await?;
        
    // ...
}
```

### Summary
1. **Annotate Diff:** Agent Runner feeds line numbers into the AI prompt.
2. **AI Predicts:** AI outputs JSON with `path` and `line`.
3. **Validate:** Quality gate ensures the line exists in the diff.
4. **Anchor & Post:** Control Plane maps the `path` and `line` to the forge's API specification (like GitLab's SHA-based `position` object) and sends the HTTP request.
