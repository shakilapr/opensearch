//! `IndexDoc` — the ingest-side projection of one Tantivy document (spec §6).
//!
//! It mirrors the frozen schema fields 1:1. Boost/concatenation fields are
//! materialized explicitly because Tantivy enforces a strict field<->index
//! relationship (no engine-side duplication).

use tantivy::schema::{Schema, TantivyDocument};
use tantivy::schema::Field;

/// One document to index. `doc_id` is the delete term; `version` orders updates
/// so an overlay write shadows an older base write for the same `doc_id`.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexDoc {
    pub doc_id: String,
    pub version: u64,
    pub resource_id: String,
    pub work_id: Option<String>,
    pub content_id: Option<String>,
    /// Canonical URL (stored in the `url` field).
    pub url: String,
    pub title: String,
    pub headings: String,
    pub body: String,
    pub entities: Vec<String>,
    pub source_id: Option<String>,
    pub doc_type: String,
    pub language: String,
    /// `None` = unknown; stored as -1, never derived from `fetched_at`.
    pub published_at: Option<i64>,
    pub modified_at: Option<i64>,
    pub fetched_at: i64,
    /// Ingestion high-water mark; search exposes the doc only once a published
    /// generation reaches this value (watermark gating, spec §6/§3).
    pub visible_generation: u64,
    pub tombstone: bool,
}

impl IndexDoc {
    /// A minimal live document.
    pub fn new(
        doc_id: impl Into<String>,
        resource_id: impl Into<String>,
        url: impl Into<String>,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            doc_id: doc_id.into(),
            version: 1,
            resource_id: resource_id.into(),
            work_id: None,
            content_id: None,
            url: url.into(),
            title: title.into(),
            headings: String::new(),
            body: body.into(),
            entities: Vec::new(),
            source_id: None,
            doc_type: "html".to_string(),
            language: "en".to_string(),
            published_at: None,
            modified_at: None,
            fetched_at: 0,
            visible_generation: 0,
            tombstone: false,
        }
    }

    /// A tombstone marker for `doc_id` at `version`. Search filters tombstones
    /// out; kept so an overlay delete shadows a base copy by version.
    pub fn tombstone(doc_id: impl Into<String>, version: u64) -> Self {
        let mut d = Self::new(doc_id, "", "", "", "");
        d.version = version;
        d.tombstone = true;
        d
    }

    pub fn with_version(mut self, version: u64) -> Self {
        self.version = version;
        self
    }

    pub fn with_generation(mut self, generation: u64) -> Self {
        self.visible_generation = generation;
        self
    }

    pub fn with_headings(mut self, headings: impl Into<String>) -> Self {
        self.headings = headings.into();
        self
    }

    pub fn with_source(mut self, source_id: impl Into<String>) -> Self {
        self.source_id = Some(source_id.into());
        self
    }

    /// Materialize this doc into a Tantivy document. `body_boost` is the
    /// title+headings+body concatenation; `title_boost` duplicates `title`.
    pub fn to_tantivy(&self, schema: &Schema) -> TantivyDocument {
        let f = |name: &str| -> Field {
            schema
                .get_field(name)
                .unwrap_or_else(|_| panic!("schema is missing field `{name}`"))
        };
        let mut d = TantivyDocument::default();
        d.add_text(f("doc_id"), self.doc_id.as_str());
        d.add_u64(f("version"), self.version);
        d.add_text(f("resource_id"), self.resource_id.as_str());
        d.add_text(f("work_id"), self.work_id.clone().unwrap_or_default());
        d.add_text(f("content_id"), self.content_id.clone().unwrap_or_default());
        d.add_text(f("url"), self.url.as_str());
        d.add_text(f("title"), self.title.as_str());
        d.add_text(f("title_boost"), self.title.as_str());
        d.add_text(f("headings"), self.headings.as_str());
        d.add_text(f("body"), self.body.as_str());
        let body_boost = format!("{} {} {}", self.title, self.headings, self.body);
        d.add_text(f("body_boost"), body_boost);
        d.add_text(f("entities"), self.entities.join(" "));
        d.add_text(f("source_id"), self.source_id.clone().unwrap_or_default());
        d.add_text(f("doc_type"), self.doc_type.as_str());
        d.add_text(f("language"), self.language.as_str());
        d.add_i64(f("published_at"), self.published_at.unwrap_or(-1));
        d.add_i64(f("modified_at"), self.modified_at.unwrap_or(-1));
        d.add_i64(f("fetched_at"), self.fetched_at);
        d.add_u64(f("visible_generation"), self.visible_generation);
        d.add_u64(f("tombstone"), u64::from(self.tombstone));
        d
    }
}
