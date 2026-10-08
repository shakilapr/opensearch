//! Adapter that implements the API's [`Searcher`] over the real Tantivy-backed
//! `search-indexer` crate.
//!
//! Compiled only with `--features real-indexer`, so the API crate keeps its
//! dependency graph free of the indexer (and Tantivy) by default and the
//! workspace build can never be broken by an in-flight indexer API.

use crate::query::{IndexHit, Searcher};
use search_core::{AppResult, IndexManifest, SearchFilters, SearchResult};
use search_indexer::Indexer;
use std::path::Path;
use std::sync::Arc;

/// The manifest file the indexer writes on every publish (`indexer::store`).
const MANIFEST_FILE: &str = "_manifest.json";

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

    fn manifest(&self) -> AppResult<IndexManifest> {
        // Read-side gap: `search_indexer::Indexer` exposes no `manifest()`
        // getter, only `publish_manifest()` (write side) plus the public
        // `visible_generation()`/`overlay_watermark()`/`base_dir()` accessors.
        // It DOES persist the current published manifest as `_manifest.json`
        // under the public `base_dir()`, so we read that file back here. If the
        // indexer ever grows an in-memory getter, use it instead.
        let path = self.indexer.base_dir().join(MANIFEST_FILE);
        if path.exists() {
            return read_manifest(&path);
        }
        // Nothing published yet: report the honest unpublished state, carrying
        // the current visible generation from the public read API.
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

/// Deserialize a persisted manifest file. `IndexManifest` is `Deserialize` (the
/// indexer uses the same type), so this is a faithful read of the published row.
fn read_manifest(path: &Path) -> AppResult<IndexManifest> {
    let bytes = std::fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
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
