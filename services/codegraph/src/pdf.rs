//! Bounded PDF text extraction (ADR-0086 §3, R2). Repos carry docs as PDFs; we extract their text and
//! feed it to the same windowed-chunk → embed path text files take.
//!
//! PDF parsing over untrusted repo input is a crash/OOM/hang surface. This module bounds it in four
//! layers so a crafted repo PDF can neither OOM-kill nor hang the index Job:
//! 1. **Cap input bytes at the I/O level BEFORE the parser sees the file** — we read at most
//!    [`MAX_PDF_BYTES`] via a limited reader, so a multi-GB "PDF" never lands in memory whole.
//! 2. **Bound cumulative FlateDecode input and reject the worst bombs** — a small input can still
//!    *inflate* to gigabytes (a "decompression bomb"), and a Rust allocation failure ABORTS the
//!    process (uncatchable by `catch_unwind`). Before `pdf-extract` runs, we pre-flight every
//!    FlateDecode content stream through a **bounded** inflate ([`MAX_PDF_DECOMPRESSED_BYTES`]) that
//!    never materialises more than a small buffer, and reject a file whose streams blow the budget.
//!    This bounds the *cumulative inflated size of the Flate streams we can see* and cheaply kills the
//!    worst multipliers — it does **not** cap peak process RSS: a file that *passes* is handed to
//!    `pdf-extract`, which re-inflates those streams itself (independently, up to the budget) and can
//!    allocate further for fonts/glyphs/object graphs on top.
//! 3. **Bound parse wall-clock** — the parse runs on a worker thread under a [`PDF_PARSE_TIMEOUT`]
//!    watchdog; a pathological file that spins can't hang the Job past the deadline.
//! 4. **Catch panics + bound output** — `pdf-extract` can panic on malformed input (contained by
//!    `catch_unwind`), and extracted text over [`MAX_PDF_TEXT_BYTES`] is truncated.
//!
//! **Residual limits (honest):** the decompression guard covers FlateDecode streams found by a
//! syntactic `stream…endstream` scan — a bomb behind a non-Flate or cascaded filter, or blow-up in
//! font/glyph tables rather than stream inflation, is not pre-flighted; the wall-clock watchdog is the
//! only backstop there, and it bounds **time, not peak RSS**. On timeout the worker thread is
//! *abandoned*, not killed: its memory is **not reclaimed** and, on exactly the non-pre-flighted path
//! the watchdog exists to catch, is **not bounded** — a runaway parse keeps allocating (and holding a
//! core) after we stop waiting. So this module bounds wall-clock and the common decompression-bomb
//! vector, but a **hard per-parse memory ceiling is still required before graduation** (subprocess +
//! `RLIMIT_AS`, so an OOM kills only the child) — tracked as a #360 cutover gate item.
//!
//! We picked **`pdf-extract`** over raw `lopdf`: it is purpose-built for text-out (it wraps `lopdf`
//! and walks content streams for us), so we don't hand-roll content-stream/font decoding. It is
//! treated as untrusted-input code regardless.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

/// Hard ceiling on PDF bytes read from disk before the parser sees them (matches the chunker's
/// per-file source ceiling). A file larger than this is skipped whole.
pub const MAX_PDF_BYTES: u64 = 5 * 1024 * 1024;

/// Ceiling on extracted text kept per PDF. Guards against a small file that decompresses to a huge
/// text stream producing an unbounded number of windowed chunks.
pub const MAX_PDF_TEXT_BYTES: usize = 2 * 1024 * 1024;

/// Cumulative decompressed-bytes budget across a PDF's Flate content streams. A file whose streams
/// inflate past this is treated as a decompression bomb and skipped. Generous enough for legitimate
/// image/vector-heavy docs (a ≤[`MAX_PDF_BYTES`] input at a sane ratio stays well under it) while a
/// real bomb (1000×+) trips it immediately. The guard never materialises this many bytes — it stops
/// reading at the ceiling — so catching a multi-GB bomb costs kilobytes, not gigabytes.
pub const MAX_PDF_DECOMPRESSED_BYTES: u64 = 256 * 1024 * 1024;

/// Wall-clock ceiling on a single PDF parse. A file that spins inside `pdf-extract` past this is
/// abandoned (the Job moves on) rather than hanging the whole index.
pub const PDF_PARSE_TIMEOUT: Duration = Duration::from_secs(15);

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

/// Extract text from in-memory PDF `bytes`, bounding peak decompression memory + parse wall-clock,
/// catching panics, and bounding output. Public so the PDF path is unit-testable without touching the
/// filesystem.
#[must_use]
pub fn extract_from_bytes(bytes: &[u8]) -> PdfOutcome {
    extract_bounded(bytes, MAX_PDF_DECOMPRESSED_BYTES, PDF_PARSE_TIMEOUT)
}

/// The bounded core, parameterised on the decompression budget + timeout so tests can exercise the
/// bomb guard with a small budget instead of allocating real gigabytes.
fn extract_bounded(bytes: &[u8], decompress_budget: u64, timeout: Duration) -> PdfOutcome {
    // Layer 2 — decompression-bomb guard: reject BEFORE `pdf-extract` ever inflates the streams, so a
    // small file that would inflate to gigabytes can't drive an (uncatchable) alloc-failure abort.
    if flate_streams_exceed_budget(bytes, decompress_budget) {
        return PdfOutcome::Failed(format!(
            "flate streams inflate past {decompress_budget} bytes (possible decompression bomb)"
        ));
    }

    // Layer 3 — wall-clock watchdog: parse on a worker thread; if it overruns we stop waiting and
    // return, bounding *time*. Caveat: on timeout the worker is ABANDONED, not killed — its memory is
    // NOT reclaimed and, on precisely the path that makes a parse overrun (a non-Flate/cascaded-filter
    // stream or font/glyph-table blow-up the layer-2 pre-flight can't see), is NOT bounded: the
    // detached thread keeps allocating after this returns. This backstops CPU/hang, not peak RSS; the
    // hard memory bound (subprocess + RLIMIT_AS) is still required before graduation (#360).
    let owned = bytes.to_vec();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // `pdf-extract` can panic on malformed input; contain it so a crafted PDF can't unwind.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pdf_extract::extract_text_from_mem(&owned)
        }));
        let _ = tx.send(result);
    });

    match rx.recv_timeout(timeout) {
        Ok(Ok(Ok(text))) => PdfOutcome::Text(truncate_text(text)),
        Ok(Ok(Err(e))) => PdfOutcome::Failed(format!("parse: {e}")),
        Ok(Err(_)) => PdfOutcome::Failed("parser panicked".to_string()),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            PdfOutcome::Failed(format!("parse exceeded {}s", timeout.as_secs()))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            PdfOutcome::Failed("parser thread died".to_string())
        }
    }
}

/// Truncate extracted text to [`MAX_PDF_TEXT_BYTES`] on a char boundary.
fn truncate_text(mut text: String) -> String {
    if text.len() > MAX_PDF_TEXT_BYTES {
        let mut end = MAX_PDF_TEXT_BYTES;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// Counting sink: discards bytes, tracks how many it saw. Lets us measure an inflate without keeping
/// the inflated output.
struct CountSink(u64);
impl Write for CountSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(buf.len() as u64);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// True if the cumulative inflated size of the PDF's FlateDecode content streams exceeds `budget`.
///
/// Scans for `stream…endstream` segments and inflates each through a reader capped at the *remaining*
/// budget + 1 byte, so a bomb stream is only ever inflated up to the ceiling — never its full
/// (potentially multi-GB) output. Non-Flate streams simply fail to inflate and are skipped: their raw
/// size is already bounded by [`MAX_PDF_BYTES`], so they carry no *decompression* amplification.
fn flate_streams_exceed_budget(bytes: &[u8], budget: u64) -> bool {
    let mut total: u64 = 0;
    let mut i = 0usize;
    while let Some(rel) = find(&bytes[i..], b"stream") {
        let kw = i + rel;
        i = kw + b"stream".len();
        // Skip a match that is actually the tail of `endstream`.
        if kw >= 3 && &bytes[kw - 3..kw] == b"end" {
            continue;
        }
        // Stream data begins after the EOL following the `stream` keyword (CRLF or LF per spec).
        let mut data = i;
        if data < bytes.len() && bytes[data] == b'\r' {
            data += 1;
        }
        if data < bytes.len() && bytes[data] == b'\n' {
            data += 1;
        }
        let Some(end_rel) = find(&bytes[data..], b"endstream") else {
            break;
        };
        let raw = &bytes[data..data + end_rel];
        i = data + end_rel + b"endstream".len();

        // Inflate through a reader bounded to the remaining budget + 1. `io::copy` reads at most that
        // many bytes total, so peak memory is the copy buffer — not the inflated size.
        let remaining = budget.saturating_sub(total);
        let mut decoder = flate2::read::ZlibDecoder::new(raw).take(remaining + 1);
        let mut sink = CountSink(0);
        // Ignore copy errors (a non-Flate / corrupt stream); only the byte count matters.
        let _ = std::io::copy(&mut decoder, &mut sink);
        total = total.saturating_add(sink.0);
        if total > budget {
            return true;
        }
    }
    false
}

/// First index of `needle` in `haystack` (naive; inputs are ≤ [`MAX_PDF_BYTES`] with few streams).
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Zlib-compress `data` — the FlateDecode payload of a synthetic content stream.
    fn zlib_compress(data: &[u8]) -> Vec<u8> {
        use flate2::{Compression, write::ZlibEncoder};
        let mut e = ZlibEncoder::new(Vec::new(), Compression::best());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    /// Wrap compressed bytes in a minimal `stream…endstream` segment the guard's scanner recognises.
    fn pdf_with_flate_stream(compressed: &[u8]) -> Vec<u8> {
        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.5\n1 0 obj<</Filter/FlateDecode>>stream\n");
        pdf.extend_from_slice(compressed);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
        pdf
    }

    #[test]
    fn decompression_bomb_is_rejected_without_inflating_it_fully() {
        // 8 MiB of zeros compresses to a few KB but inflates to 8 MiB; with a 1 MiB budget the guard
        // must reject it — and it only ever inflates ~1 MiB (the bounded reader), never the full 8.
        let bomb = zlib_compress(&vec![0u8; 8 * 1024 * 1024]);
        assert!(
            bomb.len() < 64 * 1024,
            "sanity: bomb payload is tiny ({} bytes)",
            bomb.len()
        );
        let pdf = pdf_with_flate_stream(&bomb);
        let outcome = extract_bounded(&pdf, 1024 * 1024, PDF_PARSE_TIMEOUT);
        match outcome {
            PdfOutcome::Failed(reason) => assert!(
                reason.contains("bomb") || reason.contains("inflate"),
                "expected a decompression-bomb failure, got {reason:?}"
            ),
            other => panic!("bomb must be rejected, got {other:?}"),
        }
    }

    #[test]
    fn guard_passes_a_stream_within_budget() {
        // A stream that inflates to well under the budget must NOT be flagged (no false positive).
        let small = zlib_compress(&vec![0u8; 256 * 1024]); // 256 KiB inflated
        let pdf = pdf_with_flate_stream(&small);
        assert!(
            !flate_streams_exceed_budget(&pdf, 1024 * 1024),
            "a 256 KiB inflate must not trip a 1 MiB budget"
        );
    }

    #[test]
    fn guard_ignores_non_flate_streams() {
        // Uncompressed / non-Flate stream bytes must be skipped, not counted as a bomb.
        let mut pdf = Vec::new();
        pdf.extend_from_slice(
            b"%PDF-1.5\n1 0 obj<<>>stream\nplain uncompressed bytes\nendstream\n",
        );
        assert!(!flate_streams_exceed_budget(&pdf, 1024));
    }

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
