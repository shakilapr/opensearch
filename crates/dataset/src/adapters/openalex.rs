//! OpenAlex adapter.
//!
//! Parses an **OpenAlex works JSON** fixture — the shape returned by the `/works` API:
//! `{"meta": {...}, "results": [ {"id", "display_name", "publication_date",
//! "abstract_inverted_index", "primary_location": {"landing_page_url"}, ...}, ... ]}`.
//! The abstract is reconstructed from `abstract_inverted_index` (the same lossless encoding the
//! API uses). A live snapshot path would be injected by the caller; the adapter does no I/O
//! beyond the file it is given.

use std::path::PathBuf;

use crate::adapters::{clamp_page_size, index_page, read_bounded};
use crate::time::parse_epoch;
use crate::{bound_text, DatasetAdapter, DatasetError, Page, Record, MAX_RECORDS};

/// Parses OpenAlex work JSON from an injected path.
pub struct OpenAlexAdapter {
    path: PathBuf,
    source: String,
    page_size: usize,
    records: Option<Vec<Record>>,
}

impl OpenAlexAdapter {
    /// Adapter over a local works JSON file, with the default page size.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self::with_page_size(path, 1000)
    }

    /// Adapter over a local works JSON file with an explicit page size (clamped to the bound).
    pub fn with_page_size(path: impl Into<PathBuf>, page_size: usize) -> Self {
        OpenAlexAdapter {
            path: path.into(),
            source: "openalex".to_string(),
            page_size: clamp_page_size(page_size),
            records: None,
        }
    }

    fn ensure_loaded(&mut self) -> Result<(), DatasetError> {
        if self.records.is_none() {
            let bytes = read_bounded(&self.path)?;
            let value: serde_json::Value = serde_json::from_slice(&bytes)?;
            let works = match &value {
                serde_json::Value::Object(map) => match map.get("results") {
                    Some(serde_json::Value::Array(items)) => items.clone(),
                    _ => {
                        return Err(DatasetError::Parse(
                            "openalex fixture object is missing a `results` array".into(),
                        ))
                    }
                },
                serde_json::Value::Array(items) => items.clone(),
                _ => {
                    return Err(DatasetError::Parse(
                        "openalex fixture must be a JSON object or array".into(),
                    ))
                }
            };
            let mut out = Vec::new();
            for work in &works {
                out.push(record_from_work(work, &self.source));
                if out.len() >= MAX_RECORDS {
                    break;
                }
            }
            self.records = Some(out);
        }
        Ok(())
    }
}

impl DatasetAdapter for OpenAlexAdapter {
    fn name(&self) -> &str {
        &self.source
    }

    fn page(&mut self, cursor: Option<&str>) -> Result<Page, DatasetError> {
        self.ensure_loaded()?;
        let records = self.records.as_deref().unwrap_or(&[]);
        index_page(records, cursor, self.page_size)
    }
}

fn record_from_work(work: &serde_json::Value, source: &str) -> Record {
    let get = |key: &str| work.get(key).and_then(|v| v.as_str());
    let id = get("id")
        .or_else(|| get("doi"))
        .unwrap_or("")
        .trim()
        .to_string();
    let doi = get("doi").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let title = get("display_name").or_else(|| get("title")).map(str::trim).filter(|s| !s.is_empty());
    let url = work
        .get("primary_location")
        .and_then(|p| p.get("landing_page_url"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| doi.clone());

    let abstract_text = work
        .get("abstract_inverted_index")
        .and_then(reconstruct_abstract)
        .filter(|s| !s.is_empty());

    let text = abstract_text.or_else(|| title.map(str::to_string)).unwrap_or_default();

    let mut record = Record::new(id, source);
    record.url = url;
    record.title = title.map(str::to_string);
    record.text = bound_text(&text);
    record.published_at = get("publication_date").and_then(parse_epoch);
    if let Some(d) = doi {
        record.extras.insert("doi".to_string(), d);
    }
    if let Some(kind) = get("type") {
        record.extras.insert("type".to_string(), kind.to_string());
    }
    if let Some(count) = work.get("cited_by_count").and_then(|v| v.as_i64()) {
        record.extras.insert("cited_by_count".to_string(), count.to_string());
    }
    record
}

/// Rebuild an abstract string from `{word: [positions]}`.
fn reconstruct_abstract(index: &serde_json::Value) -> Option<String> {
    let map = index.as_object()?;
    let mut positioned: Vec<(i64, &str)> = Vec::new();
    for (word, positions) in map {
        if let Some(arr) = positions.as_array() {
            for pos in arr {
                if let Some(p) = pos.as_i64() {
                    positioned.push((p, word.as_str()));
                }
            }
        }
    }
    if positioned.is_empty() {
        return None;
    }
    positioned.sort_by_key(|(p, _)| *p);
    let mut out = String::new();
    for (_, word) in positioned {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    Some(out)
}
