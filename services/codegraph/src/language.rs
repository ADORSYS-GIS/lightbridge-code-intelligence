//! Language detection by file extension. Intentionally minimal: we only parse languages for which
//! we ship a tree-sitter grammar; everything else falls through to windowed chunking. Absorbed from
//! `agent-runner/src/indexer/language.rs` (ADR-0086), plus PDF detection for the new text-extraction
//! path.

use std::path::Path;

/// Detect the language of a file from its extension. Returns `None` for unknown/binary files.
///
/// PDFs are deliberately **not** returned here: they are not source text and must go through the
/// bounded PDF-extraction path ([`is_pdf`]), not `read_to_string`.
#[must_use]
pub fn from_path(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => Some("rust"),
        Some("ts" | "tsx") => Some("typescript"),
        Some("js" | "jsx" | "mjs" | "cjs") => Some("javascript"),
        Some("py") => Some("python"),
        Some("go") => Some("go"),
        Some("java") => Some("java"),
        Some("c" | "h") => Some("c"),
        Some("cpp" | "cc" | "cxx" | "hpp") => Some("cpp"),
        Some("md" | "txt" | "toml" | "yaml" | "yml" | "json") => Some("text"),
        _ => None,
    }
}

/// True for languages we have a tree-sitter grammar for (structured chunking available).
#[must_use]
pub fn has_grammar(language: &str) -> bool {
    matches!(language, "rust" | "typescript" | "javascript" | "python")
}

/// True for languages the structural **graph** builder resolves today. Slice 1 ships Rust only
/// (ADR-0086: "Rust language first"); other languages keep Graphify for the graph and the
/// windowed-text fallback keeps them semantically searchable.
#[must_use]
pub fn has_graph(language: &str) -> bool {
    matches!(language, "rust")
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
        assert_eq!(from_path(Path::new("README.md")), Some("text"));
        assert_eq!(from_path(Path::new("image.png")), None);
        assert_eq!(
            from_path(Path::new("doc.pdf")),
            None,
            "pdf is not source text"
        );
    }

    #[test]
    fn graph_is_rust_only_in_slice_1() {
        assert!(has_graph("rust"));
        assert!(!has_graph("python"));
        assert!(!has_graph("typescript"));
    }

    #[test]
    fn pdf_detection_is_case_insensitive() {
        assert!(is_pdf(Path::new("a/Manual.PDF")));
        assert!(is_pdf(Path::new("a/manual.pdf")));
        assert!(!is_pdf(Path::new("a/manual.md")));
    }
}
