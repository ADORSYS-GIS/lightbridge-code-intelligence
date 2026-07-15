//! Per-language support, in **one place per language**. Each supported language is a small
//! [`LanguageSupport`] impl (`lang/<language>.rs`) that owns its tree-sitter grammar, its file
//! extensions, and its graph strategy (Rust's native node-kind extractor, or the grammar's bundled
//! `tags.scm` query). A single registry (`REGISTRY`) — the factory — replaces the parallel `match`
//! arms that used to be scattered across `ts.rs`/`language.rs`/`tags.rs`/`graph.rs`, so adding or
//! finding a language means touching exactly one file and the detection facade can never drift out of
//! lock-step with what actually has an extractor (that drift was the standing hazard the old
//! `language.rs` docstring warned about).
//!
//! This module is also the **shared tree-sitter plumbing**: [`parse`] maps a language id to its
//! grammar and parses source once, so chunking and graph extraction run over **one** parse of the
//! tree (ADR-0086: "the tree is parsed once. Chunking, graph, and embedding-prep share one
//! tree-sitter pass").

mod java;
mod javascript;
mod python;
mod rust;
mod typescript;

use std::path::Path;

use tree_sitter::{Language, Parser, Query, Tree};

/// How a language's structural graph is extracted. `has_graph` is exactly "a language whose
/// [`LanguageSupport::graph_strategy`] is `Some`".
pub enum GraphStrategy {
    /// Rust keeps its own tree-sitter node-kind extractor (the chunker's `interesting_node` plus the
    /// Rust `call_expression` navigation) so chunk + graph symbols stay in lock-step and the
    /// committed golden is byte-stable.
    RustNative,
    /// Every other language is classified by the grammar's **bundled `tags.scm`** compiled query,
    /// which the language impl owns and caches. See [`crate::tags`].
    Tags(&'static Query),
}

/// One language the crate ships a tree-sitter grammar for. Exactly one implementor per language lives
/// in `lang/<language>.rs`; the `REGISTRY` slice is the single source of truth the detection facade
/// and the graph/tags pipelines all look up (see `REGISTRY`).
pub trait LanguageSupport: Sync {
    /// Stable language id used throughout the crate (`"rust"`, `"python"`, `"typescript"`, `"tsx"`,
    /// `"javascript"`, `"java"`). Also the `Chunk::language` tag.
    fn id(&self) -> &'static str;
    /// File extensions (without the dot) that map to this language.
    fn extensions(&self) -> &'static [&'static str];
    /// The compiled tree-sitter grammar.
    fn ts_language(&self) -> Language;
    /// The structural-graph strategy, or `None` for a grammar we chunk but have no graph extractor
    /// for (there are none today — every shipped grammar has an extractor).
    fn graph_strategy(&self) -> Option<GraphStrategy>;
}

/// Every supported language. The factory: `has_grammar`/`has_graph`/`from_path`/`parse` and the tags
/// pipeline are all thin lookups over this slice, so there are no parallel match arms to drift.
static REGISTRY: &[&dyn LanguageSupport] = &[
    &rust::Rust,
    &python::Python,
    &javascript::JavaScript,
    &typescript::TypeScript,
    &typescript::Tsx,
    &java::Java,
];

/// All supported languages (those the crate ships a tree-sitter grammar for).
#[must_use]
pub fn all() -> &'static [&'static dyn LanguageSupport] {
    REGISTRY
}

/// The language with this id, if any.
#[must_use]
pub fn by_id(id: &str) -> Option<&'static dyn LanguageSupport> {
    REGISTRY.iter().copied().find(|l| l.id() == id)
}

/// Detect the language of a file from its extension, among the grammar-backed languages.
#[must_use]
pub fn detect(path: &Path) -> Option<&'static dyn LanguageSupport> {
    let ext = path.extension().and_then(|e| e.to_str())?;
    REGISTRY
        .iter()
        .copied()
        .find(|l| l.extensions().contains(&ext))
}

/// Parse `source` with the grammar for language id `lang`. Returns `None` when the language has no
/// grammar or the parser can't be configured. Tree-sitter is error-tolerant, so a tree is returned
/// even for source with localised syntax errors — callers deliberately do NOT bail on
/// `root.has_error()`.
#[must_use]
pub fn parse(source: &str, lang: &str) -> Option<Tree> {
    let language = by_id(lang)?.ts_language();
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    parser.parse(source, None)
}

// ── Detection facade ──────────────────────────────────────────────────────────────────────────────
// Extension→language detection used by the walk/chunker. Absorbed from the former `language.rs`
// (ADR-0086) so all language knowledge lives under `src/lang/`.

/// Detect the language of a file from its extension. Returns `None` for unknown/binary files.
///
/// Grammar-backed languages come straight from the registry. A handful of extra extensions have
/// **no** tree-sitter grammar — they are chunked via the windowed-text fallback — but still deserve a
/// sensible language tag; those are the explicit arms below (they have nothing to parse, hence no
/// registry entry).
///
/// PDFs are deliberately **not** returned here: they are not source text and must go through the
/// bounded PDF-extraction path ([`is_pdf`]), not `read_to_string`.
#[must_use]
pub fn from_path(path: &Path) -> Option<&'static str> {
    if let Some(lang) = detect(path) {
        return Some(lang.id());
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some("go") => Some("go"),
        Some("c" | "h") => Some("c"),
        Some("cpp" | "cc" | "cxx" | "hpp") => Some("cpp"),
        Some("md" | "txt" | "toml" | "yaml" | "yml" | "json") => Some("text"),
        _ => None,
    }
}

/// True for languages we have a tree-sitter grammar for (structured chunking available).
#[must_use]
pub fn has_grammar(language: &str) -> bool {
    by_id(language).is_some()
}

/// True for languages the structural **graph** builder has a real extractor for today: Rust
/// (ADR-0086 "Rust language first"), Python, TypeScript/JavaScript (incl. TSX), and Java. This is
/// derived from the registry ([`LanguageSupport::graph_strategy`]), so it can no longer drift from
/// the classifier the way a hand-maintained parallel list could. Languages absent here still get
/// chunked and stay semantically searchable via the windowed-text fallback.
#[must_use]
pub fn has_graph(language: &str) -> bool {
    by_id(language)
        .and_then(LanguageSupport::graph_strategy)
        .is_some()
}

/// True when a path is a PDF (case-insensitive `.pdf`). PDFs take the bounded extraction path, not
/// the source-text path.
#[must_use]
pub fn is_pdf(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn detects_rust_and_text() {
        assert_eq!(from_path(Path::new("a/b.rs")), Some("rust"));
        // `.tsx` gets its own language so it routes to the JSX-aware TSX grammar, not plain TS.
        assert_eq!(from_path(Path::new("src/App.tsx")), Some("tsx"));
        assert_eq!(from_path(Path::new("src/api.ts")), Some("typescript"));
        assert_eq!(from_path(Path::new("README.md")), Some("text"));
        assert_eq!(from_path(Path::new("image.png")), None);
        assert_eq!(
            from_path(Path::new("doc.pdf")),
            None,
            "pdf is not source text"
        );
    }

    #[test]
    fn graph_covers_rust_python_and_ts_js() {
        assert!(has_graph("rust"));
        assert!(has_graph("python"));
        assert!(has_graph("typescript"));
        assert!(has_graph("javascript"));
        assert!(has_graph("tsx"));
        assert!(has_graph("java"));
        // A language we chunk but have no graph extractor for must NOT claim graph support.
        assert!(!has_graph("go"));
        assert!(!has_graph("c"));
        assert!(!has_graph("text"));
    }

    #[test]
    fn pdf_detection_is_case_insensitive() {
        assert!(is_pdf(Path::new("a/Manual.PDF")));
        assert!(is_pdf(Path::new("a/manual.pdf")));
        assert!(!is_pdf(Path::new("a/manual.md")));
    }

    #[test]
    fn registry_ids_and_grammars_are_consistent() {
        // Every registered language resolves by its own id, and `has_grammar` agrees with the
        // registry — the anti-drift guarantee the factory buys us.
        for lang in all() {
            assert!(by_id(lang.id()).is_some(), "{} resolves by id", lang.id());
            assert!(has_grammar(lang.id()), "{} has a grammar", lang.id());
        }
    }
}
