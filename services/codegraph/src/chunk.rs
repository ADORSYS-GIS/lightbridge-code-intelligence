//! Syntax-aware chunking (ADR-0010), absorbed from `agent-runner/src/indexer/chunker.rs`. For
//! supported languages we use tree-sitter to extract named top-level items (functions, structs,
//! classes, impls, methods). For everything else — or when the file is too large / unparseable — we
//! fall back to a fixed-size line window. PDF-extracted text takes the same windowed path.

use serde::Serialize;
use tree_sitter::{Node, Tree};

use crate::IndexTuning;
use crate::lang;

/// Skip files larger than this (avoids embedding enormous generated files). The chunk-line ceiling
/// and the windowed-fallback sizes are operator-tunable — see [`IndexTuning`].
pub const MAX_FILE_BYTES: usize = 5 * 1024 * 1024;

/// One embeddable unit of source. `Serialize` is derived so the parity harness can snapshot chunk
/// output as a golden; the field set mirrors `agent-clients::ChunkPayload` (minus the embedding).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Chunk {
    pub file_path: String,
    pub language: String,
    pub chunk_type: String,
    pub symbol_name: Option<String>,
    pub start_line: i32,
    pub end_line: i32,
    pub content: String,
}

/// Chunk one file's `source`. Parses with tree-sitter when a grammar exists and the result is
/// non-empty; otherwise falls back to windowed chunking.
#[must_use]
pub fn chunk_file(
    file_path: &str,
    source: &str,
    language: &str,
    tuning: IndexTuning,
) -> Vec<Chunk> {
    if source.len() > MAX_FILE_BYTES {
        return Vec::new();
    }
    // Detect binary content by scanning the first 512 bytes for null bytes.
    if source.as_bytes().iter().take(512).any(|&b| b == 0) {
        return Vec::new();
    }

    if lang::has_grammar(language)
        && let Some(tree) = lang::parse(source, language)
    {
        let chunks = chunk_tree(&tree, file_path, source, language, tuning);
        if !chunks.is_empty() {
            return chunks;
        }
    }
    // Fallback: text files and languages without a grammar get windowed chunking.
    window_chunks(file_path, source, language, tuning)
}

/// Chunk a **pre-parsed** tree — used by the walk so a Rust file is parsed once and fed to both the
/// chunker and the graph builder (ADR-0086 "parse once"). Returns an empty vec if no interesting
/// nodes were found; the caller decides whether to window-fall-back.
#[must_use]
pub fn chunk_tree(
    tree: &Tree,
    file_path: &str,
    source: &str,
    language: &str,
    tuning: IndexTuning,
) -> Vec<Chunk> {
    let root = tree.root_node();
    let bytes = source.as_bytes();
    let mut chunks = Vec::new();
    collect_items(
        &root,
        bytes,
        file_path,
        source,
        language,
        tuning,
        &mut chunks,
    );
    chunks
}

/// Chunk free text (e.g. PDF-extracted text) through the windowed path text files already take.
#[must_use]
pub fn chunk_text(file_path: &str, text: &str, language: &str, tuning: IndexTuning) -> Vec<Chunk> {
    window_chunks(file_path, text, language, tuning)
}

/// Recursively collect interesting nodes. We walk the full tree (not just top-level children) so that
/// methods inside `impl` blocks, nested functions, and inner classes are captured.
fn collect_items(
    node: &Node<'_>,
    bytes: &[u8],
    file_path: &str,
    source: &str,
    language: &str,
    tuning: IndexTuning,
    out: &mut Vec<Chunk>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some((chunk_type, symbol_name)) = interesting_node(&child, bytes) {
            let start_line = child.start_position().row as i32;
            let end_line = child.end_position().row as i32;
            let span = (end_line - start_line) as usize;

            let content = &source[child.byte_range()];

            if span <= tuning.max_chunk_lines {
                out.push(Chunk {
                    file_path: file_path.to_string(),
                    language: language.to_string(),
                    chunk_type: chunk_type.to_string(),
                    symbol_name,
                    start_line,
                    end_line,
                    content: content.to_string(),
                });
                // Also recurse so methods inside a small impl / class are independently indexed.
                collect_items(&child, bytes, file_path, source, language, tuning, out);
            } else {
                // Large node: try to extract interesting children (e.g. methods inside a big impl).
                let before = out.len();
                collect_items(&child, bytes, file_path, source, language, tuning, out);
                if out.len() == before {
                    // No interesting sub-nodes (e.g. a 200-line function with no nested fns). Emit it
                    // as a single chunk rather than silently dropping it; the embedding API will
                    // truncate if the content exceeds the model's context window.
                    out.push(Chunk {
                        file_path: file_path.to_string(),
                        language: language.to_string(),
                        chunk_type: chunk_type.to_string(),
                        symbol_name,
                        start_line,
                        end_line,
                        content: content.to_string(),
                    });
                }
            }
        } else {
            // Not an interesting node itself — still descend to find nested interesting nodes.
            collect_items(&child, bytes, file_path, source, language, tuning, out);
        }
    }
}

/// Returns `(chunk_type, symbol_name)` for nodes we want to index; `None` for everything else. Shared
/// with the graph builder (definitions are exactly the interesting chunk nodes).
pub(crate) fn interesting_node(
    node: &Node<'_>,
    bytes: &[u8],
) -> Option<(&'static str, Option<String>)> {
    let (kind, name_field) = match node.kind() {
        // Rust
        "function_item" => ("function", Some("name")),
        "impl_item" => ("impl", None),
        "struct_item" => ("struct", Some("name")),
        "enum_item" => ("enum", Some("name")),
        "trait_item" => ("trait", Some("name")),
        "mod_item" => ("module", Some("name")),
        "type_alias" => ("type", Some("name")),
        // TypeScript / JavaScript
        "function_declaration" => ("function", Some("name")),
        "function_expression" => ("function", None),
        "arrow_function" => ("function", None),
        "class_declaration" => ("class", Some("name")),
        "class_expression" => ("class", Some("name")),
        "method_definition" => ("method", Some("name")),
        "variable_declarator" => return None, // too noisy at top level
        // Python
        "function_definition" => ("function", Some("name")),
        "class_definition" => ("class", Some("name")),
        "decorated_definition" => ("function", None), // decorator + def/class
        _ => return None,
    };

    let symbol_name = name_field.and_then(|field| {
        node.child_by_field_name(field).and_then(|n| {
            std::str::from_utf8(&bytes[n.byte_range()])
                .ok()
                .map(str::to_string)
        })
    });

    Some((kind, symbol_name))
}

/// Fixed-size line windows with overlap — the fallback for text / unsupported languages.
fn window_chunks(file_path: &str, source: &str, language: &str, tuning: IndexTuning) -> Vec<Chunk> {
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    // `IndexTuning::from_env` clamps these, but the fields are `pub` — a caller can construct
    // `IndexTuning { window_step: 0, .. }` directly, which would wedge this loop forever (start
    // never advances). Clamp locally to `>= 1` as a belt-and-suspenders guard.
    let window_size = tuning.window_size.max(1);
    let window_step = tuning.window_step.max(1);
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < lines.len() {
        let end = (start + window_size).min(lines.len());
        let content = lines[start..end].join("\n");
        chunks.push(Chunk {
            file_path: file_path.to_string(),
            language: language.to_string(),
            chunk_type: "window".to_string(),
            symbol_name: None,
            start_line: start as i32,
            end_line: (end - 1) as i32,
            content,
        });
        if end == lines.len() {
            break;
        }
        start += window_step;
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_function_is_extracted_as_one_chunk() {
        let src =
            "fn add(a: i32, b: i32) -> i32 { a + b }\n\nfn sub(a: i32, b: i32) -> i32 { a - b }\n";
        let chunks = chunk_file("src/math.rs", src, "rust", IndexTuning::default());
        assert!(!chunks.is_empty(), "should produce at least one chunk");
        let add = chunks
            .iter()
            .find(|c| c.symbol_name.as_deref() == Some("add"));
        assert!(add.is_some(), "should extract fn add");
        assert_eq!(add.unwrap().chunk_type, "function");
    }

    #[test]
    fn binary_content_is_skipped() {
        let src = "hello\x00world";
        let chunks = chunk_file("image.png", src, "text", IndexTuning::default());
        assert!(chunks.is_empty());
    }

    #[test]
    fn text_file_falls_back_to_windows() {
        let lines: Vec<String> = (0..200).map(|i| format!("line {i}")).collect();
        let src = lines.join("\n");
        let chunks = chunk_file("README.md", &src, "text", IndexTuning::default());
        assert!(!chunks.is_empty());
        assert!(chunks.iter().all(|c| c.chunk_type == "window"));
    }

    #[test]
    fn window_chunk_covers_full_file_when_short() {
        let src = "one\ntwo\nthree\n";
        let chunks = chunk_text("f.txt", src, "text", IndexTuning::default());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].start_line, 0);
    }
}
