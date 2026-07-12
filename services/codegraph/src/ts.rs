//! Shared tree-sitter plumbing. One place maps a language name to its grammar and parses source, so
//! chunking and graph extraction run over **one** parse of the tree (ADR-0086: "the tree is parsed
//! once. Chunking, graph, and embedding-prep share one tree-sitter pass").

use tree_sitter::{Language, Parser, Tree};

/// Map a language name to its compiled tree-sitter grammar. Mirrors the in-tree chunker's set
/// (ADR-0010): the dedicated TypeScript grammar for TS-only syntax, plain JS grammar for `.js`.
#[must_use]
pub fn ts_language(lang: &str) -> Option<Language> {
    match lang {
        "rust" => Some(tree_sitter_rust::LANGUAGE.into()),
        "typescript" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "javascript" => Some(tree_sitter_javascript::LANGUAGE.into()),
        "python" => Some(tree_sitter_python::LANGUAGE.into()),
        _ => None,
    }
}

/// Parse `source` with the grammar for `lang`. Returns `None` when the language has no grammar or the
/// parser can't be configured. Tree-sitter is error-tolerant, so a tree is returned even for source
/// with localised syntax errors — callers deliberately do NOT bail on `root.has_error()`.
#[must_use]
pub fn parse(source: &str, lang: &str) -> Option<Tree> {
    let ts_lang = ts_language(lang)?;
    let mut parser = Parser::new();
    parser.set_language(&ts_lang).ok()?;
    parser.parse(source, None)
}
