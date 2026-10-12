# Agent Runner — `services/agent-runner/`

## What is it?

The **per-task Kubernetes Job** launched by the control plane's dispatcher (ADR-0004). It is
**untrusted** — it never holds the GitHub App private key and never writes to GitHub directly.

The lifecycle: **clone → index → SAST → review → report**.

---

## Module map

```
src/
├── main.rs           # Entry point: config, lifecycle, SIGTERM handling, self-cancel poll
├── lib.rs            # Library root (re-exports for the bin)
├── clone.rs          # Git clone + checkout + PR diff computation
├── ratelimit.rs      # Gateway rate-limit header parsing (shared by embed + chat clients)
│
├── bootstrap/        # Startup wiring
│   ├── mod.rs        #  └─ submodule re-exports
│   ├── config.rs     #     RunnerConfig, EmbeddingsConfig, ReviewConfig, SastConfig, ReviewTool enum
│   └── client.rs     #     ControlPlaneClient — HTTP client for the internal runner API
│
├── indexer/          # Indexing pipeline (epic #5, slices 2 + 3)
│   ├── mod.rs        #     collect_chunks + index_checkout (orchestration)
│   ├── language.rs   #     Language detection by file extension + tree-sitter grammar support
│   ├── chunker.rs    #     Tree-sitter AST extraction + windowed fallback chunking
│   ├── embeddings.rs #     OpenAI-compatible embeddings client (eaig gateway)
│   └── graph.rs      #     Graphify CLI → structural graph (nodes + edges, AST-only, no LLM)
│
├── sast/             # Static Application Security Testing
│   └── mod.rs        #     opengrep invocation, SARIF parsing, finding buffering, digest
│
├── review/           # Review pipeline
│   ├── mod.rs        #     Orchestration: run_native_agent (picks tier, drives the loop)
│   ├── instructions.rs #  Reads repo's AGENTS.md/CLAUDE.md for house rules
│   └── native/       #     The native Rust review agent loop (ADR-0026 + ADR-0037)
│       ├── mod.rs
│       ├── agent.rs  #     Multi-turn loop: tool set, wind-down, coverage gate, refute pass
│       ├── chat.rs   #     ChatClient: OpenAI-compatible completions with streaming + retry/backoff
│       └── tools.rs  #     Tool definitions + in-process dispatcher (no MCP subprocess)
```

---

## Lifecycle (what happens in `main.rs`)

```
RunnerConfig::from_env() ────────────── reads TASK_ID, CONTROL_PLANE_URL, AGENT_RUNNER_TOKEN
    │
    ▼
ControlPlaneClient::new() ───────────── sets up HTTP client for internal API
    │
    ▼
load_file_config() ──────────────────── optional JSON config (ConfigMap-mounted)
    │
    ▼
EmbeddingsConfig::resolve() ─────────── which model, base URL, API key
ReviewConfig::resolve_tiers() ───────── fast + deep tier configs (own model/gateway/prompt per tier)
SastConfig::resolve() ───────────────── opengrep settings (opt-in, best-effort)
    │
    ▼
tokio::select! {
    run(...) → RunResult,                    # the actual work
    terminated() → exit 143,                 # SIGTERM from K8s
    cancelled_upstream() → exit 143,         # self-cancel poll (10s interval)
}
    │
    ▼
report(client, "succeeded" | "failed")
```

---

## AI/ML terms explained

### Tree-sitter parsing & chunking (`chunker.rs`, `language.rs`)

**What it does:**
[Tree-sitter](https://tree-sitter.github.io/tree-sitter/) is a parser generator tool and
incremental parsing library. Lightbridge uses it to build **syntax-aware code chunks** —
functions, structs, classes, methods, impl blocks — rather than naively splitting by line count.

**Supported languages:** Rust, TypeScript, JavaScript, Python (with C grammars compiled at
build time). All other files fall back to windowed chunking (100 lines, 50-line overlap).

**What gets extracted:**
- Rust: `function_item`, `struct_item`, `enum_item`, `trait_item`, `impl_item`, `mod_item`, `type_alias`
- TypeScript/JS: `function_declaration`, `class_declaration`, `method_definition`, `arrow_function`
- Python: `function_definition`, `class_definition`, `decorated_definition`

Each chunk carries: `file_path`, `language`, `chunk_type`, `symbol_name` (e.g. "validate_session"),
`start_line`, `end_line`, `content`, and (after embedding) `embedding`.

**Large nodes:** If a node exceeds 150 lines, the chunker recurses into its children. If no
interesting sub-nodes exist, the whole large node is emitted as one chunk (the embedding API
truncates if it exceeds the context window).

### Embeddings (`embeddings.rs`)

**What it is:**
An HTTP client that talks to any **OpenAI-compatible** `POST /v1/embeddings` endpoint. In
production this is the **eaig/core-gateway** (Envoy AI Gateway), the same gateway LibreChat's
RAG uses.

**Key details:**
- Batches 32 chunks at a time (configurable `EMBED_BATCH_SIZE`)
- Retries transient failures (connect, timeout, 429, 5xx) up to 3 times with exponential backoff
- Honours 429 `Retry-After` headers
- Sorts returned vectors by `index` field (OpenAI spec doesn't guarantee order)
- Supports private CA certs via `EMBEDDINGS_CA_CERT` env var (eaig uses self-signed internal CA)
- Adds attribution headers (`x-code-intelligence-repo`, `x-code-intelligence-owner`, etc.)
  for per-project token billing

### Semantic search via pgvector

When the agent calls `lightbridge_vector_semantic_search`, the runner:
1. Embeds the query text using the same embeddings model
2. Sends the resulting vector to `POST /internal/tasks/{id}/search` on the control plane
3. The control plane runs a pgvector `<=>` (cosine distance) query, scoped to the task's
   repository snapshot
4. Returns the most similar code chunks with file path, line range, content, and similarity score

### Structural knowledge graph via Graphify (`graph.rs`)

**What it is:**
[Graphify](https://github.com/terminusdb/graphify) is a multi-language (36 grammars) CLI that
extracts AST nodes and their relationships into a graph JSON. Lightbridge spawns it as:

```
graphify update <checkout> --no-cluster
```

The `--no-cluster` flag ensures **AST-only, no LLM pass** — important because Graphify defaults
to running a semantic (LLM) pass over documentation, which would require an API key and fail
without one. Lightbridge strips all LLM API keys from the environment before running it.

**What gets extracted & submitted:**
- **Code nodes**: functions, classes, files with their `node_id`, `label`, `source_file`, `line`
- **Edges**: relationships like `contains`, `calls`, `method of`
- **Filtered to code-only**: document/heading nodes (markdown) and synthetic nodes (no source_file)
  are dropped
- Written to a private dir **outside** the checkout to avoid polluting/reading the repo's own
  `graphify-out/` directory

**Why both pgvector AND Neo4j?**
- pgvector: semantic similarity search ("find code that does something like X")
- Neo4j: structural queries ("what calls this function?", "find all symbols named validate")

They are complementary — agents use both during review.

### SAST (opengrep) — `sast/mod.rs`

**What it is:**
A deterministic first-pass static analysis scan using **opengrep** (an open-source Semgrep
fork/evolution). It runs over the PR's changed files with a vendored, pinned ruleset
(`/opt/opengrep-rules` in the image).

**Integration:**
- SAST findings are buffered into the **same** review buffer as the agent's findings
- A compact **digest** (list of `file:line:rule`) is fed into the agent's prompt so it
  doesn't redundantly re-report the same lines
- On a true `(file, line)` collision between SAST and an agent finding, the agent's richer
  finding wins (last-write-wins upsert)
- Best-effort: scan failure is logged, never fatal to the task. Opt-in (default disabled).

### Review agent loop (`review/native/agent.rs`)

**What it is:**
A multi-turn chat loop driving the configured LLM via the eaig gateway. The model is offered
a set of tools (retrieval, write, control) and the loop runs until:

- The model calls `finish` → **Finished**: findings are posted
- The turn budget runs out → **Exhausted**: buffered findings posted with truncation note
- The model calls `abort` → **Aborted**: buffer cleared, only the abort note posted
- A transport error occurs → **Err**: nothing posted, task fails

**Key loop features:**

| Feature | Description |
|---------|-------------|
| Wind-down convergence | Near the turn budget, retrieval tools are removed → model must finish |
| Coverage gate | First `finish` bounce if changed files haven't been engaged (B, #137) |
| Refute pass | Pre-finish verification of P0/P1 findings (Phase 2, ADR-0043) |
| Scratchpad-loop guard | Consecutive recordings on same `(file,line)` trigger tool removal |
| Cumulative read budgets | Per-tool budgets (`max_files_read`, `max_searches`, `max_batches`) |
| Context-window budgeting | Estimates token usage and trims old tool output or winds down (ADR-0045) |
| Circuit breaker | After N consecutive turn failures, fail fast (ADR-0039) |
| FAST tier | Single diff-only pass with no retrieval (ADR-0062) |

### Mediated write tools (`review/native/tools.rs`)

The agent never writes to GitHub directly. Instead it calls the control plane's internal API
to **buffer** actions:

| Tool | What it does |
|------|-------------|
| `add_review_comment(file, line, title, priority, category, body, evidence, suggestion)` | Buffer one inline finding |
| `add_comment(body)` | Buffer a plain reply |
| `retract_finding(file, line)` | Drop a buffered finding (refute pass) |
| `finish(summary)` | Set verdict + finalize (flush buffer as one grouped review) |
| `abort(reason)` | Abort gracefully |

The buffer is held entirely control-plane-side, so even a crash in the runner Job doesn't
lose findings (the buffer is already on the control plane).

**Security:** `read_file` path-sanitises to within the checkout root — lexical check (rejects
absolute paths, `..` traversal) AND canonicalization (rejects symlink escapes). Bounded to 64 KiB.

### Chat client with resilience (`review/native/chat.rs`)

| Feature | Detail |
|---------|--------|
| Timeout | Per-request, default 180s (generous — eaig takes ~2 min per turn, ADR-0039) |
| Retries | Transient failures only (429, 5xx, connect/timeout), exponential backoff |
| Circuit breaker | Consecutive turn failures threshold (default 3) |
| Streaming | SSE stream with per-chunk idle timeout (instead of whole-request timeout) |
| Rate-limit awareness | Reads `RateLimit-*` / `X-RateLimit-*` headers, warns when budget <10% |
| Passthrough params | `review.extra` merged verbatim into chat body (for reasoning budget, etc.) |
