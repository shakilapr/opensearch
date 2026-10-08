//! PDF pipeline (plan Stage 1, crawler §23).
//!
//! size check → hash dedup → text-layer test → classifier → GROBID/OCR routing. OCR is an
//! interface only in Stage 1. Interfaces frozen in `docs/STAGE1_SPEC.md`.

#![forbid(unsafe_code)]

use std::time::Instant;

/// Document-type classification produced by [`classify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdfClass {
    /// A normal PDF with an extractable text layer.
    TextLayer,
    /// No text layer — a scanned/image-only PDF (OCR candidate).
    ScannedImage,
    /// A scholarly document (DOI / arXiv id / abstract / references markers).
    Scholarly,
    /// Could not be classified (e.g. too little text to be meaningful).
    Unknown,
}

/// Resource bounds applied before and during parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PdfLimits {
    pub max_bytes: u64,
    pub max_pages: u32,
    pub max_ms: u64,
}

impl Default for PdfLimits {
    fn default() -> Self {
        PdfLimits {
            max_bytes: 64 * 1024 * 1024, // 64 MiB
            max_pages: 2000,
            max_ms: 30_000,
        }
    }
}

/// Result of a successful extraction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfOutcome {
    pub text: String,
    pub page_count: u32,
    pub used_ocr: bool,
    pub classifier: PdfClass,
}

/// Work item for the (Stage-1 interface-only) OCR queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OcrJob {
    Queued { page_count: u32 },
    Skipped { reason: String },
}

/// Errors produced by [`extract`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PdfError {
    #[error("pdf exceeds size limit: {0} bytes")]
    TooLarge(u64),
    #[error("pdf has too many pages: {0}")]
    TooManyPages(u32),
    #[error("pdf parse error: {0}")]
    Parse(String),
    #[error("pdf processing exceeded time budget: {0} ms")]
    Timeout(u64),
}

/// Validate bounds, extract the text layer, count pages and classify a PDF.
///
/// Enforces [`PdfLimits::max_bytes`] *before* touching the parser, then
/// [`PdfLimits::max_pages`] once the page tree is known, then a best-effort
/// wall-clock check against [`PdfLimits::max_ms`] (parsing is synchronous, so the
/// budget is verified after each step rather than pre-empted mid-parse).
pub fn extract(bytes: &[u8], limits: PdfLimits) -> Result<PdfOutcome, PdfError> {
    // 1. Size gate — cheapest check, done before any parsing work.
    if bytes.len() as u64 > limits.max_bytes {
        return Err(PdfError::TooLarge(bytes.len() as u64));
    }

    let started = Instant::now();

    // 2. Load the document to obtain an authoritative page count.
    let doc = lopdf::Document::load_mem(bytes).map_err(|e| PdfError::Parse(e.to_string()))?;
    let page_count = doc.get_pages().len() as u32;

    if page_count > limits.max_pages {
        return Err(PdfError::TooManyPages(page_count));
    }
    if elapsed_ms(started) > limits.max_ms {
        return Err(PdfError::Timeout(limits.max_ms));
    }

    // 3. Extract the text layer.
    let text = pdf_extract::extract_text_from_mem(bytes).map_err(|e| PdfError::Parse(e.to_string()))?;

    if elapsed_ms(started) > limits.max_ms {
        return Err(PdfError::Timeout(limits.max_ms));
    }

    // 4. Classify: an empty/whitespace text layer means a scanned image.
    let classifier = if text.trim().is_empty() {
        PdfClass::ScannedImage
    } else {
        classify(&text, page_count)
    };

    Ok(PdfOutcome {
        text,
        page_count,
        used_ocr: false,
        classifier,
    })
}

/// Classify a document from its extracted text and page count.
pub fn classify(text: &str, page_count: u32) -> PdfClass {
    let _ = page_count; // page count does not currently affect the class

    if text.trim().is_empty() {
        return PdfClass::ScannedImage;
    }
    if is_scholarly(text) {
        return PdfClass::Scholarly;
    }
    if is_non_trivial(text) {
        PdfClass::TextLayer
    } else {
        PdfClass::Unknown
    }
}

/// Decide whether an OCR job should be queued for an outcome (Stage-1 interface only).
pub fn ocr_decision(outcome: &PdfOutcome) -> OcrJob {
    match outcome.classifier {
        PdfClass::ScannedImage => OcrJob::Queued {
            page_count: outcome.page_count,
        },
        other => OcrJob::Skipped {
            reason: format!("text layer present (classifier: {other:?})"),
        },
    }
}

/// Scholarly heuristics: DOI, arXiv id, or the words "abstract"/"references".
fn is_scholarly(text: &str) -> bool {
    if has_doi(text) {
        return true;
    }
    let lower = text.to_ascii_lowercase();
    if lower.contains("arxiv") {
        return true;
    }
    contains_word(&lower, "abstract") || contains_word(&lower, "references")
}

/// True for a DOI-shaped token: `10.` + at least four digits + `/` + a non-space char.
fn has_doi(text: &str) -> bool {
    let b = text.as_bytes();
    let mut from = 0;
    while let Some(pos) = text[from..].find("10.") {
        let at = from + pos;
        let mut j = at + 3;
        let mut digits = 0;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
            digits += 1;
        }
        if digits >= 4
            && j < b.len()
            && b[j] == b'/'
            && j + 1 < b.len()
            && !b[j + 1].is_ascii_whitespace()
        {
            return true;
        }
        from = at + 3;
    }
    false
}

fn contains_word(haystack_lower: &str, word: &str) -> bool {
    let mut from = 0;
    while let Some(pos) = haystack_lower[from..].find(word) {
        let at = from + pos;
        let before_ok = at == 0
            || !haystack_lower.as_bytes()[at - 1].is_ascii_alphanumeric();
        let after = at + word.len();
        let after_ok =
            after >= haystack_lower.len() || !haystack_lower.as_bytes()[after].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        from = at + word.len();
    }
    false
}

/// "Non-trivial" = at least a handful of non-whitespace characters.
fn is_non_trivial(text: &str) -> bool {
    let chars = text.chars().filter(|c| !c.is_whitespace()).count();
    chars >= 16
}

fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}
