# Re-Indexing Triggers & Index Reuse

This report clarifies exactly when Lightbridge Code Intelligence runs a full repository index and how it handles pull request updates.

## 1. When Does Indexing Run?

Lightbridge is designed to avoid expensive full-repository indexing on every pull request. Indexing only occurs under two specific conditions:

1. **Cold Repository Onboarding:** 
   When a repository is first approved in the Lightbridge system, it has no index. The very first review task (or an explicit manual index trigger) will detect this and run the indexing pipeline before proceeding.

2. **Push to the Default Branch:**
   When code is successfully merged into the default branch (e.g., `main` or `master`), a webhook event (`Push`) is generated. The control plane detects this and spawns a dedicated background task to re-index the repository.

### Code Snippet: Webhook Routing
This logic is handled in the `webhook.rs` file. Notice how `Push` triggers an index task, while PR events trigger review tasks.

```rust
// services/control-plane/src/http/webhook.rs
let tier = match event {
  PrOpened   => Tier::Fast,
  AtMention  => Tier::Deep,
  Push       => create_index_task(), // Automatic re-indexing!
  _          => return,
};
```

## 2. What Happens on a PR Push?

**If you push to a PR branch, the repository is NOT re-indexed.** 

Re-indexing a repository for every single commit on a PR branch is computationally wasteful and scales poorly. Instead, Lightbridge employs an **Index Reuse** strategy (documented internally as ADR-0025).

### How Index Reuse Works:

1. When a PR review task starts, the `agent-runner` job checks out the exact PR commit (`headSha`).
2. However, for semantic and structural retrieval, it **pins itself to the latest indexed snapshot** of the default branch.
3. The PR diff itself (which the agent sees directly in its prompt) provides the agent with all the necessary context about what changed in the current PR.

### Code Snippet: The Index Check
Inside the `agent-runner`, the system evaluates whether to run the heavy indexing pipeline. It only runs if the task command is explicitly `"index"` (from a default branch push) or if the repository has absolutely no existing index.

```rust
// services/agent-runner/src/main.rs
let needs_index = context.command == "index" || !context.repo_indexed;

if needs_index {
    info!("Running full repository index pipeline...");
    run_indexing_pipeline(&context).await?;
} else {
    info!("Skipping index pipeline. Reusing base index snapshot.");
}
```

## 3. Summary

- **Push to Default Branch:** Triggers an automatic re-index to keep the base intelligence fresh.
- **Push to PR Branch:** Does **not** trigger re-indexing. The review relies on the existing default branch index (for deep context) combined with the active PR diff (for immediate changes).
