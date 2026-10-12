# OpenCode Integration

This report details how OpenCode is integrated into the Lightbridge Code Intelligence platform, how it is installed, and its role as the agent transport host.

## 1. What is OpenCode?
OpenCode is an open-source, CLI-based AI coding agent. Instead of Lightbridge building and maintaining a complex, multi-turn LLM interaction loop from scratch, Lightbridge embeds OpenCode to handle the core "agent loop" (prompting the model, parsing tool calls, and maintaining the conversation history).

This architecture was introduced in ADR-0097 and RFC-0009.

## 2. How is OpenCode Installed?
OpenCode is **not** installed via npm or built from source during the image build. Instead, it is installed as a pre-compiled, statically-linked binary.

In the `agent-runner` Docker image (`services/agent-runner/Dockerfile.review-opencode`), the installation process is highly controlled for security and reproducibility:

1. **Pinned Binary:** The build script downloads a specific release asset from GitHub (`anomalyco/opencode/releases/download/...`).
2. **Musl Build:** It specifically downloads the `musl` variant (`opencode-linux-x64-musl.tar.gz`) so it can run natively on the lightweight Alpine Linux base image without requiring `glibc`.
3. **SHA256 Verification:** To prevent supply-chain attacks, the downloaded binary's SHA256 hash is strictly verified against a hardcoded hash in the Dockerfile before it is extracted to `/usr/local/bin/opencode`.

## 3. How Exactly is it Used?
When a review task is triggered, the `agent-runner` Kubernetes Job boots up. Here is how OpenCode takes over:

### The Supervisor Pattern
The main Lightbridge Rust binary (`agent-runner`) does not talk to the LLM directly anymore. Instead, it acts as a **supervisor**. 
1. The supervisor clones the repository.
2. It generates a dynamic, highly configured `opencode.jsonc` file for the specific review task.
3. It spawns OpenCode as a subprocess using the command: `opencode acp`.

### The Agent Loop (ACP)
OpenCode takes over and enters its "Agent Control Protocol" (ACP) loop. It reads the system prompt, sends the context to the LLM, and waits for the LLM to request a tool execution.

### Tool Provisioning via MCP
OpenCode does not have direct access to Lightbridge's internal knowledge graph or vector database. Instead, the `opencode.jsonc` file configures OpenCode to use an **MCP (Model Context Protocol)** server provided by Lightbridge.

Lightbridge provides a binary called `lci-review-mcp`. OpenCode spawns this binary over standard I/O (stdio). When the LLM decides to search for a symbol, OpenCode forwards the tool call to `lci-review-mcp`, which safely queries the Control Plane and returns the result.

### First-Party Plugins & Quality Gates
OpenCode supports TypeScript plugins. Lightbridge mounts its own custom plugins into OpenCode at runtime (`/opt/lightbridge/opencode/plugins`).
OpenCode natively executes these TypeScript plugins (using its built-in Bun runtime). These plugins enforce Lightbridge's strict rules:
- **The Coverage Gate:** A plugin intercepts the `finish` tool call. If the LLM hasn't reviewed all changed files, the plugin rejects the tool call and forces the LLM to continue.
- **The Refute Pass:** A plugin enforces the requirement that high-severity findings must be explicitly verified before submission.
- **Logging:** A logger plugin extracts the exact reasoning and tool usage from OpenCode's internal state and streams it to Loki for dashboard observability.

## 4. Summary
Lightbridge uses OpenCode purely as an "engine" to drive the LLM loop. 
- OpenCode is installed securely as a downloaded, pinned binary. 
- It is spawned by the Rust supervisor.
- It accesses the repository context strictly through Lightbridge's own mediated MCP tools.
- It is constrained by Lightbridge's custom TypeScript plugins that enforce strict review quality gates.
