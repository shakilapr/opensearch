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
    /// The lexical evidence kept in the index: ~500 chars of `body`, derived at write time when
    /// the caller leaves it empty. Never the article — see [`crate::schema`].
    pub excerpt: String,
    /// Full extracted text. **Ingest-side only**: it is written to `text/<hash>.txt` and read
    /// back on demand; it is never a Tantivy field. A document rebuilt from the index carries an
    /// empty `body` plus its `excerpt`, which is the honest shape.
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
            excerpt: String::new(),
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

    /// Set the indexed excerpt explicitly. Left empty, [`IndexDoc::to_tantivy`] derives it from
    /// `body`, so a caller that has the full text does not have to compute it twice.
    pub fn with_excerpt(mut self, excerpt: impl Into<String>) -> Self {
        self.excerpt = excerpt.into();
        self
    }

    pub fn with_source(mut self, source_id: impl Into<String>) -> Self {
        self.source_id = Some(source_id.into());
        self
    }

    /// Materialize this doc into a Tantivy document.
    ///
    /// `title_boost` duplicates `title` because Tantivy cannot boost inside a field; `excerpt` is
    /// the only body-derived field, and `body` itself is never written — the index stores the
    /// address and short evidence, not the article.
    pub fn to_tantivy(&self, schema: &Schema) -> TantivyDocument {
        let f = |name: &str| -> Field {
            schema
                .get_field(name)
                .unwrap_or_else(|_| panic!("schema is missing field `{name}`"))
        };
        let excerpt = if self.excerpt.is_empty() {
            excerpt_of(&self.body)
        } else {
            self.excerpt.clone()
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
        d.add_text(f("excerpt"), excerpt.as_str());
        // Comma-joined, not space-joined: the field is STOREd and read back by `to_index_doc` for
        // export, and a space join cannot be split again without destroying multi-word entities
        // ("paged attention" became two entities). Tantivy tokenises on punctuation, so the indexed
        // terms are unchanged.
        d.add_text(f("entities"), self.entities.join(", "));
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

/// Number of characters of the document the index keeps as lexical evidence.
///
/// 500 is the figure `index-upgrade-plan.md` costed the target model at. It is enough for BM25 to
/// see body prose and for a snippet to be a real fragment; it is ~5% of the measured average body
/// (10.6 KB), which is the point.
pub const EXCERPT_CHARS: usize = 500;

/// Derive the indexed excerpt from the extracted text.
///
/// Deterministic and whitespace-normalised, so a rebuilt index is reproducible and an excerpt can
/// be compared byte-for-byte in a test. Never slices at a byte offset: a page whose 500th byte is
/// inside a multi-byte character would panic, and non-Latin corpora are ordinary input here.
pub fn excerpt_of(body: &str) -> String {
    let normalised: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalised.chars().count() <= EXCERPT_CHARS {
        return normalised;
    }
    let mut out: String = normalised.chars().take(EXCERPT_CHARS).collect();
    // Cut at a word boundary when there is one in the back half, so an excerpt does not end
    // mid-word. `rfind` returns a byte index that is a char boundary, so `truncate` is safe.
    if let Some(cut) = out.rfind(char::is_whitespace) {
        if cut > EXCERPT_CHARS / 2 {
            out.truncate(cut);
        }
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_body_is_its_own_excerpt() {
        assert_eq!(excerpt_of("  rust async \n runtime "), "rust async runtime");
    }

    #[test]
    fn a_long_body_is_cut_at_a_word_boundary_within_the_budget() {
        let body = "word ".repeat(400);
        let e = excerpt_of(&body);
        assert!(e.chars().count() <= EXCERPT_CHARS + 1, "bounded: {}", e.chars().count());
        assert!(e.ends_with('…'), "truncation is visible: {e}");
        assert!(!e.contains("wor…"), "cut between words, not inside one");
    }

    #[test]
    fn a_multibyte_body_does_not_panic_on_the_boundary() {
        // 三 is 3 bytes: a byte-offset slice at 500 would land inside a character.
        let body = "三".repeat(600);
        let e = excerpt_of(&body);
        assert_eq!(e.chars().count(), EXCERPT_CHARS + 1);
    }

    #[test]
    fn the_excerpt_is_independent_of_repeated_whitespace() {
        assert_eq!(excerpt_of("a  b\tc\nd"), excerpt_of("a b c d"));
    }
}
