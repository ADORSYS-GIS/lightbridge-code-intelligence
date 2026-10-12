# Role of the Knowledge Graph

The Knowledge Graph is a foundational pillar of the deep review tier in Lightbridge Code Intelligence. It provides the AI agent with a structural understanding of the codebase without requiring the agent to parse or comprehend raw syntax trees directly.

## 1. What is the Knowledge Graph?

The Knowledge Graph is a Neo4j database populated by the `lci-codegraph` extraction crate during the indexing phase. It maps out the code's structural facts into two primary data structures:

```rust
// services/codegraph/src/graph/mod.rs:47
pub struct GraphNode {
    pub node_id: String,      // "<file>#<line>:<name>"
    pub label: String,        // callable labels carry a "()" suffix
    pub source_file: String,
    pub start_line: i64,      // 1-based
}
pub struct GraphEdge {
    pub source: String,
    pub target: String,
    pub relation: String,     // "contains" | "method" | "calls"
}
```

The control plane stores these objects into Neo4j using idempotent Cypher `MERGE` queries scoped to the repository and commit hash:

```cypher
-- services/control-plane/src/integrations/neo4j.rs
-- Inserting Nodes
MERGE (s:Symbol {repo_id: $repo, commit: $commit, node_id: $id})
  SET s.label = $label, s.source_file = $file, s.start_line = $line;

-- Inserting Edges
MATCH (a:Symbol {repo_id: $repo, commit: $commit, node_id: $src})
MATCH (b:Symbol {repo_id: $repo, commit: $commit, node_id: $dst})
MERGE (a)-[r:REL {relation: $rel}]->(b);
```

## 2. The Agent's View of the Graph

The **reviewer agent never sees a raw syntax tree**. Instead, it interacts with the pre-materialized knowledge graph through specific, mediated read-only tools:

### Tool 1: `lightbridge_graph_find_symbol`
When the LLM wants to find where a specific struct or function is defined.
- **Backend Action:** The control plane executes a Cypher query on Neo4j matching the term:

```cypher
MATCH (s:Symbol {repo_id: $repo, commit: $commit})
WHERE toLower(s.label) CONTAINS toLower($term)
   OR toLower(s.node_id) CONTAINS toLower($term)
   OR toLower(s.source_file) CONTAINS toLower($term)
RETURN s.node_id, s.label, s.source_file, s.start_line LIMIT $limit
```

### Tool 2: `lightbridge_graph_get_callers`
When the LLM needs to understand the impact of a change by looking for caller dependencies.
- **Backend Action:** The control plane executes a Cypher query traversing the `[:REL {relation: 'calls'}]` edge in reverse:

```cypher
MATCH (caller:Symbol {repo_id: $repo, commit: $commit})
       -[:REL {relation: 'calls'}]-> 
       (target:Symbol {repo_id: $repo, commit: $commit, node_id: $id})
RETURN caller.node_id, caller.label, caller.source_file, caller.start_line LIMIT $limit
```

## 3. The Query Path

When the agent invokes a graph tool, the flow is strictly mediated:
1. Agent outputs a request to call `lightbridge_graph_find_symbol`.
2. The `agent-runner` (`services/review-agent/src/tools/graph.rs`) intercepts the call.
3. The runner translates this to an HTTP POST to the control plane (`/internal/tasks/{id}/graph/query`).
4. The control plane applies a strict `task_scope()` security boundary so the query is pinned to the current snapshot.
5. The control plane queries Neo4j and passes the JSON hit array back to the LLM.

## 4. Why a Graph? (Compared to Semantic Search)

While semantic search (pgvector) answers *"what code reads like this?"*, it struggles with structural queries. The dual-retrieval architecture ensures the agent can:
- Use **semantic search** to find business logic or unreferenced concepts.
- Use the **knowledge graph** to perform precise refactoring analysis, dependency tracing, and call-graph traversals that vector math cannot reliably provide.
