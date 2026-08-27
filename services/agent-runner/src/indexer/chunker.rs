//! Syntax-aware chunking (ADR-0010). For supported languages we use tree-sitter to extract named
//! top-level items (functions, structs, classes, impls, methods). For everything else — or when the
//! file is too large / unparseable — we fall back to a fixed-size line window.

use tree_sitter::{Language, Node, Parser};

/// Skip files larger than this (avoids embedding enormous generated files).
/// The chunk-line ceiling and the windowed-fallback sizes are operator-tunable — see
/// [`super::IndexTuning`].
const MAX_FILE_BYTES: usize = 5 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Chunk {
    pub file_path: String,
    pub language: String,
    pub chunk_type: String,
    pub symbol_name: Option<String>,
    pub start_line: i32,
    pub end_line: i32,
    pub content: String,
}

/// Walk `root` recursively and collect all chunks for `file_path`.
pub fn chunk_file(
    file_path: &str,
    source: &str,
    language: &str,
    tuning: super::IndexTuning,
) -> Vec<Chunk> {
    if source.len() > MAX_FILE_BYTES {
        return Vec::new();
    }
    // Detect binary content by scanning the first 512 bytes for null bytes.
    if source.as_bytes().iter().take(512).any(|&b| b == 0) {
        return Vec::new();
    }

    let chunks = if super::language::has_grammar(language)
        && let Some(chunks) = try_treesitter(file_path, source, language, tuning)
        && !chunks.is_empty()
    {
        chunks
    } else {
        // Fallback: text files and languages without a grammar get windowed chunking.
        window_chunks(file_path, source, language, tuning)
    };

    // Neither strategy above bounds a chunk's byte size on its own (a tree-sitter symbol with no
    // splittable children, or a windowed slice of long unwrapped lines, can both come out huge) —
    // this pass is the single place that guarantees every chunk fits an embedding model's input.
    chunks
        .into_iter()
        .flat_map(|c| cap_chunk_bytes(c, tuning))
        .collect()
}

/// Split a chunk whose content exceeds `tuning.max_chunk_bytes` into smaller line-windowed pieces.
fn cap_chunk_bytes(chunk: Chunk, tuning: super::IndexTuning) -> Vec<Chunk> {
    if chunk.content.len() <= tuning.max_chunk_bytes {
        return vec![chunk];
    }
    let lines: Vec<&str> = chunk.content.lines().collect();
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < lines.len() {
        // A line longer than the cap on its own can't be merged with anything — split the line
        // itself instead of emitting it whole (a minified/generated single-line file otherwise
        // sails straight past an embedding model's own input limit, unsplit).
        if lines[start].len() > tuning.max_chunk_bytes {
            let line_no = chunk.start_line + start as i32;
            for piece in split_line_by_bytes(lines[start], tuning.max_chunk_bytes) {
                out.push(Chunk {
                    file_path: chunk.file_path.clone(),
                    language: chunk.language.clone(),
                    chunk_type: chunk.chunk_type.clone(),
                    symbol_name: chunk.symbol_name.clone(),
                    start_line: line_no,
                    end_line: line_no,
                    content: piece.to_string(),
                });
            }
            start += 1;
            continue;
        }

        let mut end = start + 1;
        let mut size = lines[start].len();
        while end < lines.len() {
            // +1 accounts for the "\n" the join() below inserts between lines — omitting it
            // undercounts by one byte per line, so the emitted content can exceed the cap.
            let with_next = size + 1 + lines[end].len();
            if with_next > tuning.max_chunk_bytes {
                break;
            }
            size = with_next;
            end += 1;
        }
        out.push(Chunk {
            file_path: chunk.file_path.clone(),
            language: chunk.language.clone(),
            chunk_type: chunk.chunk_type.clone(),
            symbol_name: chunk.symbol_name.clone(),
            start_line: chunk.start_line + start as i32,
            end_line: chunk.start_line + end as i32 - 1,
            content: lines[start..end].join("\n"),
        });
        start = end;
    }
    out
}

/// Split one line into `<= max_bytes`-sized pieces, each cut on a valid UTF-8 char boundary.
fn split_line_by_bytes(line: &str, max_bytes: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = line;
    while rest.len() > max_bytes {
        let mut end = max_bytes;
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        pieces.push(&rest[..end]);
        rest = &rest[end..];
    }
    pieces.push(rest);
    pieces
}

fn ts_language(lang: &str) -> Option<Language> {
    match lang {
        "rust" => Some(tree_sitter_rust::LANGUAGE.into()),
        // Use the dedicated TypeScript grammar so TS-only syntax (generics, interfaces,
        // decorators) parses correctly. JavaScript grammar is kept for plain JS files.
        "typescript" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "javascript" => Some(tree_sitter_javascript::LANGUAGE.into()),
        "python" => Some(tree_sitter_python::LANGUAGE.into()),
        _ => None,
    }
}

fn try_treesitter(
    file_path: &str,
    source: &str,
    language: &str,
    tuning: super::IndexTuning,
) -> Option<Vec<Chunk>> {
    let ts_lang = ts_language(language)?;
    let mut parser = Parser::new();
    parser.set_language(&ts_lang).ok()?;
    let tree = parser.parse(source, None)?;
    // Don't bail on `root.has_error()`: tree-sitter is error-tolerant and successfully parses
    // most of a file even with localised syntax errors. Returning None here would silently
    // fall back to windowed chunking for an entire file with one bad expression.
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

    if chunks.is_empty() {
        None
    } else {
        Some(chunks)
    }
}

/// Recursively collect interesting nodes. We walk the full tree (not just top-level children) so
/// that methods inside `impl` blocks, nested functions, and inner classes are captured.
fn collect_items(
    node: &Node<'_>,
    bytes: &[u8],
    file_path: &str,
    source: &str,
    language: &str,
    tuning: super::IndexTuning,
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
                    // No interesting sub-nodes (e.g. a 200-line function with no nested fns).
                    // Emit it as one chunk rather than silently dropping the symbol —
                    // `chunk_file`'s cap_chunk_bytes pass splits it further if it's oversized.
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

/// Returns `(chunk_type, symbol_name)` for nodes we want to index; `None` for everything else.
fn interesting_node(node: &Node<'_>, bytes: &[u8]) -> Option<(&'static str, Option<String>)> {
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
fn window_chunks(
    file_path: &str,
    source: &str,
    language: &str,
    tuning: super::IndexTuning,
) -> Vec<Chunk> {
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < lines.len() {
        let end = (start + tuning.window_size).min(lines.len());
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
        start += tuning.window_step;
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::super::IndexTuning;
    use super::*;

    #[test]
    fn rust_function_is_extracted_as_one_chunk() {
        let src = r#"fn add(a: i32, b: i32) -> i32 { a + b }

fn sub(a: i32, b: i32) -> i32 { a - b }
"#;
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
        let chunks = window_chunks("f.txt", src, "text", IndexTuning::default());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].start_line, 0);
    }

    #[test]
    fn chunk_under_the_cap_passes_through_unchanged() {
        let tuning = IndexTuning {
            max_chunk_bytes: 1_000,
            ..IndexTuning::default()
        };
        let chunk = Chunk {
            file_path: "f.txt".to_string(),
            language: "text".to_string(),
            chunk_type: "window".to_string(),
            symbol_name: None,
            start_line: 0,
            end_line: 2,
            content: "small".to_string(),
        };
        let out = cap_chunk_bytes(chunk, tuning);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].content, "small");
    }

    // Mirrors a real failure: a markdown changelog whose entries are each one very long unwrapped
    // line, chunked by `window_chunks` (100 lines/window by default) — a window of such lines can
    // vastly exceed an embedding model's input limit despite easily fitting the line-count cap.
    #[test]
    fn oversized_windowed_chunk_from_long_unwrapped_lines_is_split_under_the_cap() {
        let tuning = IndexTuning {
            max_chunk_bytes: 2_000,
            ..IndexTuning::default()
        };
        let long_line = "x".repeat(500);
        let src = vec![long_line; 50].join("\n");
        let chunks = chunk_file("CHANGELOG.md", &src, "text", tuning);
        assert!(
            chunks.len() > 1,
            "one 25KB window must split into several chunks"
        );
        assert!(
            chunks
                .iter()
                .all(|c| c.content.len() <= tuning.max_chunk_bytes),
            "no output chunk may exceed max_chunk_bytes"
        );
    }

    #[test]
    fn a_single_line_longer_than_the_cap_is_split_into_capped_pieces() {
        let tuning = IndexTuning {
            max_chunk_bytes: 10,
            ..IndexTuning::default()
        };
        let chunk = Chunk {
            file_path: "f.txt".to_string(),
            language: "text".to_string(),
            chunk_type: "window".to_string(),
            symbol_name: None,
            start_line: 0,
            end_line: 0,
            content: "a".repeat(100),
        };
        let out = cap_chunk_bytes(chunk, tuning);
        assert!(
            out.len() > 1,
            "a 100-byte line at a 10-byte cap must split into multiple pieces"
        );
        assert!(
            out.iter()
                .all(|c| c.content.len() <= tuning.max_chunk_bytes),
            "no output chunk may exceed max_chunk_bytes"
        );
        assert_eq!(
            out.iter().map(|c| c.content.len()).sum::<usize>(),
            100,
            "the pieces must reconstruct the full line with nothing dropped"
        );
        assert!(out.iter().all(|c| c.start_line == 0 && c.end_line == 0));
    }

    // Regression: ADORSYS-GIS/CoopData's frontend/openapi.json is one 191,026-byte line — a
    // minified/generated file — which the embeddings API rejects outright past its own
    // 131,072-char input limit. The splitter must never emit a chunk that large regardless of
    // file shape.
    #[test]
    fn a_giant_single_line_json_file_is_split_under_the_embedding_models_limit() {
        let tuning = IndexTuning::default(); // max_chunk_bytes: 16_000
        let src = "x".repeat(191_026);
        let chunks = chunk_file("openapi.json", &src, "json", tuning);
        assert!(!chunks.is_empty());
        assert!(
            chunks
                .iter()
                .all(|c| c.content.len() <= tuning.max_chunk_bytes),
            "no chunk may exceed max_chunk_bytes, even from a single giant line"
        );
    }

    #[test]
    fn split_line_by_bytes_never_slices_through_a_multibyte_char() {
        let line = "€".repeat(20); // each € is 3 bytes — max_bytes not a multiple of 3
        let pieces = split_line_by_bytes(&line, 10);
        assert!(pieces.iter().all(|p| p.len() <= 10));
        assert_eq!(pieces.concat(), line, "no bytes lost or corrupted");
    }

    // Regression: the byte accumulator must count the `\n` that `join("\n")` inserts between
    // lines, not just line lengths — otherwise many short lines can pack into one chunk whose
    // real length exceeds the cap (worst case ~2x, per lightbridge-assistant's PR #619 review).
    #[test]
    fn cap_accounts_for_join_separator_bytes_not_just_line_lengths() {
        let tuning = IndexTuning {
            max_chunk_bytes: 100,
            ..IndexTuning::default()
        };
        let chunk = Chunk {
            file_path: "f.txt".to_string(),
            language: "text".to_string(),
            chunk_type: "window".to_string(),
            symbol_name: None,
            start_line: 0,
            end_line: 0,
            content: vec!["x"; 99].join("\n"), // 99 + 98 separators = 197 bytes
        };
        let out = cap_chunk_bytes(chunk, tuning);
        assert!(
            out.iter()
                .all(|c| c.content.len() <= tuning.max_chunk_bytes),
            "every split chunk must respect the cap once join() separators are counted"
        );
    }
}
