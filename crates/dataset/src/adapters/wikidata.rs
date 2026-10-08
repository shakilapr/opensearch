//! Wikidata adapter.
//!
//! Parses a **Wikidata entity JSON** fixture: a JSON array of entity objects (the shape used by
//! `Special:EntityData`/the entity dumps), e.g.
//! `{"id":"Q42","type":"item","labels":{"en":{"value":"Douglas Adams"}},
//!   "sitelinks":{"enwiki":{"title":"Douglas Adams"}}}`.
//! The adapter is offline; a live dump path would be injected by the caller.

use std::path::PathBuf;

use crate::adapters::{clamp_page_size, index_page, read_bounded};
use crate::{bound_text, DatasetAdapter, DatasetError, Page, Record, MAX_RECORDS};

/// Parses Wikidata entity JSON from an injected path.
pub struct WikidataAdapter {
    path: PathBuf,
    source: String,
    page_size: usize,
    records: Option<Vec<Record>>,
}

impl WikidataAdapter {
    /// Adapter over a local entity JSON file, with the default page size.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self::with_page_size(path, 1000)
    }

    /// Adapter over a local entity JSON file with an explicit page size (clamped to the bound).
    pub fn with_page_size(path: impl Into<PathBuf>, page_size: usize) -> Self {
        WikidataAdapter {
            path: path.into(),
            source: "wikidata".to_string(),
            page_size: clamp_page_size(page_size),
            records: None,
        }
    }

    fn ensure_loaded(&mut self) -> Result<(), DatasetError> {
        if self.records.is_none() {
            let bytes = read_bounded(&self.path)?;
            let value: serde_json::Value = serde_json::from_slice(&bytes)?;
            let entities = match &value {
                serde_json::Value::Array(items) => items.clone(),
                // Tolerate a `{"entities": {...}}` wrapper / id->entity map.
                serde_json::Value::Object(map) => match map.get("entities") {
                    Some(serde_json::Value::Array(items)) => items.clone(),
                    Some(serde_json::Value::Object(by_id)) => by_id.values().cloned().collect(),
                    _ => map.values().cloned().collect(),
                },
                _ => {
                    return Err(DatasetError::Parse(
                        "wikidata fixture must be a JSON array or object".into(),
                    ))
                }
            };
            let mut out = Vec::new();
            for entity in &entities {
                out.push(record_from_entity(entity, &self.source));
                if out.len() >= MAX_RECORDS {
                    break;
                }
            }
            self.records = Some(out);
        }
        Ok(())
    }
}

impl DatasetAdapter for WikidataAdapter {
    fn name(&self) -> &str {
        &self.source
    }

    fn page(&mut self, cursor: Option<&str>) -> Result<Page, DatasetError> {
        self.ensure_loaded()?;
        let records = self.records.as_deref().unwrap_or(&[]);
        index_page(records, cursor, self.page_size)
    }
}

fn record_from_entity(entity: &serde_json::Value, source: &str) -> Record {
    let id = entity
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let title = preferred_label(entity, "labels");
    let description = preferred_label(entity, "descriptions");
    let url = entity
        .get("sitelinks")
        .and_then(|s| s.get("enwiki"))
        .and_then(|s| s.get("title"))
        .and_then(|t| t.as_str())
        .map(|t| format!("https://en.wikipedia.org/wiki/{}", t.replace(' ', "_")));

    let mut record = Record::new(id, source);
    record.url = url;
    record.title = title.clone();
    record.text = bound_text(&match (title, description) {
        (_, Some(d)) => d,
        (Some(t), None) => t,
        (None, None) => String::new(),
    });
    if let Some(kind) = entity.get("type").and_then(|v| v.as_str()) {
        record.extras.insert("type".to_string(), kind.to_string());
    }
    if let Some(datatype) = entity.get("datatype").and_then(|v| v.as_str()) {
        record.extras.insert("datatype".to_string(), datatype.to_string());
    }
    if let Some(claims) = entity.get("claims").and_then(|v| v.as_object()) {
        record.extras.insert("claims".to_string(), claims.len().to_string());
    }
    record
}

/// Pick `labels`/`descriptions` in `en`, then `en-*`, then any language.
fn preferred_label(entity: &serde_json::Value, field: &str) -> Option<String> {
    let map = entity.get(field)?.as_object()?;
    let value_of = |v: &serde_json::Value| -> Option<String> {
        v.get("value")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    if let Some(v) = map.get("en").and_then(value_of) {
        return Some(v);
    }
    let mut en_variant: Option<String> = None;
    let mut any: Option<String> = None;
    for (lang, value) in map {
        if let Some(s) = value_of(value) {
            if lang.starts_with("en") && en_variant.is_none() {
                en_variant = Some(s.clone());
            }
            if any.is_none() {
                any = Some(s);
            }
        }
    }
    en_variant.or(any)
}
