//! Rust language support. Rust is special: it keeps its **own** tree-sitter node-kind extractor
//! (the chunker's `interesting_node` plus the Rust `call_expression` navigation in [`crate::graph`])
//! rather than a `tags.scm` query, so chunk + graph symbols stay in lock-step and the committed
//! golden is byte-stable (ADR-0086 "Rust language first").

use tree_sitter::Language;

use super::{GraphStrategy, LanguageSupport};

pub struct Rust;

impl LanguageSupport for Rust {
    fn id(&self) -> &'static str {
        "rust"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["rs"]
    }

    fn ts_language(&self) -> Language {
        tree_sitter_rust::LANGUAGE.into()
    }

    fn graph_strategy(&self) -> Option<GraphStrategy> {
        Some(GraphStrategy::RustNative)
    }
}
