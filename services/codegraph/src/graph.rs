//! Structural code graph (ADR-0086 §1). Built directly on tree-sitter — the same grammars and the
//! same `interesting_node` symbol set the chunker uses — as **edges on top of the per-file symbols**.
//!
//! Output is payload-compatible with the current Graphify `graph.json` parse: [`GraphNode`] mirrors
//! `agent-clients::GraphNodePayload` (`node_id`, `label`, `source_file`, `start_line`) and
//! [`GraphEdge`] mirrors `GraphEdgePayload` (`source`, `target`, `relation`). The control plane's
//! generic `:Symbol` + `[:REL {relation}]` Neo4j write and the `graph_find_symbol` /
//! `graph_get_callers` retrieval tools are unchanged behind the seam.
//!
//! Slice 1 resolves **Rust** (ADR-0086 "Rust language first"). Relations emitted:
//! - `contains` — file → top-level def, and container def (impl/mod/struct/trait) → nested def.
//! - `calls` — caller def → callee def, with **cross-file** resolution (a call in file A resolved to a
//!   definition in file B). `graph_get_callers` traverses this relation, so the name must match.

use std::collections::HashMap;

use serde::Serialize;
use tree_sitter::{Node, Tree};

use crate::chunk::interesting_node;

/// One graph node. Field set mirrors `agent-clients::GraphNodePayload` exactly.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphNode {
    pub node_id: String,
    pub label: String,
    pub source_file: String,
    pub start_line: i64,
}

/// One directed edge. Field set mirrors `agent-clients::GraphEdgePayload` exactly.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphEdge {
    pub source: String,
    pub target: String,
    pub relation: String,
}

/// A resolved structural graph, canonicalised (nodes + edges sorted, deduped) so it is stable to
/// snapshot as a golden and stable to submit.
#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
pub struct Graph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

/// The per-file structural facts: the file's nodes (file node + defs), its intra-file `contains`
/// edges, and the unresolved call sites (caller node_id + callee name) for the cross-file pass.
#[derive(Debug, Default)]
pub struct FileSymbols {
    nodes: Vec<GraphNode>,
    contains: Vec<GraphEdge>,
    /// `(caller_node_id, callee_name)` — resolved to `calls` edges once every file is known.
    calls: Vec<(String, String)>,
    /// Callable defs (functions/methods) in this file, `name → node_id`, for same-file-first
    /// resolution.
    callables: Vec<(String, String)>,
}

/// Extract the structural facts for one **pre-parsed** file. `language` gates which languages produce
/// a graph (Rust only in slice 1); an unsupported language yields empty facts (Graphify still covers
/// it). `source_file` is the repo-relative, forward-slashed path used as the file node id.
#[must_use]
pub fn extract_file(tree: &Tree, source_file: &str, source: &str, language: &str) -> FileSymbols {
    let mut facts = FileSymbols::default();
    if language != "rust" {
        return facts;
    }
    // The file node: id = the path, label = the file name. Top-level defs are `contains`-ed by it.
    facts.nodes.push(GraphNode {
        node_id: source_file.to_string(),
        label: file_label(source_file),
        source_file: source_file.to_string(),
        start_line: 0,
    });

    let bytes = source.as_bytes();
    let root = tree.root_node();
    // Stack of enclosing definition node ids (innermost last) for `contains` parenting + attributing
    // a call site to the definition it sits inside.
    let mut stack: Vec<String> = Vec::new();
    walk(&root, bytes, source_file, &mut stack, &mut facts);
    facts
}

/// DFS that, in one pass, emits def nodes + `contains` edges and records call sites attributed to the
/// innermost enclosing def.
fn walk(
    node: &Node<'_>,
    bytes: &[u8],
    source_file: &str,
    stack: &mut Vec<String>,
    facts: &mut FileSymbols,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some((kind, name)) = interesting_node(&child, bytes) {
            let start_line = child.start_position().row as i64;
            let node_id = def_node_id(source_file, start_line, kind, name.as_deref());
            let parent_id = stack
                .last()
                .cloned()
                .unwrap_or_else(|| source_file.to_string());
            facts.contains.push(GraphEdge {
                source: parent_id,
                target: node_id.clone(),
                relation: "contains".to_string(),
            });
            facts.nodes.push(GraphNode {
                node_id: node_id.clone(),
                label: name.clone().unwrap_or_else(|| kind.to_string()),
                source_file: source_file.to_string(),
                start_line,
            });
            // Functions/methods are callable — record for cross-file resolution.
            if (kind == "function" || kind == "method")
                && let Some(n) = name.clone()
            {
                facts.callables.push((n, node_id.clone()));
            }
            stack.push(node_id);
            walk(&child, bytes, source_file, stack, facts);
            stack.pop();
        } else {
            if child.kind() == "call_expression"
                && let Some(caller) = stack.last()
                && let Some(callee) = callee_name(&child, bytes)
            {
                facts.calls.push((caller.clone(), callee));
            }
            walk(&child, bytes, source_file, stack, facts);
        }
    }
}

/// The name being called in a Rust `call_expression`, if we can name it. Handles free calls
/// (`foo()`), paths (`a::b::foo()` → `foo`), method calls (`x.foo()` → `foo`), and turbofish
/// (`foo::<T>()`).
fn callee_name(call: &Node<'_>, bytes: &[u8]) -> Option<String> {
    let func = call.child_by_field_name("function")?;
    name_of(&func, bytes)
}

fn name_of(node: &Node<'_>, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" | "field_identifier" | "type_identifier" => text(node, bytes),
        // `a::b::foo` — the callable name is the final segment.
        "scoped_identifier" => node
            .child_by_field_name("name")
            .and_then(|n| text(&n, bytes)),
        // `x.foo` — the method name is the `field`.
        "field_expression" => node
            .child_by_field_name("field")
            .and_then(|n| name_of(&n, bytes)),
        // `foo::<T>` — the callable is under the `function` field.
        "generic_function" => node
            .child_by_field_name("function")
            .and_then(|n| name_of(&n, bytes)),
        _ => None,
    }
}

fn text(node: &Node<'_>, bytes: &[u8]) -> Option<String> {
    std::str::from_utf8(&bytes[node.byte_range()])
        .ok()
        .map(str::to_string)
}

/// Node id for a definition: `<file>#<line>:<name>`. Line makes it unique within a file even when two
/// defs share a name (e.g. `new` on two impls); the name keeps it human-recognisable.
fn def_node_id(source_file: &str, start_line: i64, kind: &str, name: Option<&str>) -> String {
    let label = name.unwrap_or(kind);
    format!("{source_file}#{start_line}:{label}")
}

fn file_label(source_file: &str) -> String {
    source_file
        .rsplit('/')
        .next()
        .unwrap_or(source_file)
        .to_string()
}

/// Resolve every file's call sites into `calls` edges with cross-file resolution, and return the
/// canonicalised whole-repo [`Graph`]. Resolution policy (precision-favouring, ADR-0086 R5):
/// same-file definitions win; otherwise a **unique** global definition of that name resolves;
/// ambiguous (>1 global) or unknown (0) names are dropped and counted, not guessed.
#[must_use]
pub fn resolve(files: Vec<FileSymbols>) -> Graph {
    // Global callable table: name → node_ids across all files.
    let mut global: HashMap<String, Vec<String>> = HashMap::new();
    for f in &files {
        for (name, id) in &f.callables {
            global.entry(name.clone()).or_default().push(id.clone());
        }
    }

    let mut nodes: Vec<GraphNode> = Vec::new();
    let mut edges: Vec<GraphEdge> = Vec::new();
    let mut ambiguous = 0usize;
    let mut unresolved = 0usize;

    for f in &files {
        nodes.extend(f.nodes.iter().cloned());
        edges.extend(f.contains.iter().cloned());

        // Per-file callable table for same-file-first resolution.
        let mut local: HashMap<&str, Vec<&str>> = HashMap::new();
        for (name, id) in &f.callables {
            local.entry(name.as_str()).or_default().push(id.as_str());
        }

        for (caller, callee) in &f.calls {
            let targets: Vec<String> = if let Some(local_ids) = local.get(callee.as_str()) {
                // Same-file definition(s) win.
                local_ids.iter().map(|s| (*s).to_string()).collect()
            } else {
                match global.get(callee).map(Vec::as_slice) {
                    Some([only]) => vec![only.clone()],
                    Some(many) if many.len() > 1 => {
                        ambiguous += 1;
                        Vec::new()
                    }
                    _ => {
                        unresolved += 1;
                        Vec::new()
                    }
                }
            };
            for target in targets {
                edges.push(GraphEdge {
                    source: caller.clone(),
                    target,
                    relation: "calls".to_string(),
                });
            }
        }
    }

    // Canonicalise: sort + dedup so output is deterministic (stable goldens, stable submit).
    nodes.sort();
    nodes.dedup();
    edges.sort();
    edges.dedup();

    tracing::debug!(
        nodes = nodes.len(),
        edges = edges.len(),
        ambiguous_calls = ambiguous,
        unresolved_calls = unresolved,
        "codegraph: resolved structural graph"
    );

    Graph { nodes, edges }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ts;

    fn graph_of(files: &[(&str, &str)]) -> Graph {
        let facts: Vec<FileSymbols> = files
            .iter()
            .map(|(path, src)| {
                let tree = ts::parse(src, "rust").expect("rust parses");
                extract_file(&tree, path, src, "rust")
            })
            .collect();
        resolve(facts)
    }

    #[test]
    fn emits_file_and_function_nodes_with_contains() {
        let g = graph_of(&[("src/math.rs", "fn add(a: i32, b: i32) -> i32 { a + b }\n")]);
        assert!(
            g.nodes
                .iter()
                .any(|n| n.node_id == "src/math.rs" && n.label == "math.rs")
        );
        let add = g.nodes.iter().find(|n| n.label == "add").expect("add node");
        assert_eq!(add.source_file, "src/math.rs");
        assert!(
            g.edges.iter().any(|e| e.relation == "contains"
                && e.source == "src/math.rs"
                && e.target == add.node_id),
            "file should contain add"
        );
    }

    #[test]
    fn intra_file_call_edge_is_built() {
        let src = "fn helper() {}\nfn caller() { helper(); }\n";
        let g = graph_of(&[("src/a.rs", src)]);
        let helper = g.nodes.iter().find(|n| n.label == "helper").unwrap();
        let caller = g.nodes.iter().find(|n| n.label == "caller").unwrap();
        assert!(
            g.edges.iter().any(|e| e.relation == "calls"
                && e.source == caller.node_id
                && e.target == helper.node_id),
            "caller → helper calls edge, got {:?}",
            g.edges
        );
    }

    #[test]
    fn cross_file_call_resolves_to_definition_in_another_file() {
        // The high-value case (ADR-0086): a reference in file A → a definition in file B.
        let a = ("src/a.rs", "fn caller() { target(); }\n");
        let b = ("src/b.rs", "fn target() {}\n");
        let g = graph_of(&[a, b]);
        let target = g.nodes.iter().find(|n| n.label == "target").unwrap();
        let caller = g.nodes.iter().find(|n| n.label == "caller").unwrap();
        assert_eq!(target.source_file, "src/b.rs");
        assert_eq!(caller.source_file, "src/a.rs");
        assert!(
            g.edges.iter().any(|e| e.relation == "calls"
                && e.source == caller.node_id
                && e.target == target.node_id),
            "cross-file caller → target edge missing; edges = {:?}",
            g.edges
        );
    }

    #[test]
    fn method_call_resolves_to_method_definition() {
        let src = "struct S;\nimpl S { fn run(&self) {} }\nfn go(s: &S) { s.run(); }\n";
        let g = graph_of(&[("src/s.rs", src)]);
        let run = g.nodes.iter().find(|n| n.label == "run").unwrap();
        let go = g.nodes.iter().find(|n| n.label == "go").unwrap();
        assert!(
            g.edges.iter().any(|e| e.relation == "calls"
                && e.source == go.node_id
                && e.target == run.node_id),
            "go → S::run method-call edge missing; edges = {:?}",
            g.edges
        );
    }

    #[test]
    fn ambiguous_global_name_is_not_guessed() {
        // Two files each define `dup`; a third calls `dup` with no local def → ambiguous → no edge.
        let files = &[
            ("src/x.rs", "fn dup() {}\n"),
            ("src/y.rs", "fn dup() {}\n"),
            ("src/z.rs", "fn caller() { dup(); }\n"),
        ];
        let g = graph_of(files);
        assert!(
            !g.edges.iter().any(|e| e.relation == "calls"),
            "ambiguous name must not produce a guessed calls edge, got {:?}",
            g.edges
        );
    }

    #[test]
    fn output_is_deterministic() {
        let files = &[
            ("src/b.rs", "fn target() {}\n"),
            ("src/a.rs", "fn caller() { target(); }\n"),
        ];
        let g1 = graph_of(files);
        let g2 = graph_of(files);
        assert_eq!(g1, g2, "resolve must be order-independent + canonical");
    }
}
