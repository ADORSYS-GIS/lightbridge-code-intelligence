//! Bounded PDF text extraction (ADR-0086 §3, R2). Repos carry docs as PDFs; we extract their text and
//! feed it to the same windowed-chunk → embed path text files take.
//!
//! PDF parsing over untrusted repo input is a crash/OOM surface. This module bounds the **input**
//! and the **output**, but does NOT yet bound peak parse memory/time — see the caveat below:
//! 1. **Cap input bytes at the I/O level BEFORE the parser sees the file** — we read at most
//!    [`MAX_PDF_BYTES`] via a limited reader, so a multi-GB "PDF" never lands in memory whole.
//! 2. **Catch panics** — `pdf-extract` can panic on malformed input; the parse runs inside
//!    `catch_unwind` so a crafted file is skipped, never unwinding into the index Job.
//! 3. **Bound output** — extracted text over [`MAX_PDF_TEXT_BYTES`] is truncated, so a decompression
//!    blow-up can't turn into an unbounded chunk set.
//!
//! **Not yet bounded:** a bounded-input PDF can still expand during parsing (stream decompression,
//! font/glyph tables, object graphs), so peak *parse-time* memory and CPU are NOT capped here. A
//! small crafted file can still spike memory/time inside `pdf-extract`. Bounding the parse itself
//! (e.g. a memory/time-limited subprocess or a streaming parser) is tracked in the cutover gate.
//!
//! We picked **`pdf-extract`** over raw `lopdf`: it is purpose-built for text-out (it wraps `lopdf`
//! and walks content streams for us), so we don't hand-roll content-stream/font decoding. It is
//! treated as untrusted-input code regardless.

use std::io::Read;
use std::path::Path;

/// Hard ceiling on PDF bytes read from disk before the parser sees them (matches the chunker's
/// per-file source ceiling). A file larger than this is skipped whole.
pub const MAX_PDF_BYTES: u64 = 5 * 1024 * 1024;

/// Ceiling on extracted text kept per PDF. Guards against a small file that decompresses to a huge
/// text stream producing an unbounded number of windowed chunks.
pub const MAX_PDF_TEXT_BYTES: usize = 2 * 1024 * 1024;

/// Outcome of attempting to extract text from a PDF.
#[derive(Debug)]
pub enum PdfOutcome {
    /// Text extracted (already truncated to [`MAX_PDF_TEXT_BYTES`] if it was over).
    Text(String),
    /// The file exceeded [`MAX_PDF_BYTES`] and was skipped without parsing.
    TooLarge,
    /// Parsing failed or panicked; the file is skipped. Carries a short reason for logging.
    Failed(String),
}

/// Extract text from a PDF at `path`, reading at most [`MAX_PDF_BYTES`] before parsing. Never panics
/// and never returns an error: a broken/oversized/crafted PDF yields [`PdfOutcome::Failed`] /
/// [`PdfOutcome::TooLarge`] so the caller logs-and-skips rather than failing the whole task.
#[must_use]
pub fn extract_from_path(path: &Path) -> PdfOutcome {
    // Cap at the I/O level: open, then read through a limited reader so bytes past the ceiling are
    // never pulled into memory (ADR-0086 R2 — "cap input bytes before the parser ever sees the file").
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return PdfOutcome::Failed(format!("open: {e}")),
    };
    // If the file is obviously oversized by metadata, skip before any read.
    if let Ok(meta) = file.metadata()
        && meta.len() > MAX_PDF_BYTES
    {
        return PdfOutcome::TooLarge;
    }
    let mut bytes = Vec::new();
    // `+ 1` so we can detect a file that grew past the ceiling between the metadata check and the read.
    if let Err(e) = file.take(MAX_PDF_BYTES + 1).read_to_end(&mut bytes) {
        return PdfOutcome::Failed(format!("read: {e}"));
    }
    if bytes.len() as u64 > MAX_PDF_BYTES {
        return PdfOutcome::TooLarge;
    }
    extract_from_bytes(&bytes)
}

/// Extract text from in-memory PDF `bytes`, catching panics and bounding output. Public so the PDF
/// path is unit-testable without touching the filesystem.
#[must_use]
pub fn extract_from_bytes(bytes: &[u8]) -> PdfOutcome {
    // `pdf-extract` can panic on malformed input; contain it so a crafted PDF can't unwind the Job.
    let result = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(bytes));
    match result {
        Ok(Ok(mut text)) => {
            if text.len() > MAX_PDF_TEXT_BYTES {
                // Truncate on a char boundary at or below the ceiling.
                let mut end = MAX_PDF_TEXT_BYTES;
                while end > 0 && !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
            }
            PdfOutcome::Text(text)
        }
        Ok(Err(e)) => PdfOutcome::Failed(format!("parse: {e}")),
        Err(_) => PdfOutcome::Failed("parser panicked".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbage_bytes_fail_gracefully_without_panicking() {
        // Not a PDF at all — must be caught and reported, never unwind.
        let outcome = extract_from_bytes(b"this is definitely not a pdf");
        assert!(
            matches!(outcome, PdfOutcome::Failed(_)),
            "non-pdf input should be Failed, got {outcome:?}"
        );
    }

    #[test]
    fn truncated_pdf_header_fails_gracefully() {
        let outcome = extract_from_bytes(b"%PDF-1.7\n<<broken");
        assert!(matches!(outcome, PdfOutcome::Failed(_)));
    }

    #[test]
    fn oversized_file_is_reported_too_large() {
        // Write a file just over the ceiling and confirm it's skipped pre-parse.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.pdf");
        let blob = vec![b'a'; (MAX_PDF_BYTES + 16) as usize];
        std::fs::write(&path, &blob).unwrap();
        assert!(matches!(extract_from_path(&path), PdfOutcome::TooLarge));
    }

    #[test]
    fn extracts_text_from_a_minimal_pdf() {
        // A genuinely valid single-page PDF (proper xref, standard Helvetica font) drawing "Hello
        // LCI", built with lopdf so the round-trip through pdf-extract is real.
        let pdf = minimal_pdf_with_text();
        match extract_from_bytes(&pdf) {
            PdfOutcome::Text(t) => assert!(
                t.contains("Hello LCI"),
                "expected extracted text to contain 'Hello LCI', got {t:?}"
            ),
            other => panic!("expected Text outcome for a valid minimal PDF, got {other:?}"),
        }
    }

    /// A minimal, valid single-page PDF whose content stream draws the text "Hello LCI".
    fn minimal_pdf_with_text() -> Vec<u8> {
        use lopdf::content::{Content, Operation};
        use lopdf::{Document, Object, Stream, dictionary};

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 24.into()]),
                Operation::new("Td", vec![20.into(), 100.into()]),
                Operation::new("Tj", vec![Object::string_literal("Hello LCI")]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
        });
        let pages = dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
        };
        doc.objects.insert(pages_id, Object::Dictionary(pages));
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);

        let mut buf = Vec::new();
        doc.save_to(&mut buf).expect("save synthetic pdf");
        buf
    }
}
