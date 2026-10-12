# The AI Agent Loop and Quality Gates

This report details the inner workings of the "AI Agent Loop" and "Quality Gates" stages shown in the Architecture at a Glance diagram. These stages represent the core decision-making phase of a Lightbridge review task.

## 1. Core Concepts

### Syntax Tree
A **Syntax Tree** (or Abstract Syntax Tree - AST) is a hierarchical tree representation of the syntactic structure of source code. Unlike a flat text representation, a syntax tree captures the logical structure of code, showing how functions, classes, statements, and expressions are nested within each other. This allows automated systems to understand the *meaning* of code rather than just its appearance.

### Tree-sitter
**Tree-sitter** is a parser generator and incremental parsing library for programming languages. Unlike traditional parsers that require a complete, valid program to parse, Tree-sitter is designed to work with partial, malformed code. It produces a concrete syntax tree that can be updated incrementally as code changes, making it ideal for code intelligence systems that need to understand code in real-time.

### Knowledge Graph
A **Knowledge Graph** is a structured representation of entities and their relationships. In the context of Lightbridge, it's a graph database (Neo4j) where every function, class, variable, and module is a node, and function calls, imports, and dependencies are edges connecting these nodes. This enables the system to answer questions like "What functions call this modified function?" and "Which files depend on this module?"

### Vector Embeddings
**Vector Embeddings** are numerical representations of code that capture semantic meaning. By converting code snippets into high-dimensional vectors using embedding models (like OpenAI's text-embedding-3-small), the system can perform semantic similarity searches. This allows the AI to find code that implements similar functionality even when the exact keywords don't match.

## 2. The AI Agent Loop (OpenCode ACP)

In the Lightbridge architecture, the "AI Agent Loop" is not a custom-built while-loop in Rust. Instead, Lightbridge embeds **OpenCode**, an open-source, CLI-based AI coding agent.

### What is Fed into OpenCode?
Before the agent loop starts, the Agent Runner prepares a massive initial context payload. This payload acts as the foundation of the AI's understanding of the task. It includes:

1. **The Code Diff:** The exact unified diff of the pull request, annotated with line numbers so the AI knows exactly what lines were added, modified, or deleted.
2. **Pull Request Metadata:** The title, description, and author of the PR, giving the AI the human context of *why* the change is being made.
3. **System Instructions:** A strict persona definition instructing the AI to act as a senior staff engineer, focusing on security, performance, and architectural integrity rather than nitpicking formatting.
4. **Repository Rules (`AGENTS.md`):** If the repository has an `AGENTS.md` or `.github/AGENTS.md` file (like Lightbridge's own AI Governance rules), its contents are injected directly into the prompt so the AI obeys repository-specific guidelines.

### How the Loop Works
Once OpenCode receives this initial payload, it drives the Agent Control Protocol (ACP) loop autonomously:

1. **LLM Communication:** It sends the payload to the frontier LLM (e.g., Claude 3.5 Sonnet or GPT-4o) to begin reasoning.
2. **Tool Dispatching:** The LLM realizes it needs more context (e.g., "What calls this modified function?") and attempts to use a tool like `graph_find_symbol` or `vector_semantic_search`.
3. **Mediated Execution:** OpenCode intercepts the tool call and forwards it to Lightbridge's MCP (Model Context Protocol) server (`lci-review-mcp`). The Control Plane fulfills the request and returns the data.
4. **Context Expansion:** OpenCode injects the returned tool data back into the LLM's context window and prompts the LLM again.

This loop continues iteratively: the LLM asks questions, explores the codebase, reasons about findings, and buffers comments until it decides it has completed its review.

## 3. Quality Gates Implementation

Because LLMs are non-deterministic and prone to hallucinations, Lightbridge does not trust the AI's output blindly. Before the review is allowed to exit the Agent Runner and be posted to the pull request, it must pass through strict **Quality Gates**.

These gates are implemented as **TypeScript plugins** that are mounted directly into OpenCode (`/opt/lightbridge/opencode/plugins`). OpenCode executes them natively using its bundled Bun runtime.

### The Coverage Gate

**Implementation Details:**

The Coverage Gate is implemented as a TypeScript plugin that intercepts the AI's attempt to use the "finish review" tool. The plugin performs the following checks:

1. **File Tracking:** The plugin maintains a set of all files that have been read or commented on during the review session.
2. **Diff Analysis:** It parses the PR diff to identify every file that was modified.
3. **Comparison:** It compares the tracked files against the modified files.
4. **Validation:** If any modified file is missing from the tracked set, the plugin rejects the "finish" tool call.

**Code Location:**
- Plugin file: `/opt/lightbridge/opencode/plugins/coverage-gate.ts`
- Key function: `validateCoverage()`
- Error message: "You have not reviewed X, Y, and Z files"

**Mechanism:**
- The plugin hooks into OpenCode's tool execution layer
- When the AI attempts to call `finish_review()`, the plugin runs validation
- If validation fails, the plugin returns an error to the LLM and forces the loop to continue
- The AI is instructed to read the missing files before retrying

### The Refute Pass (The "Two-Time" Review)

To prevent hallucinations (e.g., the AI claiming a security check is missing when it is actually performed in a parent function), the system requires the AI to challenge its own assumptions.

**Implementation Details:**

The Refute Pass is implemented as a conditional gate that checks the severity of findings before allowing them to proceed:

1. **Severity Threshold:** Findings with severity > 2 (medium or high) trigger the Refute Pass.
2. **Persona Switching:** The system temporarily switches the AI's persona from "reviewer" to "hardened skeptic."
3. **Knowledge Base Search:** The AI is instructed to search the structural graph and semantic index for evidence that its original finding is wrong.
4. **Validation:** Only if the AI cannot refute its own finding after a deep search is the finding allowed through.

**Code Location:**
- Plugin file: `/opt/lightbridge/opencode/plugins/refute-pass.ts`
- Key function: `validateRefutePass()`
- Configuration: `refute_threshold` in `opencode.config.json`

**Mechanism:**
- The plugin intercepts high-severity findings before they are buffered
- It sends a special prompt to the LLM: "Act as a hardened skeptic. Search the knowledge base for evidence that this finding is incorrect."
- The LLM performs a second pass over the codebase using MCP tools
- If the LLM finds evidence contradicting the original finding, it retracts it
- Only proven issues survive to the final review

### Diff Alignment Check

The system validates that the lines the AI wants to comment on actually exist in the modified PR diff. If the AI hallucinates line numbers or tries to comment on unchanged code, the finding is either realigned or downgraded to a general summary comment, preventing the forge API (GitHub/GitLab) from rejecting the comment.

**Implementation Details:**

The Diff Alignment Check is implemented as a post-processing step that runs after the AI has completed its review:

1. **Line Number Validation:** The plugin checks if each comment's line number falls within the modified diff range.
2. **Realignment:** If a comment is outside the diff, the plugin attempts to realign it to the nearest changed line.
3. **Downgrade:** If realignment is not possible, the plugin downgrades the finding to a general summary comment.
4. **Submission:** Only validated findings are submitted to the GitHub/GitLab API.

**Code Location:**
- Plugin file: `/opt/lightbridge/opencode/plugins/diff-alignment.ts`
- Key function: `validateAndAlignComments()`
- API client: Uses the GitHub/GitLab API client to post comments

**Mechanism:**
- The plugin receives the final list of findings from the AI
- It parses each finding's line number and content
- It checks against the PR diff boundaries
- If a finding is invalid, it is either fixed or removed
- The plugin then formats the findings into Markdown and posts them

## 4. Conclusion

The **AI Agent Loop** provides the creative, autonomous exploration of the codebase using OpenCode, while the **Quality Gates** act as deterministic safety nets that ensure the review is thorough, accurate, and hallucination-free before it reaches human developers.

## 5. Summary of the OpenCode Payload

To summarize how the loop begins: OpenCode does not magically know about the repository. It is explicitly fed a comprehensive context payload at startup.

**The Initial Payload (The Prompt)**:
1. **The PR Diff:** The literal text of the diff, annotated with line numbers so the AI can reference exact lines when generating inline comments.
2. **PR Metadata:** Title, description, and author.
3. **System Instructions:** The persona and security focus.
4. **AGENTS.md:** The local AI Governance rules.

This initial prompt is what jump-starts the ACP loop, giving the AI the baseline knowledge it needs to begin calling tools and reading the codebase.
