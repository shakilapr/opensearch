//! Adapters for the four Stage-6 sources.
//!
//! Each adapter reads an **injected path** (a committed fixture in tests) — there is no network
//! access, no credentials, and no hardcoded keys anywhere in this module tree.

mod commoncrawl;
mod oai_pmh;
mod openalex;
mod wikidata;

pub use commoncrawl::CommonCrawlAdapter;
pub use oai_pmh::OaiPmhAdapter;
pub use openalex::OpenAlexAdapter;
pub use wikidata::WikidataAdapter;

use std::path::Path;

use crate::{DatasetError, Page, MAX_FILE_BYTES, MAX_PAGE_SIZE};

/// Read a fixture/document, enforcing [`MAX_FILE_BYTES`] before and after the read.
pub(crate) fn read_bounded(path: &Path) -> Result<Vec<u8>, DatasetError> {
    let meta = std::fs::metadata(path)?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(DatasetError::Parse(format!(
            "file {} exceeds size limit: {} bytes",
            path.display(),
            meta.len()
        )));
    }
    let bytes = std::fs::read(path)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(DatasetError::Parse(format!(
            "file {} exceeds size limit: {} bytes",
            path.display(),
            bytes.len()
        )));
    }
    Ok(bytes)
}

/// Clamp a caller-supplied page size to `[1, MAX_PAGE_SIZE]`.
pub(crate) fn clamp_page_size(page_size: usize) -> usize {
    page_size.clamp(1, MAX_PAGE_SIZE)
}

/// Slice an index-addressed record vector into one page.
///
/// `cursor` is the next start index (`None` = 0). Returns [`DatasetError::Exhausted`] once the
/// cursor reaches the end, and `next = Some(end)` while records remain.
pub(crate) fn index_page(
    records: &[crate::Record],
    cursor: Option<&str>,
    page_size: usize,
) -> Result<Page, DatasetError> {
    let start: usize = match cursor {
        Some(c) => c
            .parse()
            .map_err(|_| DatasetError::Parse(format!("invalid cursor: {c}")))?,
        None => 0,
    };
    if start >= records.len() {
        return Err(DatasetError::Exhausted);
    }
    let end = (start + page_size).min(records.len());
    let next = if end < records.len() {
        Some(end.to_string())
    } else {
        None
    };
    Ok(Page {
        records: records[start..end].to_vec(),
        next,
    })
}
