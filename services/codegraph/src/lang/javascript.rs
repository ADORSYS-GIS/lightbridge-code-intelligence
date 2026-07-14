//! JavaScript language support (`.js`/`.jsx`/`.mjs`/`.cjs`). The JavaScript grammar already parses
//! JSX, so `.jsx` shares it. Definitions + call references come from the grammar's bundled `tags.scm`
//! query, run by [`crate::tags`]. The JavaScript query also underpins TypeScript/TSX, which compose
//! it — see [`super::typescript`].

use std::sync::OnceLock;

use tree_sitter::{Language, Query};

use super::{GraphStrategy, LanguageSupport};

pub struct JavaScript;

impl LanguageSupport for JavaScript {
    fn id(&self) -> &'static str {
        "javascript"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["js", "jsx", "mjs", "cjs"]
    }

    fn ts_language(&self) -> Language {
        tree_sitter_javascript::LANGUAGE.into()
    }

    fn graph_strategy(&self) -> Option<GraphStrategy> {
        static QUERY: OnceLock<Query> = OnceLock::new();
        let query = QUERY.get_or_init(|| {
            Query::new(&self.ts_language(), tree_sitter_javascript::TAGS_QUERY)
                .expect("bundled javascript tags.scm query compiles")
        });
        Some(GraphStrategy::Tags(query))
    }
}
