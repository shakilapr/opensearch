//! Adapter that implements the API's [`Searcher`] over the real Tantivy-backed
//! `search-indexer` crate.
//!
//! Compiled only with `--features real-indexer`, so the API crate keeps its
//! dependency graph free of the indexer (and Tantivy) by default and the
//! workspace build can never be broken by an in-flight indexer API.

use crate::query::{IndexHit, Searcher};
use search_core::{AppResult, IndexManifest, SearchFilters, SearchResult};
use search_indexer::Indexer;
use std::sync::Arc;

/// Adapts [`search_indexer::Indexer`] to the API's [`Searcher`] trait.
pub struct IndexerSearcher {
    indexer: Arc<Indexer>,
}

impl IndexerSearcher {
    pub fn new(indexer: Arc<Indexer>) -> Self {
        Self { indexer }
    }
}

impl Searcher for IndexerSearcher {
    fn lookup_doc(&self, doc_id: &str) -> Option<IndexHit> {
        // A lane that knows only a doc_id (dense, graph, concept) needs this to have its candidates
        // rendered at all.
        let doc = self.indexer.doc_by_id(doc_id).ok().flatten()?;
        Some(IndexHit {
            doc_id: doc.doc_id,
            version: doc.version,
            resource_id: doc.resource_id,
            work_id: doc.work_id,
            content_id: doc.content_id,
            url: doc.url,
            title: doc.title,
            // No query, so no snippet: the engine renders the title and the answer path fetches
            // the body through `doc_text` when it needs real content.
            snippet: String::new(),
            source_id: doc.source_id,
            doc_type: Some(doc.doc_type).filter(|s| !s.is_empty()),
            language: Some(doc.language).filter(|s| !s.is_empty()),
            published_at: doc.published_at,
            modified_at: doc.modified_at,
            fetched_at: doc.fetched_at,
            lane: "lookup".to_string(),
            provenance: search_core::Provenance::Snapshot,
            score: 0.0,
            signals: Vec::new(),
        })
    }

    fn search(
        &self,
        q: &str,
        limit: usize,
        filters: Option<&SearchFilters>,
    ) -> AppResult<Vec<IndexHit>> {
        // The indexer's search has no filter parameters, so we PUSH DOWN the
        // facets it can answer as field-qualified query clauses (doc_type /
        // language / source_id are indexed fields in the frozen schema) and
        // POST-FILTER the temporal bounds, which need the stored published_at.
        let query = build_query(q, filters);
        let hits = self.indexer.search(&query, limit)?;
        let mut out: Vec<IndexHit> = hits.into_iter().map(from_search_result).collect();
        if let Some(f) = filters {
            out.retain(|h| passes_time(h, f));
        }
        Ok(out)
    }

    fn doc_text(&self, doc_id: &str) -> AppResult<Option<String>> {
        self.indexer.doc_text(doc_id)
    }

    fn manifest(&self) -> AppResult<IndexManifest> {
        // Read the published manifest through the indexer's own read-side API.
        // No file-path coupling: the persisted manifest location stays private
        // to the indexer crate.
        if let Some(m) = self.indexer.manifest()? {
            return Ok(m);
        }
        // Nothing published yet: report the honest unpublished state, carrying
        // the current visible generation from the public read API. A generation
        // of 0 makes `execute` mark the response `index_unpublished`.
        let generation = self.indexer.visible_generation();
        Ok(IndexManifest {
            manifest_id: if generation == 0 {
                "unpublished".to_string()
            } else {
                format!("gen-{generation}")
            },
            generation,
            doc_count: 0,
            base_segments: 0,
            watermark: self.indexer.overlay_watermark(),
            embedding_model_ids: Vec::new(),
            metadata_version: generation,
            created_at: 0,
            published_at: None,
        })
    }
}

/// Build the indexer query from the user text plus pushed-down field filters.
///
/// The indexer's `QueryParser` ORs bare terms by default, so each filter clause
/// is prefixed with `+` to make it a REQUIRED (AND) term — otherwise a positive
/// clause would broaden the query instead of constraining it.
///
/// Values are stripped of query-syntax characters (`"` `\`, whitespace) so a
/// hostile filter value cannot alter the query structure; the indexer drops the
/// whole query on a parse error, so this keeps a filter from poisoning search.
fn build_query(q: &str, filters: Option<&SearchFilters>) -> String {
    let mut query = q.trim().to_string();
    if let Some(f) = filters {
        for (field, value) in [
            ("source_id", f.source_id.as_deref()),
            ("doc_type", f.doc_type.as_deref()),
            ("language", f.language.as_deref()),
        ] {
            let Some(v) = value else { continue };
            let clean: String = v
                .chars()
                .filter(|c| *c != '"' && *c != '\\' && !c.is_whitespace() && *c != '+')
                .collect();
            if !clean.is_empty() {
                query.push(' ');
                query.push('+');
                query.push_str(field);
                query.push(':');
                query.push('"');
                query.push_str(&clean);
                query.push('"');
            }
        }
    }
    query
}

/// Temporal bounds are enforced here because the indexer has no time parameter.
fn passes_time(h: &IndexHit, f: &SearchFilters) -> bool {
    if f.after.is_none() && f.before.is_none() {
        return true;
    }
    match h.published_at {
        Some(p) => f.after.map_or(true, |a| p >= a) && f.before.map_or(true, |b| p <= b),
        // Unknown publication date cannot satisfy a bounded range.
        None => false,
    }
}

/// Faithful map of the indexer's stored result into the API's candidate hit.
/// `doc_type`/`language` are intentionally left `None`: they are indexed but not
/// stored in the frozen schema, so the indexer cannot surface them — the
/// filters for those facets are pushed into the query instead (see `build_query`).
fn from_search_result(r: SearchResult) -> IndexHit {
    IndexHit {
        doc_id: r.doc_id,
        version: r.version,
        resource_id: r.resource_id,
        work_id: r.work_id,
        content_id: r.content_id,
        url: r.canonical_url,
        title: r.title,
        snippet: r.snippet,
        source_id: r.source_id,
        doc_type: None,
        language: None,
        published_at: r.published_at,
        modified_at: r.modified_at,
        fetched_at: r.fetched_at,
        lane: r.lane,
        provenance: r.provenance,
        score: r.score,
        signals: r.signals,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_query_pushes_down_facets() {
        let f = SearchFilters {
            source_id: Some("news".into()),
            doc_type: Some("pdf".into()),
            language: Some("en".into()),
            ..Default::default()
        };
        let q = build_query("rust async", Some(&f));
        assert!(q.starts_with("rust async "));
        assert!(q.contains("+source_id:\"news\""));
        assert!(q.contains("+doc_type:\"pdf\""));
        assert!(q.contains("+language:\"en\""));
        // No filters -> text is untouched.
        assert_eq!(build_query("rust", None), "rust");
    }

    #[test]
    fn build_query_strips_query_syntax_from_values() {
        let f = SearchFilters {
            source_id: Some("bad\"injection\\term".into()),
            ..Default::default()
        };
        let q = build_query("x", Some(&f));
        assert_eq!(q, "x +source_id:\"badinjectionterm\"");
    }
}
