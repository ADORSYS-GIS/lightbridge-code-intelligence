//! TypeScript + TSX language support. Two grammars, one shared tags query.
//!
//! `.ts` uses the dedicated TypeScript grammar; `.tsx` needs the dedicated TSX grammar (the plain
//! TypeScript grammar does NOT parse JSX and would litter tsx files with error nodes, degrading both
//! chunking and graph extraction).
//!
//! The tags query is **composed**: the pinned TypeScript `tags.scm` covers only TS-specific nodes
//! (signatures/interface/module), so for concrete `class`/`function`/`method`/`call` it must be
//! composed with the JavaScript query (the TS grammar is a JS superset, so those JS node patterns
//! match). The JavaScript query compiles and matches against both the TS and TSX grammars.

use std::sync::OnceLock;

use tree_sitter::{Language, Query};

use super::{GraphStrategy, LanguageSupport};

/// The composed TypeScript/TSX tags query source: the JavaScript query (concrete
/// class/function/method/call) followed by the TS-specific query (signatures/interface/module).
fn composed_tags_source() -> String {
    format!(
        "{}\n{}",
        tree_sitter_javascript::TAGS_QUERY,
        tree_sitter_typescript::TAGS_QUERY
    )
}

pub struct TypeScript;

impl LanguageSupport for TypeScript {
    fn id(&self) -> &'static str {
        "typescript"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["ts"]
    }

    fn ts_language(&self) -> Language {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    }

    fn graph_strategy(&self) -> Option<GraphStrategy> {
        static QUERY: OnceLock<Query> = OnceLock::new();
        let query = QUERY.get_or_init(|| {
            Query::new(&self.ts_language(), &composed_tags_source())
                .expect("composed typescript tags query compiles")
        });
        Some(GraphStrategy::Tags(query))
    }
}

pub struct Tsx;

impl LanguageSupport for Tsx {
    fn id(&self) -> &'static str {
        "tsx"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["tsx"]
    }

    fn ts_language(&self) -> Language {
        tree_sitter_typescript::LANGUAGE_TSX.into()
    }

    fn graph_strategy(&self) -> Option<GraphStrategy> {
        static QUERY: OnceLock<Query> = OnceLock::new();
        let query = QUERY.get_or_init(|| {
            Query::new(&self.ts_language(), &composed_tags_source())
                .expect("composed tsx tags query compiles")
        });
        Some(GraphStrategy::Tags(query))
    }
}
