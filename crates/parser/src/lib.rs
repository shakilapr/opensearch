//! `parser` — response-body normalization (spec §3 NORMALIZED stage).
//!
//! Stage 0 supports HTML. PDF/feed/JSON adapters are later stages; the entry
//! point is expressed as a router so they can be added without changing callers.

pub mod html;

pub use html::{normalize_html, NormalizedDoc};

use search_core::{AppError, AppResult};

/// Normalize a fetched body based on its content type.
///
/// Stage 0 routes `text/html` (and unknown/missing types, treated as HTML) to
/// the HTML adapter; everything else is rejected with `AppError::Invalid` so the
/// ingest pipeline records the failure rather than indexing garbage.
pub fn normalize(url: &str, body: &[u8], content_type: Option<&str>) -> AppResult<NormalizedDoc> {
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    let is_html = ct.is_empty() || ct.contains("html") || ct.contains("xhtml");
    if is_html {
        normalize_html(url, body)
    } else {
        Err(AppError::Invalid(format!(
            "no Stage-0 adapter for content type `{ct}`"
        )))
    }
}
