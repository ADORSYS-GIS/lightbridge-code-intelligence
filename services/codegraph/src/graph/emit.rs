//! Node/edge emission: the tree-sitter DFS that turns one parsed file into [`super::FileSymbols`] —
//! def nodes, intra-file `contains`/`method` edges, and the unresolved call sites the cross-file
//! [`super::resolve`] pass consumes. The definition/call *classifier* ([`Classifier`]) is the seam
//! between this shared walk and the two ways a language identifies defs + calls: Rust's own
//! tree-sitter node-kind extractor (byte-stable golden), or the grammar's bundled `tags.scm` query
//! ([`crate::tags`]) every other language uses.

use tree_sitter::{Node, Tree};

use super::callee::{self, CalleeRef};
use super::{CallSite, Callable, FileSymbols, GraphEdge, GraphNode};
use crate::chunk::interesting_node;
use crate::lang::{self, GraphStrategy, LanguageSupport};
use crate::tags;

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
/// `contains`/`method` edge choice, the type-scope threading, and the cross-file
/// [`super::resolve::resolve`]/[`super::resolve::pick`] resolver — so a call site's qualifier is always
/// tree-derived (tags omit it).
#[derive(Clone, Copy)]
pub(super) enum Classifier<'a> {
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
                "impl" => callee::impl_type_name(node, bytes),
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
                .then(|| callee::callee_ref_of(node, bytes))
                .flatten(),
            Classifier::Tagged(tags) => tags.calls.get(&node.id()).map(|name| CalleeRef {
                name: name.clone(),
                qualifier: callee::qualifier_from_callee_node(node, bytes),
            }),
        }
    }
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
