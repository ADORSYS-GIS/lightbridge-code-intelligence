# End-to-End Request Journey: From Webhook to Review Egress

This report details the complete journey of a code review request in the Lightbridge Code Intelligence system, starting from a forge webhook (GitHub/GitLab) down to the final posted review.

## 1. Trigger and Webhook Handling

The journey begins at the control plane (`services/control-plane/src/main.rs`), which exposes the `POST /webhook` unified route. This route handles both GitHub and GitLab events:

```rust
// services/control-plane/src/main.rs
// Unified webhook route — detects the platform (GitHub/GitLab) from headers.
.route("/webhook", post(webhook::webhook_router))
// Legacy GitHub webhook route — kept for compatibility.
.route("/github/webhook", post(webhook::github_webhook_legacy))
```

When a webhook arrives, `services/control-plane/src/http/webhook.rs` detects the platform, verifies the HMAC signature, and processes the event. The tier is decided based on the trigger:
- **PR Opened (`pull_request opened`):** Triggers an automatic **fast** review (`tier: "fast"`).
- **Comment Mention (`@mention`):** Triggers an on-demand **deep** review (`tier: "deep"`).

The webhook handler creates a `Task` in the PostgreSQL database with the designated `tier`.

## 2. Dispatch and Agent Job Creation

The **dispatcher** role in the control plane monitors the tasks table. When it picks up a new task, it spawns a dedicated Kubernetes Job (`agent-runner`). 
Each task gets its own hermetic environment.

## 3. Clone and Index Decision

When the `agent-runner` Job starts, the first step is to check out the repository at the target `headSha`.

Before reviewing, the runner checks if the repository needs indexing. If the repository is completely cold, it performs a full index. Otherwise, it reuses the base index:

```rust
// services/agent-runner/src/main.rs
let needs_index = context.command == "index" || !context.repo_indexed;
```
If the repository is already indexed, the review **reuses the base index** of the default branch (`latest_indexed_commit`). This avoids the cost of re-indexing the entire repository for every single PR, pinning the retrieval to the latest snapshot.

## 4. The Native Agent Loop

The `agent-runner` executes the in-process native agent loop (`services/agent-runner/src/review/native/agent.rs`). The LLM is fed a prompt containing the unified PR diff, prior review memory, repo feedback memory, and workspace instructions (e.g., `AGENTS.md`).

The agent then enters a chat loop where it can call mediated tools:
- **Fast Tier:** Operates diff-only. Tools are restricted to `add_review_comment`, `finish`, `abort`, and optionally `run_sast`.
- **Deep Tier:** Full access to structural and semantic search (`lightbridge_graph_find_symbol`, `read_file`), allowing deep code investigation.

*Note: The agent does NOT post directly to GitHub. It buffers its findings using the `add_review_comment` tool.*

## 5. Finalize and Output Shaping

Once the agent loop finishes, the `agent-runner` sends the buffered findings back to the control plane. The control plane's finalize stage (`services/control-plane/src/http/internal.rs` -> `finalize_review`):

1. **Validates:** Re-validates findings against the PR diff to ensure they anchor correctly to commentable lines.
2. **Buckets:** Sorts findings into `inline` (anchored to diff), `deferred` (changed file but bad line), and `out_of_scope` (unchanged files).
3. **Renders:** Compiles the Markdown body. Fast tier gets a specific banner (`> 🅵 **Fast automated pass**`).

```rust
// Conceptually, handling outcomes looks like:
match outcome {
    ReviewOutcome::Finished => { /* finalize and post findings */ },
    ReviewOutcome::Exhausted => { /* finalize but append an exhaustion note */ },
    ReviewOutcome::Aborted(reason) => { /* clear unverified buffer, post abort note */ },
}
```

## 6. Egress (Reconciler)

To maintain a strict trust boundary, the `serve` role does not write to GitHub. Instead, the **reconciler** role continuously drains the `github_outbox` table and performs the actual API calls to GitHub or GitLab to post the grouped review and inline comments.
