//! Dataset builder (plan Stage 6): Common Crawl / Wikidata / OpenAlex / OAI-PMH adapters and
//! release packs. Interfaces frozen in `docs/STAGE6_SPEC.md`.
//!
//! Every adapter is **offline-testable**: it reads an injected path/dir (a committed fixture in
//! tests) and never performs network I/O or reads a credential. Parsing is bounded the same way
//! the discovery/pdf crates bound theirs: a hard cap on a single document, on per-record text,
//! and on the number of records returned by one [`DatasetAdapter::page`] call.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use thiserror::Error;

mod adapters;
mod checksum;
mod pack;
mod time;

pub use adapters::{CommonCrawlAdapter, OaiPmhAdapter, OpenAlexAdapter, WikidataAdapter};
pub use pack::{build_from_adapter, validate_pack, write_pack, PackManifest, ShardInfo};

/// Hard cap on `Record::text` (spec bound: 8 MiB per record).
pub const MAX_RECORD_TEXT_BYTES: usize = 8 * 1024 * 1024;
/// Hard cap on records returned by a single [`DatasetAdapter::page`] call.
pub const MAX_PAGE_SIZE: usize = 10_000;
/// Hard cap on how many records an adapter loads from one fixture document.
pub const MAX_RECORDS: usize = 1_000_000;
/// Hard cap on a fixture/document body (spec bound: 64 MiB).
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Manifest schema version written by [`write_pack`].
pub const PACK_SCHEMA: u32 = 1;

/// One harvested record, the unit written to a pack shard as a JSON object.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Record {
    pub id: String,
    /// Origin adapter: "commoncrawl" | "wikidata" | "openalex" | "oai-pmh" | ...
    pub source: String,
    pub url: Option<String>,
    pub title: Option<String>,
    pub text: String,
    /// Unix epoch seconds, when the source carries a parseable date.
    pub published_at: Option<i64>,
    pub extras: BTreeMap<String, String>,
}

impl Record {
    /// A record with every optional field empty, sourced from `source`.
    pub fn new(id: impl Into<String>, source: impl Into<String>) -> Self {
        Record {
            id: id.into(),
            source: source.into(),
            url: None,
            title: None,
            text: String::new(),
            published_at: None,
            extras: BTreeMap::new(),
        }
    }
}

/// One page of records plus the opaque cursor for the following page (`None` = last page).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub records: Vec<Record>,
    pub next: Option<String>,
}

/// A cursor-paged source of [`Record`]s.
///
/// `cursor` is the opaque token returned as [`Page::next`] by a previous call; `None` asks for
/// the first page. An adapter with no more records returns [`DatasetError::Exhausted`].
pub trait DatasetAdapter {
    fn name(&self) -> &str;
    fn page(&mut self, cursor: Option<&str>) -> Result<Page, DatasetError>;
}

/// Errors produced by adapters and pack I/O.
#[derive(Debug, Error)]
pub enum DatasetError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("http error: {0}")]
    Http(String),
    #[error("adapter exhausted")]
    Exhausted,
}

impl From<serde_json::Error> for DatasetError {
    fn from(e: serde_json::Error) -> Self {
        DatasetError::Parse(e.to_string())
    }
}

/// Truncate `text` to [`MAX_RECORD_TEXT_BYTES`] on a UTF-8 char boundary.
fn bound_text(text: &str) -> String {
    if text.len() <= MAX_RECORD_TEXT_BYTES {
        return text.to_string();
    }
    let mut end = MAX_RECORD_TEXT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Parse an optional string field, mapping empty/whitespace to `None`.
fn non_empty(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}
