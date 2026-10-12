# How the AI Queries the Knowledge Base

This report explains in detail how the AI agent reads and uses both the Structural Graph and the Semantic Index during a code review. This is the mechanism that transforms the agent from a simple diff-reader into a repository-aware expert.

---

## 1. The Two Stores and Why Both Are Needed

When the codebase is indexed, two separate databases are populated from the same parse output:

| Store | Database | What it holds | Best for answering |
|---|---|---|---|
| **Structural Graph** | Neo4j | Every function, class, and module as nodes; every call/import as a directed edge | "Who calls this function?" "What does this class depend on?" |
| **Semantic Index** | PostgreSQL + `pgvector` | Dense vector embeddings of logical code chunks | "Where do we handle rate-limit errors?" "Show me the authentication logic" |

Neither store alone is sufficient. The graph gives exact structural precision; the semantic index gives conceptual recall. Together they give the agent the two complementary views an expert engineer has when reviewing a PR.

---

## 2. How the Structural Graph Is Queried

### Building It
During indexing, the `lci-codegraph` crate uses Tree-sitter to walk every source file. For each symbol it finds (function, struct, class, trait, etc.) it emits a **node** into Neo4j with properties: `name`, `file_path`, `line_start`, `line_end`, `language`.

For every call expression found in the AST, it emits a directed **`CALLS` edge** between the caller and callee nodes.

### Querying It (During a Review)
When the AI agent is reasoning about a PR that modifies, say, `validate_hmac_signature`, it calls the MCP tool `graph_find_symbol`:

```
tool: graph_find_symbol
args: { "name": "validate_hmac_signature" }
```

The control plane executes a Cypher query against Neo4j:
```cypher
MATCH (sym:Symbol {name: "validate_hmac_signature", repo_id: $repo_id})
OPTIONAL MATCH (caller)-[:CALLS]->(sym)
RETURN sym, collect(caller) AS callers
```

The agent receives back:
- The exact file path and line range where the function is defined.
- A list of every function across the entire codebase that calls it.

This tells the agent: **"If I change this function's behaviour, these 7 other places in the codebase will be affected."** This is knowledge the agent cannot derive from the PR diff alone.

### The `graph_get_callers` Tool
The agent also has a dedicated `graph_get_callers` tool for transitive impact analysis:

```
tool: graph_get_callers
args: { "symbol": "validate_hmac_signature", "depth": 2 }
```

This returns a two-hop chain: not just who calls the function, but also who calls the callers. This is critical for spotting blast-radius issues in deep dependency chains.

---

## 3. How the Semantic Index Is Queried

### Building It
The `lci-codegraph` crate splits each file into **semantic chunks** along logical boundaries (e.g., at function boundaries, not at fixed line counts). Each chunk is sent to an internal OpenAI-compatible embedding gateway running the `qwen3-embedding-8b` model, which returns a 4096-dimensional float vector.

Each chunk is stored in PostgreSQL as a row with columns: `repo_id`, `commit_sha`, `file_path`, `chunk_text`, `embedding` (a `vector(4096)` column via `pgvector`).

### Querying It (During a Review)
When the AI agent needs conceptual context, it calls `vector_semantic_search`:

```
tool: vector_semantic_search
args: { "query": "HMAC signature validation for webhook payloads" }
```

The control plane:
1. Sends the query text to the same embedding model to get a 4096-dimensional query vector.
2. Executes an exact cosine similarity scan (using the `vector_cosine_ops` / `<=>` operator) against the `pgvector` column. This is implemented in the Control Plane within `services/control-plane/src/db/code_chunks.rs`. The scan is scoped strictly to the active repository (`repo_id`) and its latest indexed snapshot (`commit_sha`).
3. Returns the top-k most similar code chunks.

The agent receives raw source code chunks — the exact functions and blocks that are conceptually most similar to its query, regardless of naming conventions or exact keywords.

### Why Exact Scanning Instead of an HNSW Index
Because the 4096-dimensional embedding model exceeds standard ANN (Approximate Nearest Neighbour) index limits like HNSW in `pgvector`, Lightbridge performs exact cosine similarity scans. This is acceptable because:
- Each scan is scoped to a single repository (typically a few thousand rows, not millions).
- Exact results guarantee no conceptually-relevant chunk is missed.

---

## 4. How Results Flow Back to the Agent

Both tools are exposed via the `lci-review-mcp` binary — a stdio MCP server that OpenCode connects to. The sequence is:

```
OpenCode (LLM Loop)
  ↓ requests tool: "graph_find_symbol"
lci-review-mcp (stdio MCP server)
  ↓ HTTP call with runner token
Control Plane API
  ↓ runs Cypher on Neo4j / cosine scan on Postgres
  ↓ returns JSON result
lci-review-mcp
  ↓ forwards result as MCP tool response
OpenCode
  ↓ injects result into next LLM message
AI Agent — now understands the full context
```

Each result is injected as a new assistant message, expanding the agent's context window with only the highly relevant code. This is the RAG (Retrieval-Augmented Generation) loop.

---

## 5. The Security Contract

A critical property: **the agent never directly contacts Neo4j or PostgreSQL.** Every tool call goes through the Control Plane, which:
- Validates the runner token (scoped to one specific task and repository).
- Enforces that queries are restricted to the correct `repo_id` and commit snapshot.
- Prevents the agent from browsing other tenants' code.

This is the mediated tool pattern — the agent can ask questions, but the Control Plane decides what answers it is allowed to receive.
