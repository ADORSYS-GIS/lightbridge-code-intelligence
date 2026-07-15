//! Definition + call-reference identification via each grammar's **bundled `tags.scm`** query
//! (ADR-0086 language expansion). Rather than hand-maintain per-language node-kind lists, we run the
//! `TAGS_QUERY` the grammar authors ship and maintain — it already tags function/method/class
//! definitions (`@definition.*`) and call references (`@reference.call`) with the symbol `@name`.
//!
//! What the tags query gives us is *what is a definition/call and its name*. What it deliberately does
//! NOT give — and what [`crate::graph`] still derives from the tree — is (a) **containment/scope** (a
//! flat tag list has no "this method belongs to that class") and (b) the **call qualifier** (a
//! `Circle.new()` tag captures only `new`, dropping the `Circle` receiver the resolver uses to break
//! same-name ambiguity). So this module is the *classifier*; the graph walk owns structure + qualifier
//! + the shared cross-file resolver.
//!
//! Version notes for the pinned grammars (verified by test): the TypeScript `tags.scm` covers only
//! TS-specific nodes (signatures/interface/module) and must be **composed with the JavaScript query**
//! for concrete `class`/`function`/`method`/`call` — so `typescript`/`tsx` run `JS_TAGS ⧺ TS_TAGS`.
//! The JavaScript query compiles and matches against both the TS and TSX grammars.

use std::collections::HashMap;

use tree_sitter::{Node, QueryCursor, StreamingIterator, Tree};

use crate::lang::{self, GraphStrategy};

/// One tagged definition: the graph node kind (normalised across languages) and its symbol name.
#[derive(Debug, Clone)]
pub struct DefTag {
    pub kind: &'static str,
    pub name: Option<String>,
}

/// The per-file classification the tags query yields, keyed by tree-node id (stable within one parse):
/// `defs` maps a definition node → its kind+name; `calls` maps a callee **name node** → the bare callee
/// name. The graph walk consults these instead of a hand-written node-kind match.
#[derive(Debug, Default)]
pub struct TaggedSymbols {
    pub defs: HashMap<usize, DefTag>,
    /// Callee name-node id → bare callee name. The node is the identifier the resolver keys on; the
    /// graph derives the qualifier from that node's tree position (its receiver), which tags omit.
    pub calls: HashMap<usize, String>,
}

/// Run the bundled tags query for `language` over a pre-parsed `tree`. Returns `None` for languages
/// with no tags pipeline (Rust keeps its own extractor to hold the byte-stable golden; an unknown
/// language has no grammar). The compiled query is owned + cached by the language's
/// [`crate::lang::LanguageSupport`] impl ([`GraphStrategy::Tags`]), so there is no per-language
/// `match` here.
#[must_use]
pub fn extract(language: &str, tree: &Tree, source: &str) -> Option<TaggedSymbols> {
    let query = match lang::by_id(language)?.graph_strategy()? {
        GraphStrategy::Tags(query) => query,
        GraphStrategy::RustNative => return None,
    };
    let names = query.capture_names();
    let mut out = TaggedSymbols::default();
    let mut cursor = QueryCursor::new();
    let mut it = cursor.matches(query, tree.root_node(), source.as_bytes());
    while let Some(m) = it.next() {
        let mut def_node: Option<Node<'_>> = None;
        let mut def_kind: Option<&'static str> = None;
        let mut name_node: Option<Node<'_>> = None;
        let mut is_call = false;
        for cap in m.captures {
            match names[cap.index as usize] {
                "name" => name_node = Some(cap.node),
                "reference.call" => is_call = true,
                capture if capture.starts_with("definition.") => {
                    def_node = Some(cap.node);
                    def_kind = map_def_kind(&capture["definition.".len()..]);
                }
                // `@reference.class` (new/superclass), `@reference.type`, `@doc`, etc. — not graph
                // definitions or callable references; ignored.
                _ => {}
            }
        }
        match (def_node, def_kind) {
            (Some(node), Some(kind)) => {
                let name = name_node.and_then(|n| node_text(&n, source));
                out.defs.insert(node.id(), DefTag { kind, name });
            }
            _ if is_call => {
                if let Some(nn) = name_node
                    && let Some(name) = node_text(&nn, source)
                {
                    out.calls.insert(nn.id(), name);
                }
            }
            _ => {}
        }
    }
    Some(out)
}

/// Map a `tags.scm` `@definition.<suffix>` capture to a graph node kind, or `None` to skip. We keep
/// callables + type containers (the graph's vocabulary) and drop `constant`/`module` (module-level
/// consts and TS namespaces are not call-graph material and were not emitted before).
fn map_def_kind(suffix: &str) -> Option<&'static str> {
    match suffix {
        "function" => Some("function"),
        "method" => Some("method"),
        "class" => Some("class"),
        "interface" => Some("interface"),
        "enum" => Some("enum"),
        _ => None,
    }
}

fn node_text(node: &Node<'_>, source: &str) -> Option<String> {
    source.get(node.byte_range()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang;

    fn tagged(language: &str, code: &str) -> TaggedSymbols {
        let tree = lang::parse(code, language).expect("parses");
        extract(language, &tree, code).expect("has a tags pipeline")
    }

    #[test]
    fn rust_has_no_tags_pipeline() {
        let tree = lang::parse("fn a() {}\n", "rust").unwrap();
        assert!(extract("rust", &tree, "fn a() {}\n").is_none());
    }

    #[test]
    fn python_tags_find_class_function_and_calls() {
        let t = tagged(
            "python",
            "class C:\n    def m(self):\n        return self.m()\ndef f():\n    C.m()\n",
        );
        let kinds: Vec<&str> = t.defs.values().map(|d| d.kind).collect();
        assert!(kinds.contains(&"class"), "class def: {:?}", t.defs);
        assert!(
            kinds.contains(&"function") || kinds.contains(&"method"),
            "callable def: {:?}",
            t.defs
        );
        assert!(!t.calls.is_empty(), "at least one call ref");
    }

    #[test]
    fn typescript_composes_js_query_for_concrete_constructs() {
        // The TS query alone captures no concrete class/function/method/call — composition is required.
        let t = tagged(
            "typescript",
            "class C { m(){ return this.m(); } }\nfunction g(){ new C(); g(); }\n",
        );
        let kinds: Vec<&str> = t.defs.values().map(|d| d.kind).collect();
        assert!(
            kinds.contains(&"class"),
            "class captured via composed query: {:?}",
            t.defs
        );
        assert!(kinds.contains(&"method"), "method captured: {:?}", t.defs);
        assert!(
            kinds.contains(&"function"),
            "function captured: {:?}",
            t.defs
        );
        assert!(!t.calls.is_empty(), "g() call captured");
    }

    #[test]
    fn tsx_uses_the_jsx_aware_grammar() {
        let t = tagged(
            "tsx",
            "const W = () => { return <div/>; };\nfunction A(){ return W(); }\n",
        );
        let names: Vec<Option<&str>> = t.defs.values().map(|d| d.name.as_deref()).collect();
        assert!(
            names.contains(&Some("W")),
            "arrow component tagged through JSX: {:?}",
            t.defs
        );
    }
}
