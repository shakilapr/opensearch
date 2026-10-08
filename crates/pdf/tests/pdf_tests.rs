//! Hermetic PDF pipeline tests (plan Stage 1, crawler §23).
//!
//! The two fixtures below are hand-built, byte-exact PDFs with correct cross-reference
//! tables. `TEXT_LAYER_PDF` carries a single `BT ... Tj ... ET` text object whose operand is
//! the sentinel `HELLO_SENTINEL_PDF_TEXT_LAYER`; `NO_TEXT_PDF` has a content stream with
//! drawing operators only (no text object), simulating a scanned page.

use search_pdf::{classify, extract, ocr_decision, OcrJob, PdfClass, PdfError, PdfLimits};

/// Sentinel string embedded in `TEXT_LAYER_PDF`'s text object.
const SENTINEL: &str = "HELLO_SENTINEL_PDF_TEXT_LAYER";

pub const TEXT_LAYER_PDF: &[u8] = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>\nendobj\n4 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>\nendobj\n5 0 obj\n<< /Length 61 >>\nstream\nBT /F1 24 Tf 72 700 Td (HELLO_SENTINEL_PDF_TEXT_LAYER) Tj ET\nendstream\nendobj\nxref\n0 6\n0000000000 65535 f\r\n0000000015 00000 n\r\n0000000064 00000 n\r\n0000000121 00000 n\r\n0000000247 00000 n\r\n0000000344 00000 n\r\ntrailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n454\n%%EOF\n";

pub const NO_TEXT_PDF: &[u8] = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R >>\nendobj\n4 0 obj\n<< /Length 30 >>\nstream\n0 0 1 rg 100 100 200 200 re f\nendstream\nendobj\nxref\n0 5\n0000000000 65535 f\r\n0000000015 00000 n\r\n0000000064 00000 n\r\n0000000121 00000 n\r\n0000000208 00000 n\r\ntrailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n287\n%%EOF\n";

#[test]
fn extracts_text_layer_sentinel_and_counts_one_page() {
    let outcome = extract(TEXT_LAYER_PDF, PdfLimits::default()).expect("fixture should parse");
    assert_eq!(outcome.page_count, 1, "fixture has exactly one page");
    assert!(
        outcome.text.contains(SENTINEL),
        "extracted text should contain the sentinel, got: {:?}",
        outcome.text
    );
    assert!(!outcome.used_ocr);
    assert!(
        matches!(outcome.classifier, PdfClass::TextLayer | PdfClass::Scholarly),
        "expected a text-layer class, got {:?}",
        outcome.classifier
    );
    assert!(matches!(ocr_decision(&outcome), OcrJob::Skipped { .. }));
}

#[test]
fn no_text_layer_is_scanned_and_queues_ocr() {
    let outcome = extract(NO_TEXT_PDF, PdfLimits::default()).expect("fixture should parse");
    assert_eq!(outcome.page_count, 1);
    assert!(
        outcome.text.trim().is_empty(),
        "drawing-only page should yield no text, got {:?}",
        outcome.text
    );
    assert_eq!(outcome.classifier, PdfClass::ScannedImage);
    assert!(!outcome.used_ocr);
    match ocr_decision(&outcome) {
        OcrJob::Queued { page_count } => assert_eq!(page_count, 1),
        other => panic!("expected OcrJob::Queued, got {other:?}"),
    }
}

#[test]
fn oversize_input_is_rejected_before_parsing() {
    let limits = PdfLimits {
        max_bytes: 64,
        ..PdfLimits::default()
    };
    let err = extract(TEXT_LAYER_PDF, limits).unwrap_err();
    assert_eq!(err, PdfError::TooLarge(TEXT_LAYER_PDF.len() as u64));
}

#[test]
fn page_limit_is_enforced() {
    let limits = PdfLimits {
        max_pages: 0,
        ..PdfLimits::default()
    };
    let err = extract(TEXT_LAYER_PDF, limits).unwrap_err();
    assert_eq!(err, PdfError::TooManyPages(1));
}

#[test]
fn garbage_bytes_are_a_parse_error() {
    let err = extract(b"not a pdf at all", PdfLimits::default()).unwrap_err();
    assert!(matches!(err, PdfError::Parse(_)), "got {err:?}");
}

#[test]
fn default_limits_match_spec() {
    let l = PdfLimits::default();
    assert_eq!(l.max_bytes, 64 * 1024 * 1024);
    assert_eq!(l.max_pages, 2000);
    assert_eq!(l.max_ms, 30_000);
}

#[test]
fn classify_scholarly_doi() {
    assert_eq!(
        classify("See the paper at https://doi.org/10.1234/abc.def for details.", 3),
        PdfClass::Scholarly
    );
}

#[test]
fn classify_scholarly_arxiv() {
    assert_eq!(
        classify("This work extends arXiv:2301.00001 in several ways.", 8),
        PdfClass::Scholarly
    );
}

#[test]
fn classify_scholarly_abstract_and_references() {
    assert_eq!(
        classify("Abstract We study retrieval over large corpora.", 1),
        PdfClass::Scholarly
    );
    assert_eq!(
        classify("Body text. References [1] Smith 2020.", 2),
        PdfClass::Scholarly
    );
}

#[test]
fn classify_not_scholarly_on_false_markers() {
    // "abstracted" and "referenced" must not trip the word-boundary check.
    assert_eq!(
        classify("This is a plain page about abstracted ideas and referenced figures.", 1),
        PdfClass::TextLayer
    );
}

#[test]
fn classify_plain_text_layer() {
    assert_eq!(
        classify("Just an ordinary page with plenty of readable content.", 1),
        PdfClass::TextLayer
    );
}

#[test]
fn classify_empty_is_scanned() {
    assert_eq!(classify("   \n\t ", 1), PdfClass::ScannedImage);
}

#[test]
fn classify_tiny_text_is_unknown() {
    assert_eq!(classify("hi", 1), PdfClass::Unknown);
}

#[test]
fn ocr_skipped_reason_for_text_layer() {
    let outcome = search_pdf::PdfOutcome {
        text: "ordinary content that is long enough".to_string(),
        page_count: 4,
        used_ocr: false,
        classifier: PdfClass::TextLayer,
    };
    match ocr_decision(&outcome) {
        OcrJob::Skipped { reason } => assert!(reason.contains("TextLayer"), "reason: {reason}"),
        other => panic!("expected Skipped, got {other:?}"),
    }
}
