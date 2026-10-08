//! Adapter that implements the API's [`Searcher`] over the real Tantivy-backed
//! `search-indexer` crate.
//!
//! Compiled only with `--features real-indexer`, so the API crate keeps its
//! dependency graph free of the indexer (and Tantivy) by default and the
//! workspace build can never be broken by an in-flight indexer API.

use crate::query::{IndexHit, Searcher};
use search_core::{AppResult, SearchResult};
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
    fn search(&self, q: &str, limit: usize) -> AppResult<Vec<IndexHit>> {
        let hits = self.indexer.search(q, limit)?;
        Ok(hits.into_iter().map(from_search_result).collect())
    }

    // NOTE: the published manifest/generation should be read from the indexer
    // once it exposes a read side; until then the trait default (generation 0)
    // is used and `/health` reports the honest "unpublished" state.
}

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
        published_at: r.published_at,
        modified_at: r.modified_at,
        fetched_at: r.fetched_at,
        lane: r.lane,
        provenance: r.provenance,
        score: r.score,
        signals: r.signals,
    }
}
