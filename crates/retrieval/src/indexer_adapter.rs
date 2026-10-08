//! Adapter implementing [`LaneSearcher`] over the real Tantivy-backed `search-indexer`.
//!
//! Compiled only with `--features real-indexer`, so the default `search-retrieval` build
//! never drags in Tantivy. The indexer's synchronous `search(query, limit)` has no source
//! parameter, so a source scope is pushed down as a required `+source_id:"..."` clause in
//! the query string (mirroring `search-api`'s `IndexerSearcher`), and every hit is mapped
//! straight onto our [`Candidate`] with the BM25 lane tag.

use std::sync::Arc;

use search_indexer::Indexer;

use crate::{Candidate, Lane, LaneSearcher, RetrievalError};

/// Wraps a shared [`Indexer`] as a [`LaneSearcher`].
pub struct IndexerLaneSearcher {
    indexer: Arc<Indexer>,
}

impl IndexerLaneSearcher {
    pub fn new(indexer: Arc<Indexer>) -> Self {
        Self { indexer }
    }
}

impl LaneSearcher for IndexerLaneSearcher {
    fn search(
        &self,
        query: &str,
        limit: usize,
        source_id: Option<&str>,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        let query = build_query(query, source_id);
        let hits = self
            .indexer
            .search(&query, limit)
            .map_err(RetrievalError::index)?;
        Ok(hits
            .into_iter()
            .map(|r| Candidate {
                doc_id: r.doc_id,
                score: r.score,
                lane: Lane::Bm25,
                source_id: r.source_id,
                title: Some(r.title),
                url: Some(r.canonical_url),
            })
            .collect())
    }
}

/// Push a source scope into the query string as a required term.
///
/// The indexer's `QueryParser` ORs bare terms, so the clause is prefixed with `+` to make
/// it an AND constraint. The value is stripped of query-syntax characters so a hostile
/// source id cannot alter the query structure.
fn build_query(query: &str, source_id: Option<&str>) -> String {
    let Some(source_id) = source_id else {
        return query.trim().to_string();
    };
    let clean: String = source_id
        .chars()
        .filter(|c| *c != '"' && *c != '\\' && !c.is_whitespace() && *c != '+')
        .collect();
    let mut q = query.trim().to_string();
    if !clean.is_empty() {
        q.push_str(" +source_id:\"");
        q.push_str(&clean);
        q.push('"');
    }
    q
}

#[cfg(test)]
mod tests {
    use super::build_query;

    #[test]
    fn no_source_leaves_query_untouched() {
        assert_eq!(build_query("rust async", None), "rust async");
    }

    #[test]
    fn source_is_pushed_down_as_required_clause() {
        assert_eq!(
            build_query("rust async", Some("news")),
            "rust async +source_id:\"news\""
        );
    }

    #[test]
    fn hostile_source_value_is_sanitized() {
        assert_eq!(
            build_query("x", Some("bad\"injection\\term")),
            "x +source_id:\"badinjectionterm\""
        );
    }
}
