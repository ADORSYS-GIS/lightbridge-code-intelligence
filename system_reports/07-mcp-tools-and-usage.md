# MCP (Model Context Protocol) Usage

This report details how Lightbridge Code Intelligence uses the Model Context Protocol (MCP) to provide tools to the AI agent securely.

## 1. What is MCP?
The Model Context Protocol (MCP) is a standard that allows AI agents to securely interact with external data sources and tools. In Lightbridge, it is the standard interface through which the untrusted review agent reads code, queries the knowledge graph, and retrieves external knowledge.

## 2. The Mediated MCP Architecture

Lightbridge employs a **mediated** approach to MCP (ADR-0020, ADR-0037).

Because the agent runs in an isolated, untrusted Kubernetes Job (`agent-runner`), it is completely denied direct access to databases, GitHub credentials, or the internet. Instead, it must route its tool requests through the Control Plane.

1. The Control Plane exposes a `lightbridge` MCP endpoint.
2. The agent connects to this endpoint using a short-lived, task-scoped runner token.
3. The agent sends an MCP tool execution request (e.g., `graph_find_symbol`).
4. The Control Plane intercepts the request, validates that the agent is allowed to perform it on the specific repository snapshot, executes the query against Neo4j or Postgres, and returns the result.

## 3. Tool Surfaces

The agent utilizes several categories of MCP tools:

### Native Mediated Tools (Retrieval & Write)
These are core capabilities built directly into the Lightbridge control plane:
- `vector_semantic_search` (Semantic retrieval)
- `graph_find_symbol` / `graph_get_callers` (Structural retrieval)
- `read_file` (Raw source reading)
- `add_review_comment` (Buffering findings)

### Forge Read Tools (ADR-0096)
To allow the agent to read issues, pull requests, and other branches without giving it a GitHub token, the control plane curates a set of read-only forge tools on the mediated MCP surface. The control plane performs the API calls using its server-side token on behalf of the agent.

### External Knowledge MCP Servers (ADR-0066)
Lightbridge supports dynamic discovery of external, in-cluster MCP servers (e.g., `brave-search` for web context). 
A repository owner can declare external MCP tools in their configuration. The control plane registers these servers and exposes their tools to the agent through the same unified, mediated surface. This allows the agent to search the web or internal wikis without breaking the security sandbox.

## 4. Current State (OpenCode Integration)

While early iterations of the system moved to a native Rust loop (ADR-0026), the platform recently adopted **OpenCode** (ADR-0097, RFC-0009) as the transport host for the agent. 

OpenCode brings a robust, battle-tested MCP client built-in. By exposing the Lightbridge capabilities as standard MCP tools, the system seamlessly integrates with the OpenCode runtime while maintaining the absolute trust boundary enforced by the Control Plane's mediation layer.
