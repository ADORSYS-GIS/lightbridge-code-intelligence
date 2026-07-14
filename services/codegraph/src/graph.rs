//! Structural code graph (ADR-0086 §1). Built directly on tree-sitter — the same grammars the chunker
//! uses — as **edges on top of the per-file symbols** (definitions + their call sites).
//!
//! Output is payload-compatible with the graph payload the retired Graphify `graph.json` parse
//! produced (schema history, ADR-0019 → ADR-0086): [`GraphNode`] mirrors
//! `agent-clients::GraphNodePayload` (`node_id`, `label`, `source_file`, `start_line`) and
//! [`GraphEdge`] mirrors `GraphEdgePayload` (`source`, `target`, `relation`). The control plane's
//! generic `:Symbol` + `[:REL {relation}]` Neo4j write and the `graph_find_symbol` /
//! `graph_get_callers` retrieval tools are unchanged behind the seam.
//!
//! Languages with a real extractor today: **Rust** (ADR-0086 "Rust language first"), **Python**,
//! **TypeScript/JavaScript** (incl. JSX/TSX), and **Java**. Every language emits the SAME node/edge
//! vocabulary — the cross-file resolver ([`resolve`]/[`pick`]) is language-agnostic and shared
//! verbatim. Definitions and call references are identified by each grammar's bundled `tags.scm`
//! query ([`crate::tags`]); Rust keeps its own node-kind extractor for a byte-stable golden. See
//! [`Classifier`]. Relations emitted:
//! - `contains` — file → top-level def, and container def (mod/struct/trait/enum/class) → nested def.
//! - `method` — a type container (impl/trait/struct/enum/class/interface) → a callable it defines.
//!   This is a specialisation of `contains` kept separate for parity with Graphify (which emits
//!   `method`).
//! - `calls` — caller def → callee def, with **cross-file** resolution (a call in file A resolved to a
//!   definition in file B). `graph_get_callers` traverses this relation, so the name must match.
//!
//! Line numbers are **1-based** and callable labels carry a `()` suffix (`add` → `add()`), both for
//! parity with the Graphify `graph.json` schema this crate replaced.

use std::collections::HashMap;

use serde::Serialize;
use tree_sitter::{Node, Tree};

use crate::chunk::interesting_node;
use crate::lang::{self, GraphStrategy, LanguageSupport};
use crate::tags;

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

/// The per-file structural facts: the file's nodes (file node + defs), its intra-file `contains` /
/// `method` edges, and the unresolved call sites for the cross-file pass.
#[derive(Debug, Default)]
pub struct FileSymbols {
    nodes: Vec<GraphNode>,
    contains: Vec<GraphEdge>,
    /// Call sites attributed to their enclosing caller, resolved to `calls` edges once every file is
    /// known.
    calls: Vec<CallSite>,
    /// Callable defs (functions/methods) in this file, for same-file-first resolution.
    callables: Vec<Callable>,
}

/// A callable definition (`function`/`method`) recorded for call resolution.
#[derive(Debug, Clone)]
struct Callable {
    name: String,
    node_id: String,
    /// Enclosing type/class name (Rust `impl S` → `S`; a Python/TS/Java `class C` → `C`), used
    /// **only** as a tiebreaker to disambiguate several same-named callables. `None` for free
    /// functions.
    scope: Option<String>,
}

/// An unresolved call site: the enclosing caller def id, the bare callee name, and — when the call is
/// qualified by a receiver/path (`A::new`, `Foo.bar`) — that qualifier, used only to break same-name
/// ambiguity.
#[derive(Debug, Clone)]
struct CallSite {
    caller: String,
    name: String,
    qualifier: Option<String>,
}

/// Extract the structural facts for one **pre-parsed** file. `language` gates which languages produce
/// a graph (Rust, or any language [`crate::tags::extract`] handles); an unsupported language yields
/// empty facts (no structural graph — semantic search still covers it). `source_file` is the
/// repo-relative, forward-slashed path used as the file node id.
#[must_use]
pub fn extract_file(tree: &Tree, source_file: &str, source: &str, language: &str) -> FileSymbols {
    let mut facts = FileSymbols::default();
    // The definition/call *classifier* is chosen by the language's registry [`GraphStrategy`]: Rust
    // keeps its own node-kind extractor (byte-stable golden); every other language identifies defs +
    // calls via the grammar's bundled `tags.scm` query. An unknown language, or one with no graph
    // extractor, yields empty facts. The `tagged` binding owns the query result the
    // `Classifier::Tagged` variant borrows for the walk.
    let tagged;
    let classifier = match lang::by_id(language).and_then(LanguageSupport::graph_strategy) {
        Some(GraphStrategy::RustNative) => Classifier::Rust,
        Some(GraphStrategy::Tags(_)) => match tags::extract(language, tree, source) {
            Some(symbols) => {
                tagged = symbols;
                Classifier::Tagged(&tagged)
            }
            None => return facts,
        },
        None => return facts,
    };

    // The file node: id = the path, label = the file name, line 1 (1-based, matching Graphify's `L1`
    // file node). Top-level defs are `contains`-ed by it. For Python/JS the file *is* the module unit.
    facts.nodes.push(GraphNode {
        node_id: source_file.to_string(),
        label: file_label(source_file),
        source_file: source_file.to_string(),
        start_line: 1,
    });

    let bytes = source.as_bytes();
    let root = tree.root_node();
    // Stack of enclosing definition node ids (innermost last) for `contains` parenting + attributing
    // a call site to the definition it sits inside.
    let mut stack: Vec<String> = Vec::new();
    walk(
        classifier,
        &root,
        bytes,
        source_file,
        &mut stack,
        None,
        None,
        &mut facts,
    );
    facts
}

/// DFS that, in one pass, emits def nodes + `contains`/`method` edges and records call sites
/// attributed to the innermost enclosing def. `scope` is the nearest enclosing type name (for
/// same-name method disambiguation); `enclosing_kind` is the nearest enclosing def kind (to decide
/// `contains` vs `method`).
#[allow(clippy::too_many_arguments)]
fn walk(
    classifier: Classifier<'_>,
    node: &Node<'_>,
    bytes: &[u8],
    source_file: &str,
    stack: &mut Vec<String>,
    scope: Option<&str>,
    enclosing_kind: Option<&str>,
    facts: &mut FileSymbols,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some((kind, name)) = classifier.classify(&child, bytes) {
            // 1-based line (Graphify parity).
            let start_line = child.start_position().row as i64 + 1;
            let node_id = def_node_id(source_file, start_line, kind, name.as_deref());
            let parent_id = stack
                .last()
                .cloned()
                .unwrap_or_else(|| source_file.to_string());
            let is_callable = kind == "function" || kind == "method";
            // A callable directly inside a type container is a *method* (Graphify emits `method`);
            // everything else nests via `contains`.
            let relation = if is_callable && is_type_container(enclosing_kind) {
                "method"
            } else {
                "contains"
            };
            facts.contains.push(GraphEdge {
                source: parent_id,
                target: node_id.clone(),
                relation: relation.to_string(),
            });
            // Label parity with Graphify: callables carry a `()` suffix (`add` → `add()`).
            let label = match name.clone() {
                Some(n) if is_callable => format!("{n}()"),
                Some(n) => n,
                None => kind.to_string(),
            };
            facts.nodes.push(GraphNode {
                node_id: node_id.clone(),
                label,
                source_file: source_file.to_string(),
                start_line,
            });
            // Functions/methods are callable — record for resolution, tagged with their type scope.
            if is_callable && let Some(n) = name.clone() {
                facts.callables.push(Callable {
                    name: n,
                    node_id: node_id.clone(),
                    scope: scope.map(str::to_string),
                });
            }
            // The type scope introduced for this def's children (qualifies nested methods).
            let child_scope: Option<String> =
                classifier.container_scope(&child, kind, name.as_deref(), bytes);
            stack.push(node_id);
            walk(
                classifier,
                &child,
                bytes,
                source_file,
                stack,
                child_scope.as_deref(),
                Some(kind),
                facts,
            );
            stack.pop();
        } else {
            if let Some(caller) = stack.last()
                && let Some(callee) = classifier.call_site(&child, bytes)
            {
                facts.calls.push(CallSite {
                    caller: caller.clone(),
                    name: callee.name,
                    qualifier: callee.qualifier,
                });
            }
            walk(
                classifier,
                &child,
                bytes,
                source_file,
                stack,
                scope,
                enclosing_kind,
                facts,
            );
        }
    }
}

/// True when a callable directly inside a def of this kind is a *method* (joined by a `method` edge)
/// rather than a plain nested `contains`. Type containers across languages: Rust `impl`/`trait`/
/// `struct`/`enum`, and the `class`/`interface`/`enum` of Python, TS/JS, and Java.
fn is_type_container(kind: Option<&str>) -> bool {
    matches!(
        kind,
        Some("impl" | "trait" | "struct" | "enum" | "class" | "interface")
    )
}

/// The definition/call *classifier* the shared walk consults. Two sources, one uniform result.
/// **Rust** keeps its own tree-sitter node-kind extractor (the chunker's `interesting_node` plus the
/// Rust `call_expression` navigation) so chunk and graph symbols stay in lock-step and the committed
/// golden is byte-stable. **Every other language** is classified by the grammar's bundled `tags.scm`
/// query ([`crate::tags`]). Everything downstream is identical regardless of source — the
/// `contains`/`method` edge choice, the type-scope threading, and the cross-file [`resolve`]/[`pick`]
/// resolver — so a call site's qualifier is always tree-derived (tags omit it).
#[derive(Clone, Copy)]
enum Classifier<'a> {
    Rust,
    Tagged(&'a tags::TaggedSymbols),
}

impl Classifier<'_> {
    /// Classify a node as a graph definition: `(kind, name)` for functions/methods/classes/containers,
    /// or `None` otherwise. `kind` drives callable-ness (`function`/`method`) and the type scope.
    fn classify(self, node: &Node<'_>, bytes: &[u8]) -> Option<(&'static str, Option<String>)> {
        match self {
            Classifier::Rust => interesting_node(node, bytes),
            Classifier::Tagged(tags) => tags
                .defs
                .get(&node.id())
                .map(|def| (def.kind, def.name.clone())),
        }
    }

    /// The type name a container def introduces for its children — used only to disambiguate several
    /// same-named callables (e.g. two classes each with `run`). `None` for non-containers.
    fn container_scope(
        self,
        node: &Node<'_>,
        kind: &str,
        name: Option<&str>,
        bytes: &[u8],
    ) -> Option<String> {
        match self {
            Classifier::Rust => match kind {
                // `impl S` / `impl T for S` — the implementing type is the scope, not the trait.
                "impl" => impl_type_name(node, bytes),
                "trait" | "struct" | "enum" | "class" => name.map(str::to_string),
                _ => None,
            },
            // Tagged languages: a class/interface/enum scopes the methods it contains by its own name.
            Classifier::Tagged(_) => match kind {
                "class" | "interface" | "enum" => name.map(str::to_string),
                _ => None,
            },
        }
    }

    /// If `node` is a call site, return its callee reference (bare name + an optional type qualifier
    /// used solely as an ambiguity tiebreaker). For Rust this is the `call_expression` navigation; for
    /// tagged languages the node is the callee **name node** the query captured, and the qualifier is
    /// recovered from that node's receiver in the tree (tags drop it).
    fn call_site(self, node: &Node<'_>, bytes: &[u8]) -> Option<CalleeRef> {
        match self {
            Classifier::Rust => (node.kind() == "call_expression")
                .then(|| callee_ref_of(node, bytes))
                .flatten(),
            Classifier::Tagged(tags) => tags.calls.get(&node.id()).map(|name| CalleeRef {
                name: name.clone(),
                qualifier: qualifier_from_callee_node(node, bytes),
            }),
        }
    }
}

/// The qualifier for a tags-captured callee name node: its receiver, when the name is the property of
/// a member access (`Foo.bar()` → `Foo`; Python `obj.m()` / Java `Obj.m()`). A bare call
/// (`function`-field identifier) or a `self`/`this`/`cls`/`super` receiver yields no qualifier — the
/// call resolves on the bare name (single hit) or is dropped as ambiguous, never mis-attributed.
fn qualifier_from_callee_node(name_node: &Node<'_>, bytes: &[u8]) -> Option<String> {
    let parent = name_node.parent()?;
    match parent.kind() {
        // JS/TS `obj.foo()`, Python `obj.foo()`, Java `obj.foo()` — the receiver is the `object` field.
        "member_expression" | "attribute" | "method_invocation" => parent
            .child_by_field_name("object")
            .and_then(|object| receiver_qualifier(&object, bytes)),
        _ => None,
    }
}

/// A receiver object as a qualifier, iff it is a plain identifier that could name a *type* (`Foo` in
/// `Foo.bar()`). Implicit-`self` receivers (`self`/`cls`/`this`/`super`) carry no type information, so
/// they yield no qualifier — the call resolves on the bare method name (single hit) or is dropped as
/// ambiguous, never mis-attributed by a bogus `self` qualifier.
fn receiver_qualifier(object: &Node<'_>, bytes: &[u8]) -> Option<String> {
    if object.kind() != "identifier" {
        return None;
    }
    let name = text(object, bytes)?;
    match name.as_str() {
        "self" | "cls" | "this" | "super" => None,
        _ => Some(name),
    }
}

/// The callee of a Rust `call_expression`: its bare name plus, for a qualified path, the qualifier
/// segment. `A::new()` → `{name: "new", qualifier: Some("A")}`; `a::b::foo()` → `{"foo", Some("b")}`;
/// `foo()` / `x.foo()` → no qualifier (a method receiver's type is not known without inference).
struct CalleeRef {
    name: String,
    qualifier: Option<String>,
}

fn callee_ref_of(call: &Node<'_>, bytes: &[u8]) -> Option<CalleeRef> {
    let func = call.child_by_field_name("function")?;
    callee_ref(&func, bytes)
}

fn callee_ref(node: &Node<'_>, bytes: &[u8]) -> Option<CalleeRef> {
    match node.kind() {
        "identifier" | "field_identifier" | "type_identifier" => Some(CalleeRef {
            name: text(node, bytes)?,
            qualifier: None,
        }),
        // `a::b::foo` — callable is the final `name`; qualifier is the segment before it (`b`).
        "scoped_identifier" => {
            let name = node
                .child_by_field_name("name")
                .and_then(|n| text(&n, bytes))?;
            let qualifier = node
                .child_by_field_name("path")
                .and_then(|p| path_tail(&p, bytes));
            Some(CalleeRef { name, qualifier })
        }
        // `x.foo` — the method name is the `field`; the receiver type is unknown, so no qualifier.
        "field_expression" => node
            .child_by_field_name("field")
            .and_then(|n| callee_ref(&n, bytes)),
        // `foo::<T>` — the callable is under the `function` field.
        "generic_function" => node
            .child_by_field_name("function")
            .and_then(|n| callee_ref(&n, bytes)),
        _ => None,
    }
}

/// The last segment of a path node — the receiver/namespace qualifier (`A` in `A`, `b` in `a::b`).
fn path_tail(node: &Node<'_>, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "scoped_identifier" => node
            .child_by_field_name("name")
            .and_then(|n| text(&n, bytes)),
        _ => text(node, bytes),
    }
}

/// The implementing type of an `impl_item` (`impl S` / `impl T for S` / `impl Vec<T>` → `S` / `Vec`),
/// used to scope the methods it contains for same-name disambiguation.
fn impl_type_name(node: &Node<'_>, bytes: &[u8]) -> Option<String> {
    let ty = node.child_by_field_name("type")?;
    type_head_name(&ty, bytes)
}

fn type_head_name(node: &Node<'_>, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "type_identifier" => text(node, bytes),
        "generic_type" => node
            .child_by_field_name("type")
            .and_then(|n| type_head_name(&n, bytes)),
        "scoped_type_identifier" => node
            .child_by_field_name("name")
            .and_then(|n| text(&n, bytes)),
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

/// Outcome of resolving one call against a candidate set.
enum Pick<'a> {
    /// Exactly one definition matched — emit the edge.
    One(&'a str),
    /// Several same-named defs and no qualifier singles one out — drop and count (never fan out).
    Ambiguous,
    /// Nothing matched here — try the next table (or count as unresolved).
    None,
}

/// Resolve a call against `candidates` (all defs sharing the callee's bare name). A single candidate
/// resolves unless a **type** qualifier positively contradicts it (a *module* qualifier — the
/// candidate has no type scope — still matches, so `math::add` resolves to a free `add`). Several
/// candidates are [`Pick::Ambiguous`] unless the qualifier singles exactly one out — we never emit an
/// edge to every same-named def (the same-file fan-out bug).
fn pick<'a>(candidates: Option<&[&'a Callable]>, qualifier: Option<&str>) -> Pick<'a> {
    let candidates = match candidates {
        Some(c) if !c.is_empty() => c,
        _ => return Pick::None,
    };
    if let [only] = candidates {
        if let (Some(q), Some(scope)) = (qualifier, only.scope.as_deref())
            && q != scope
        {
            return Pick::None;
        }
        return Pick::One(only.node_id.as_str());
    }
    // Genuine same-name ambiguity: a type qualifier is the only thing that can break it.
    if let Some(q) = qualifier {
        let mut narrowed = candidates.iter().filter(|c| c.scope.as_deref() == Some(q));
        if let Some(first) = narrowed.next()
            && narrowed.next().is_none()
        {
            return Pick::One(first.node_id.as_str());
        }
    }
    Pick::Ambiguous
}

/// Resolve every file's call sites into `calls` edges with cross-file resolution, and return the
/// canonicalised whole-repo [`Graph`]. Resolution policy (precision-favouring, ADR-0086 R5):
/// same-file definitions win; otherwise the global table is consulted. In **both** tables a call
/// resolves only to a **single** matching definition — a bare name matching several same-named defs
/// is **dropped and counted, not fanned out** to every candidate; a path qualifier (`A::new`) is used
/// solely as a tiebreaker to single out the right one when there is genuine ambiguity.
#[must_use]
pub fn resolve(files: Vec<FileSymbols>) -> Graph {
    // Global callable table: name → all callables across files (for cross-file resolution).
    let mut global: HashMap<&str, Vec<&Callable>> = HashMap::new();
    for f in &files {
        for c in &f.callables {
            global.entry(c.name.as_str()).or_default().push(c);
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
        let mut local: HashMap<&str, Vec<&Callable>> = HashMap::new();
        for c in &f.callables {
            local.entry(c.name.as_str()).or_default().push(c);
        }

        for call in &f.calls {
            let qualifier = call.qualifier.as_deref();
            let local_pick = pick(local.get(call.name.as_str()).map(Vec::as_slice), qualifier);
            // Same-file definitions win on a clean single hit. Otherwise — no local match, OR a local
            // set too ambiguous to attribute — consult the global table, where a qualifier may still
            // single out exactly one definition (e.g. locals `A::new`+`B::new` don't shadow a global
            // `C::new()` call). A local ambiguity is only *counted* ambiguous if the global fails too.
            let local_ambiguous = matches!(local_pick, Pick::Ambiguous);
            let target = match local_pick {
                Pick::One(id) => Some(id.to_string()),
                Pick::None | Pick::Ambiguous => {
                    match pick(global.get(call.name.as_str()).map(Vec::as_slice), qualifier) {
                        Pick::One(id) => Some(id.to_string()),
                        Pick::Ambiguous => {
                            ambiguous += 1;
                            None
                        }
                        Pick::None => {
                            if local_ambiguous {
                                ambiguous += 1;
                            } else {
                                unresolved += 1;
                            }
                            None
                        }
                    }
                }
            };
            if let Some(target) = target {
                edges.push(GraphEdge {
                    source: call.caller.clone(),
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
    use crate::lang;

    fn graph_of_lang(language: &str, files: &[(&str, &str)]) -> Graph {
        let facts: Vec<FileSymbols> = files
            .iter()
            .map(|(path, src)| {
                let tree = lang::parse(src, language).expect("source parses");
                extract_file(&tree, path, src, language)
            })
            .collect();
        resolve(facts)
    }

    fn graph_of(files: &[(&str, &str)]) -> Graph {
        graph_of_lang("rust", files)
    }

    /// Find the single node with `label`, panicking with the graph on failure (test ergonomics).
    fn node<'a>(g: &'a Graph, label: &str) -> &'a GraphNode {
        g.nodes
            .iter()
            .find(|n| n.label == label)
            .unwrap_or_else(|| panic!("no node labelled {label:?}; nodes = {:?}", g.nodes))
    }

    /// True iff a `calls` edge `caller → callee` (by label) exists.
    fn has_call(g: &Graph, caller: &str, callee: &str) -> bool {
        let src = node(g, caller).node_id.as_str();
        let dst = node(g, callee).node_id.as_str();
        g.edges
            .iter()
            .any(|e| e.relation == "calls" && e.source == src && e.target == dst)
    }

    #[test]
    fn emits_file_and_function_nodes_with_contains() {
        let g = graph_of(&[("src/math.rs", "fn add(a: i32, b: i32) -> i32 { a + b }\n")]);
        assert!(
            g.nodes
                .iter()
                .any(|n| n.node_id == "src/math.rs" && n.label == "math.rs")
        );
        let add = g
            .nodes
            .iter()
            .find(|n| n.label == "add()")
            .expect("add node");
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
        let helper = g.nodes.iter().find(|n| n.label == "helper()").unwrap();
        let caller = g.nodes.iter().find(|n| n.label == "caller()").unwrap();
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
        let target = g.nodes.iter().find(|n| n.label == "target()").unwrap();
        let caller = g.nodes.iter().find(|n| n.label == "caller()").unwrap();
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
        let run = g.nodes.iter().find(|n| n.label == "run()").unwrap();
        let go = g.nodes.iter().find(|n| n.label == "go()").unwrap();
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
    fn method_definition_emits_a_method_edge_not_contains() {
        // Parity with Graphify: a callable inside a type container is joined by `method`, not
        // `contains`.
        let src = "struct S;\nimpl S { fn run(&self) {} }\n";
        let g = graph_of(&[("src/s.rs", src)]);
        let run = g.nodes.iter().find(|n| n.label == "run()").unwrap();
        // The enclosing container is the `impl S` node (codegraph emits an anonymous `impl` node, not
        // the named type — this asserts codegraph's own SELF-CONSISTENCY: the `method` edge's source
        // is exactly that container node. Whether Graphify anchors `method` at the type name instead
        // is a separate side-by-side parity check still open on the #360 gate.
        let impl_node = g
            .nodes
            .iter()
            .find(|n| n.label == "impl")
            .expect("impl container node");
        let method_edge = g
            .edges
            .iter()
            .find(|e| e.relation == "method" && e.target == run.node_id)
            .unwrap_or_else(|| panic!("S::run must be a `method` edge; edges = {:?}", g.edges));
        assert_eq!(
            method_edge.source, impl_node.node_id,
            "method edge must originate from the `impl S` container node, not elsewhere"
        );
        assert!(
            !g.edges
                .iter()
                .any(|e| e.relation == "contains" && e.target == run.node_id),
            "S::run must NOT also be a `contains` target; edges = {:?}",
            g.edges
        );
    }

    #[test]
    fn ambiguous_local_does_not_shadow_a_qualified_global() {
        // file a: two colliding `new`s + a call to `C::new()` (a *different* type). The local set is
        // ambiguous, but the qualifier singles out the global `C::new` in file c — the edge must
        // resolve cross-file, not be dropped (P2-c).
        let a = (
            "src/a.rs",
            "\
struct A;
struct B;
impl A { fn new() -> A { A } }
impl B { fn new() -> B { B } }
fn make() { let _ = C::new(); }
",
        );
        let c = ("src/c.rs", "struct C;\nimpl C { fn new() -> C { C } }\n");
        let g = graph_of(&[a, c]);
        let make = g.nodes.iter().find(|n| n.label == "make()").unwrap();
        let c_new = g
            .nodes
            .iter()
            .find(|n| n.label == "new()" && n.source_file == "src/c.rs")
            .expect("C::new in c.rs");
        let calls: Vec<_> = g
            .edges
            .iter()
            .filter(|e| e.relation == "calls" && e.source == make.node_id)
            .collect();
        assert_eq!(calls.len(), 1, "one cross-file edge; got {calls:?}");
        assert_eq!(
            calls[0].target, c_new.node_id,
            "ambiguous locals must not shadow the qualified global C::new"
        );
    }

    #[test]
    fn labels_and_lines_match_graphify_shape() {
        // 1-based lines + `()` on callables (Graphify parity).
        let g = graph_of(&[("src/m.rs", "\nfn add() {}\n")]); // `add` on source line 2
        let add = g.nodes.iter().find(|n| n.label == "add()").unwrap();
        assert_eq!(add.start_line, 2, "1-based start_line");
        let file = g.nodes.iter().find(|n| n.node_id == "src/m.rs").unwrap();
        assert_eq!(file.start_line, 1, "file node is line 1 like Graphify's L1");
    }

    #[test]
    fn same_file_qualified_call_hits_the_right_constructor() {
        // Two impls in one file both define `new`. `A::new()` must resolve to A's `new` only — the
        // old same-file branch fanned out to BOTH (the bug this fixes).
        let src = "\
struct A;
struct B;
impl A { fn new() -> A { A } }
impl B { fn new() -> B { B } }
fn make() { let _ = A::new(); }
";
        let g = graph_of(&[("src/lib.rs", src)]);
        let make = g.nodes.iter().find(|n| n.label == "make()").unwrap();
        // `impl A { fn new ... }` are one source line: A::new is line 3, B::new line 4.
        let a_new = g
            .nodes
            .iter()
            .find(|n| n.label == "new()" && n.start_line == 3)
            .expect("A::new at line 3");
        let b_new = g
            .nodes
            .iter()
            .find(|n| n.label == "new()" && n.start_line == 4)
            .expect("B::new at line 4");
        let calls: Vec<_> = g
            .edges
            .iter()
            .filter(|e| e.relation == "calls" && e.source == make.node_id)
            .collect();
        assert_eq!(
            calls.len(),
            1,
            "exactly one calls edge (no fan-out); got {calls:?}"
        );
        assert_eq!(calls[0].target, a_new.node_id, "must resolve to A::new");
        assert_ne!(calls[0].target, b_new.node_id, "must NOT also hit B::new");
    }

    #[test]
    fn same_file_bare_duplicate_call_is_dropped_not_fanned_out() {
        // `default` on two impls; a *bare* `default()` can't be attributed → dropped, not one edge
        // to each definition.
        let src = "\
struct A;
struct B;
impl A { fn default() -> A { A } }
impl B { fn default() -> B { B } }
fn make() { let _ = default(); }
";
        let g = graph_of(&[("src/lib.rs", src)]);
        assert!(
            !g.edges.iter().any(|e| e.relation == "calls"),
            "ambiguous bare call must be dropped, not fanned out; edges = {:?}",
            g.edges
        );
    }

    #[test]
    fn qualified_call_disambiguates_from_and_build_across_impls() {
        // A mix of `from`/`build` constructors on colliding types; each qualified call lands on its
        // own type's method and nothing else.
        let src = "\
struct Meters;
struct Feet;
impl Meters { fn from(v: i32) -> Meters { Meters } fn build() -> Meters { Meters } }
impl Feet { fn from(v: i32) -> Feet { Feet } fn build() -> Feet { Feet } }
fn go() { let _ = Feet::from(3); let _ = Meters::build(); }
";
        let g = graph_of(&[("src/u.rs", src)]);
        let go = g.nodes.iter().find(|n| n.label == "go()").unwrap();
        let calls: Vec<_> = g
            .edges
            .iter()
            .filter(|e| e.relation == "calls" && e.source == go.node_id)
            .collect();
        assert_eq!(calls.len(), 2, "two resolved edges; got {calls:?}");
        // `Feet::from` and `Meters::build` are the only correct targets.
        // `impl Meters {...}` is line 3, `impl Feet {...}` line 4 (each impl + its methods one line).
        let feet_from = g
            .nodes
            .iter()
            .find(|n| n.label == "from()" && n.start_line == 4)
            .expect("Feet::from at line 4");
        let meters_build = g
            .nodes
            .iter()
            .find(|n| n.label == "build()" && n.start_line == 3)
            .expect("Meters::build at line 3");
        assert!(calls.iter().any(|e| e.target == feet_from.node_id));
        assert!(calls.iter().any(|e| e.target == meters_build.node_id));
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

    // ── Python ──────────────────────────────────────────────────────────────────────────────────

    #[test]
    fn python_free_function_contains_and_intra_file_call() {
        let src = "def helper():\n    pass\n\ndef caller():\n    helper()\n";
        let g = graph_of_lang("python", &[("app/util.py", src)]);
        let file = node(&g, "util.py");
        assert_eq!(file.node_id, "app/util.py");
        let helper = node(&g, "helper()");
        assert_eq!(helper.start_line, 1, "1-based def line");
        assert!(
            g.edges.iter().any(|e| e.relation == "contains"
                && e.source == "app/util.py"
                && e.target == helper.node_id),
            "file → helper contains edge; edges = {:?}",
            g.edges
        );
        assert!(has_call(&g, "caller()", "helper()"), "caller → helper");
    }

    #[test]
    fn python_class_method_emits_method_edge_and_self_call_resolves() {
        // A class with two methods; `describe` calls `self.area()`. `area` is a `method` edge off the
        // class node, and the self-call resolves to the single `area` despite the `self` receiver.
        let src = "\
class Circle:
    def area(self):
        return 3.14

    def describe(self):
        return self.area()
";
        let g = graph_of_lang("python", &[("shapes.py", src)]);
        let class = node(&g, "Circle");
        let area = node(&g, "area()");
        assert!(
            g.edges.iter().any(|e| e.relation == "method"
                && e.source == class.node_id
                && e.target == area.node_id),
            "Circle → area must be a `method` edge; edges = {:?}",
            g.edges
        );
        assert!(
            !g.edges
                .iter()
                .any(|e| e.relation == "contains" && e.target == area.node_id),
            "a method must not also be a `contains` target"
        );
        assert!(
            has_call(&g, "describe()", "area()"),
            "self.area() must resolve to Circle.area; edges = {:?}",
            g.edges
        );
    }

    #[test]
    fn python_cross_file_call_resolves() {
        let a = ("pkg/a.py", "def caller():\n    target()\n");
        let b = ("pkg/b.py", "def target():\n    pass\n");
        let g = graph_of_lang("python", &[a, b]);
        assert_eq!(node(&g, "target()").source_file, "pkg/b.py");
        assert!(
            has_call(&g, "caller()", "target()"),
            "cross-file caller → target"
        );
    }

    #[test]
    fn python_colliding_method_names_qualified_resolves_bare_dropped() {
        // Two classes each define `new`; `Circle.new()` resolves to Circle's only, a bare `new()`
        // is ambiguous and dropped (never fanned out to both).
        let src = "\
class Circle:
    def new(self):
        return self

class Square:
    def new(self):
        return self

def build():
    return Circle.new()

def build_bare():
    return new()
";
        let g = graph_of_lang("python", &[("shapes.py", src)]);
        let circle_new = g
            .nodes
            .iter()
            .find(|n| n.label == "new()" && n.start_line == 2)
            .expect("Circle.new at line 2");
        let build_calls: Vec<_> = g
            .edges
            .iter()
            .filter(|e| e.relation == "calls" && e.source == node(&g, "build()").node_id)
            .collect();
        assert_eq!(
            build_calls.len(),
            1,
            "one qualified edge; got {build_calls:?}"
        );
        assert_eq!(
            build_calls[0].target, circle_new.node_id,
            "→ Circle.new only"
        );
        assert!(
            !g.edges
                .iter()
                .any(|e| e.relation == "calls" && e.source == node(&g, "build_bare()").node_id),
            "bare ambiguous new() must be dropped; edges = {:?}",
            g.edges
        );
    }

    #[test]
    fn python_decorated_def_is_named_not_wrapped() {
        // `@decorator` must not spawn an anonymous wrapper node — the def keeps its real name/line.
        let src = "import functools\n\n@functools.cache\ndef cached():\n    return 1\n";
        let g = graph_of_lang("python", &[("m.py", src)]);
        let cached = node(&g, "cached()");
        assert_eq!(cached.start_line, 4, "def line, not the decorator line");
        assert!(
            g.edges.iter().any(|e| e.relation == "contains"
                && e.source == "m.py"
                && e.target == cached.node_id),
            "decorated def is contained by the file directly (no wrapper); edges = {:?}",
            g.edges
        );
        assert!(
            !g.nodes.iter().any(|n| n.label == "function"),
            "no anonymous `function` wrapper node; nodes = {:?}",
            g.nodes
        );
    }

    // ── TypeScript / JavaScript ─────────────────────────────────────────────────────────────────

    #[test]
    fn ts_function_declaration_and_call() {
        let src = "function helper() {}\nfunction caller() { helper(); }\n";
        let g = graph_of_lang("typescript", &[("src/a.ts", src)]);
        assert!(has_call(&g, "caller()", "helper()"), "caller → helper");
    }

    #[test]
    fn ts_named_arrow_const_is_a_callable() {
        // The dominant modern form: `const helper = () => {}`. It must be a named, callable def.
        let src = "const helper = () => {};\nfunction caller() { helper(); }\n";
        let g = graph_of_lang("typescript", &[("src/a.ts", src)]);
        let helper = node(&g, "helper()");
        assert!(
            g.edges.iter().any(|e| e.relation == "contains"
                && e.source == "src/a.ts"
                && e.target == helper.node_id),
            "file → const-arrow helper; edges = {:?}",
            g.edges
        );
        assert!(
            has_call(&g, "caller()", "helper()"),
            "caller → const-arrow helper"
        );
    }

    #[test]
    fn ts_class_method_edge_and_this_call() {
        let src = "\
class Service {
  run() {}
  go() { this.run(); }
}
";
        let g = graph_of_lang("typescript", &[("src/s.ts", src)]);
        let class = node(&g, "Service");
        let run = node(&g, "run()");
        assert!(
            g.edges.iter().any(|e| e.relation == "method"
                && e.source == class.node_id
                && e.target == run.node_id),
            "Service → run `method` edge; edges = {:?}",
            g.edges
        );
        assert!(
            has_call(&g, "go()", "run()"),
            "this.run() resolves to Service.run"
        );
    }

    #[test]
    fn ts_cross_file_call_resolves() {
        let a = (
            "src/a.ts",
            "import { target } from './b';\nfunction caller() { target(); }\n",
        );
        let b = ("src/b.ts", "export function target() {}\n");
        let g = graph_of_lang("typescript", &[a, b]);
        assert_eq!(node(&g, "target()").source_file, "src/b.ts");
        assert!(
            has_call(&g, "caller()", "target()"),
            "cross-file caller → target"
        );
    }

    #[test]
    fn ts_colliding_methods_qualified_resolves_bare_dropped() {
        let src = "\
class Circle { make() { return this; } }
class Square { make() { return this; } }
function build() { return Circle.make(); }
function buildBare(m) { return make(); }
";
        let g = graph_of_lang("typescript", &[("src/shapes.ts", src)]);
        let circle_make = g
            .nodes
            .iter()
            .find(|n| n.label == "make()" && n.start_line == 1)
            .expect("Circle.make at line 1");
        let build_calls: Vec<_> = g
            .edges
            .iter()
            .filter(|e| e.relation == "calls" && e.source == node(&g, "build()").node_id)
            .collect();
        assert_eq!(
            build_calls.len(),
            1,
            "one qualified edge; got {build_calls:?}"
        );
        assert_eq!(
            build_calls[0].target, circle_make.node_id,
            "→ Circle.make only"
        );
        assert!(
            !g.edges
                .iter()
                .any(|e| e.relation == "calls" && e.source == node(&g, "buildBare()").node_id),
            "bare ambiguous make() must be dropped; edges = {:?}",
            g.edges
        );
    }

    #[test]
    fn tsx_component_uses_the_jsx_grammar() {
        // A `.tsx` component returning JSX must parse via the TSX grammar (the plain TS grammar chokes
        // on JSX). Assert the component + a hook call resolve, proving the JSX body didn't wreck the
        // parse.
        let src = "\
function useThing() { return 1; }
export const Widget = () => {
  const n = useThing();
  return <div>{n}</div>;
};
";
        let g = graph_of_lang("tsx", &[("src/Widget.tsx", src)]);
        assert!(
            has_call(&g, "Widget()", "useThing()"),
            "Widget → useThing across a JSX body; edges = {:?}",
            g.edges
        );
    }

    #[test]
    fn javascript_jsx_arrow_component_is_extracted() {
        // JSX in a `.jsx`/`.js` file must still parse + yield the named component and its call.
        let src = "\
const Button = () => { return null; };
function App() { return Button(); }
";
        let g = graph_of_lang("javascript", &[("src/app.jsx", src)]);
        assert!(
            has_call(&g, "App()", "Button()"),
            "App → Button; edges = {:?}",
            g.edges
        );
    }

    // ── Java (stretch) ──────────────────────────────────────────────────────────────────────────

    #[test]
    fn java_class_method_edge_and_this_call() {
        let src = "\
class Greeter {
    String hello() { return \"hi\"; }
    String greet() { return this.hello(); }
}
";
        let g = graph_of_lang("java", &[("Greeter.java", src)]);
        let class = node(&g, "Greeter");
        let hello = node(&g, "hello()");
        assert!(
            g.edges.iter().any(|e| e.relation == "method"
                && e.source == class.node_id
                && e.target == hello.node_id),
            "Greeter → hello `method` edge; edges = {:?}",
            g.edges
        );
        assert!(
            has_call(&g, "greet()", "hello()"),
            "this.hello() → Greeter.hello"
        );
    }

    #[test]
    fn java_cross_file_qualified_static_call_resolves() {
        let a = (
            "Caller.java",
            "class Caller { void run() { Helper.help(); } }\n",
        );
        let b = ("Helper.java", "class Helper { static void help() {} }\n");
        let g = graph_of_lang("java", &[a, b]);
        assert_eq!(node(&g, "help()").source_file, "Helper.java");
        assert!(
            has_call(&g, "run()", "help()"),
            "Helper.help() resolves cross-file"
        );
    }

    #[test]
    fn java_colliding_methods_qualified_resolves() {
        let src = "\
class Circle { Circle make() { return this; } }
class Square { Square make() { return this; } }
class Builder { Object build() { return Circle.make(); } }
";
        let g = graph_of_lang("java", &[("Shapes.java", src)]);
        let circle_make = g
            .nodes
            .iter()
            .find(|n| n.label == "make()" && n.start_line == 1)
            .expect("Circle.make at line 1");
        let build_calls: Vec<_> = g
            .edges
            .iter()
            .filter(|e| e.relation == "calls" && e.source == node(&g, "build()").node_id)
            .collect();
        assert_eq!(
            build_calls.len(),
            1,
            "one qualified edge; got {build_calls:?}"
        );
        assert_eq!(
            build_calls[0].target, circle_make.node_id,
            "→ Circle.make only"
        );
    }
}
