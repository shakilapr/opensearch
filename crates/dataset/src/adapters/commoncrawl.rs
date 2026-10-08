//! Common Crawl adapter.
//!
//! Parses a **WAT/WET-ish JSONL** fixture: one JSON object per line, e.g. a WET record
//! `{"url", "text", "timestamp"}` or a WAT metadata record with `title`/`status`/`languages`.
//! Live harvests would be pointed at a caller-supplied local shard/index path — the adapter
//! itself never opens a socket.

use std::path::PathBuf;

use crate::adapters::{clamp_page_size, index_page, read_bounded};
use crate::time::parse_epoch;
use crate::{bound_text, DatasetAdapter, DatasetError, Page, Record, MAX_RECORDS};

/// Parses Common Crawl WAT/WET-style JSONL from an injected path.
pub struct CommonCrawlAdapter {
    path: PathBuf,
    source: String,
    page_size: usize,
    records: Option<Vec<Record>>,
}

impl CommonCrawlAdapter {
    /// Adapter over a local JSONL file, with the default page size.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self::with_page_size(path, 1000)
    }

    /// Adapter over a local JSONL file with an explicit page size (clamped to the spec bound).
    pub fn with_page_size(path: impl Into<PathBuf>, page_size: usize) -> Self {
        CommonCrawlAdapter {
            path: path.into(),
            source: "commoncrawl".to_string(),
            page_size: clamp_page_size(page_size),
            records: None,
        }
    }

    fn ensure_loaded(&mut self) -> Result<(), DatasetError> {
        if self.records.is_none() {
            let bytes = read_bounded(&self.path)?;
            self.records = Some(parse_jsonl(&bytes, &self.source)?);
        }
        Ok(())
    }
}

impl DatasetAdapter for CommonCrawlAdapter {
    fn name(&self) -> &str {
        &self.source
    }

    fn page(&mut self, cursor: Option<&str>) -> Result<Page, DatasetError> {
        self.ensure_loaded()?;
        let records = self.records.as_deref().unwrap_or(&[]);
        index_page(records, cursor, self.page_size)
    }
}

/// Parse a JSONL body into records (blank lines skipped, bounded by [`MAX_RECORDS`]).
fn parse_jsonl(bytes: &[u8], source: &str) -> Result<Vec<Record>, DatasetError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| DatasetError::Parse(format!("commoncrawl fixture is not UTF-8: {e}")))?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| DatasetError::Parse(format!("line {}: {e}", i + 1)))?;
        out.push(record_from_value(&value, source));
        if out.len() >= MAX_RECORDS {
            break;
        }
    }
    Ok(out)
}

fn record_from_value(v: &serde_json::Value, source: &str) -> Record {
    let get = |key: &str| v.get(key).and_then(|x| x.as_str());
    let url = get("url").map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let id = url.clone().unwrap_or_else(|| {
        // Fall back to a stable synthetic id when the record lacks a URL.
        format!("{source}:{}", crate::checksum::fnv1a64(v.to_string().as_bytes()))
    });

    let mut record = Record::new(id, source);
    record.url = url;
    record.title = crate::non_empty(get("title"));
    record.text = bound_text(get("text").or_else(|| get("content")).unwrap_or(""));
    record.published_at = get("timestamp").or_else(|| get("warc_date")).and_then(parse_epoch);
    for key in ["status", "mime", "languages", "content_type"] {
        if let Some(value) = get(key) {
            if !value.trim().is_empty() {
                record.extras.insert(key.to_string(), value.trim().to_string());
            }
        }
    }
    record
}
