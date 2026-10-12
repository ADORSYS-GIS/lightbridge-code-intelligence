# AI Agent Review Process and Capability

This report details how the Lightbridge LLM agent actually reads, understands, and reviews code.

## 1. The Core Paradigm: Mediated Tools

The reviewer is a **native, in-process Rust loop** (`services/agent-runner/src/review/native/agent.rs`) that interacts with the code exclusively via **mediated write tools**. There is no OpenCode or MCP subprocess.

The LLM operates in a loop:
1. The system prompt and context are injected.
2. The LLM responds with a tool call.
3. The runner executes the tool and feeds the result back as a new message.
4. The loop repeats until the LLM calls `finish` or the turn budget (`max_turns`) is exhausted.

Tools are registered into a central registry that proxies between the LLM and the control plane:

```rust
// services/review-agent/src/tools.rs:102
pub fn tool_registry(client, embedder, discovered, caps, sast) -> Result<ToolRegistry, RegistryError> {
    let services = ReviewServices { client, embedder };
    let mut registry = ToolRegistry::new();
    
    // Register retrieval tools
    vector::register(&mut registry, &services, caps)?;
    graph::register(&mut registry, &services, caps)?;
    
    // Register file read tools
    read_file::register(&mut registry, caps)?;
    
    // Register SAST, Write, and Control tools
    record::register(&mut registry, &services, caps)?;
    sast::register(&mut registry, sast)?;
    // ...
    
    Ok(registry)
}
```

## 2. Context Injection (How it understands the task)

Before the loop begins, the context window is constructed. The agent receives:
- **System Prompt:** Configurable by the operator (`reviewSystemPrompt` for deep, `reviewSystemPromptFast` for fast).
- **The Diff:** The unified diff of the PR.
- **Prior Review Memory:** The agent's own past reviews on this PR to ensure consistency.
- **Feedback Memory:** Prior human rejections (thumbs down) so the agent learns not to raise false positives.
- **Workspace Instructions:** Contents of `AGENTS.md` or `CLAUDE.md`.

*Note: Large context blocks are dynamically scaled to the model's context window size via `review.context_window` configuration.*

## 3. How the Agent Reads Code

The agent does not blindly load the entire repository. It uses targeted tools:
- **`read_file`:** Reads raw source code from the checkout. Capped at 64 KiB per read with strict path sanitization.
- **`lightbridge_vector_semantic_search`:** Retrieves code chunks via pgvector.
- **`lightbridge_graph_find_symbol` / `get_callers`:** Traverses the Neo4j structural graph.

## 4. SAST Integration (`run_sast`)

Lightbridge uses `opengrep` for deterministic SAST scanning. SAST is **not** an automatic pre-pass. It is a tool (`run_sast`) that the LLM must choose to call, configured per tier in `review.<tier>.tools`.

When invoked, the tool executes `opengrep` strictly over the changed files, pushes severe findings (P1/P2) directly to the review buffer, and returns an advisory digest back to the LLM to prevent duplication.

## 5. Convergence and Quality Gates

To prevent rubber-stamping and hallucinations, the loop implements several rigorous validation mechanisms in the **deep tier**:

- **Turn Budgets & Wind-down:** Near the end of its turn limit (default 40), the agent's read tools are revoked. It is forced into a "wind-down" phase where it can only write findings or finish.
- **Full-Diff Coverage Gate:** If the agent calls `finish` but hasn't read or commented on all the changed files in the diff, the control plane will **bounce the finish** (up to 3 times), explicitly telling the LLM which files were ignored.
- **Refute Pass:** If the agent records a high-severity finding (P0/P1), its first `finish` is bounced, forcing it to re-verify its own findings against the cited code and use the `retract_finding` tool if it detects a hallucination.

## 6. Fast Tier vs Deep Tier

The behavior of the agent drastically shifts based on the tier:
- **Fast Tier:** Triggered on PR open. Optimized for speed and cost. `max_turns` is clamped to 5. The agent is **denied** access to retrieval tools (semantic/graph). It only sees the diff and must rely solely on it.
- **Deep Tier:** Triggered by `@mention`. Full 40-turn budget. Full access to semantic search, graph traversal, and `read_file`. It must pass the coverage and refute gates.
