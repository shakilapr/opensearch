//! Retrieval lanes (plan Stage 1, engine §17).
//!
//! Source-first retrieval: run BM25 scoped to each source whose profile matches the query,
//! then merge global BM25 and source-scoped candidates. RRF fusion and reranking arrive in
//! Stage 2. Interfaces frozen in `docs/STAGE1_SPEC.md`.
//!
//! Everything here is synchronous: the index backend ([`search_indexer`]) is a synchronous
//! Tantivy wrapper, so the [`LaneSearcher`] boundary is sync too. A test/agent backend that
//! needs no Tantivy index is provided by [`MemoryLaneSearcher`]; the real indexer is wired
//! through [`IndexerLaneSearcher`] behind the `real-indexer` feature so the default build
//! never depends on Tantivy.

#![forbid(unsafe_code)]

pub mod dense;

pub use dense::DenseLane;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

#[cfg(feature = "real-indexer")]
mod indexer_adapter;
mod memory;
mod sparse;

#[cfg(feature = "real-indexer")]
pub use indexer_adapter::IndexerLaneSearcher;
pub use memory::{MemoryDoc, MemoryLaneSearcher};
pub use sparse::SparseExpander;

/// Which retrieval lane produced a candidate.
///
/// `Bm25` is the plain lexical lane (global, or a backend default); `SourceFirst` is a hit
/// that came from a search scoped to one of the [`SourceFirst`] lane's sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Lane {
    #[default]
    Bm25,
    SourceFirst,
    /// Semantic lane over a document-vector store (plan §9.3).
    Dense,
    /// Entity-graph lane: candidates reached by activating the entity graph around the entities a
    /// query names (plan §9.4). Its hits are what the API reports as `graph_expansion`.
    Graph,
}

/// A scored retrieval candidate carrying enough provenance for the UI's
/// "Why this result?" panel.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub doc_id: String,
    pub score: f32,
    pub lane: Lane,
    pub source_id: Option<String>,
    pub title: Option<String>,
    pub url: Option<String>,
}

impl Candidate {
    /// A candidate from the BM25 lane with no source/title/url metadata.
    pub fn new(doc_id: impl Into<String>, score: f32) -> Self {
        Self {
            doc_id: doc_id.into(),
            score,
            lane: Lane::Bm25,
            source_id: None,
            title: None,
            url: None,
        }
    }

    /// Builder: override the lane.
    pub fn with_lane(mut self, lane: Lane) -> Self {
        self.lane = lane;
        self
    }

    /// Builder: attach the owning source id.
    pub fn with_source(mut self, source_id: impl Into<String>) -> Self {
        self.source_id = Some(source_id.into());
        self
    }

    /// Builder: attach display metadata.
    pub fn with_meta(
        mut self,
        title: impl Into<String>,
        url: impl Into<String>,
    ) -> Self {
        self.title = Some(title.into());
        self.url = Some(url.into());
        self
    }
}

/// Errors surfaced by a retrieval lane.
#[derive(Debug, thiserror::Error)]
pub enum RetrievalError {
    /// A search backend (e.g. the Tantivy indexer) failed. The message carries the
    /// backend's own error text so callers never need a dependency on it.
    #[error("index backend error: {0}")]
    Index(String),
    /// The lane was called with parameters it cannot honor.
    #[error("invalid retrieval request: {0}")]
    Invalid(String),
}

impl RetrievalError {
    /// Wrap any backend error into an [`RetrievalError::Index`].
    pub fn index<E: std::fmt::Display>(err: E) -> Self {
        RetrievalError::Index(err.to_string())
    }
}

/// A search backend a lane can drive: BM25 over the local index, optionally scoped to one
/// source. Synchronous because the indexer is synchronous.
pub trait LaneSearcher: Send + Sync {
    /// Return up to `limit` hits for `query`. When `source_id` is `Some`, only documents
    /// owned by that source may be returned.
    fn search(
        &self,
        query: &str,
        limit: usize,
        source_id: Option<&str>,
    ) -> Result<Vec<Candidate>, RetrievalError>;
}

/// Blanket forwarding so a `Box<dyn LaneSearcher>` / `Arc<dyn LaneSearcher>` /
/// `Arc<MemoryLaneSearcher>` can be used anywhere a `&dyn LaneSearcher` is expected by
/// simply passing the handle (it derefs through this impl).
impl<T: LaneSearcher + ?Sized> LaneSearcher for Box<T> {
    fn search(
        &self,
        query: &str,
        limit: usize,
        source_id: Option<&str>,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        (**self).search(query, limit, source_id)
    }
}

impl<T: LaneSearcher + ?Sized> LaneSearcher for Arc<T> {
    fn search(
        &self,
        query: &str,
        limit: usize,
        source_id: Option<&str>,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        (**self).search(query, limit, source_id)
    }
}

/// Deduplicate candidates by `doc_id`.
///
/// For each `doc_id` the **maximum** score wins, while the **first-seen** lane / source_id /
/// title / url are preserved (a later, higher-scoring duplicate only raises the score — it
/// never overwrites the provenance of where the document first appeared). The result is
/// ordered by score descending, ties broken by first-seen order (the sort is stable).
pub fn merge_dedup(cands: Vec<Candidate>) -> Vec<Candidate> {
    let mut order: Vec<String> = Vec::new();
    let mut best: HashMap<String, Candidate> = HashMap::new();

    for cand in cands {
        match best.get_mut(&cand.doc_id) {
            Some(existing) => {
                if cand.score > existing.score {
                    existing.score = cand.score;
                }
            }
            None => {
                order.push(cand.doc_id.clone());
                best.insert(cand.doc_id.clone(), cand);
            }
        }
    }

    let mut out: Vec<Candidate> = order
        .into_iter()
        .filter_map(|id| best.remove(&id))
        .collect();
    // `sort_by` is stable, so equal scores keep their first-seen order.
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then(Ordering::Equal));
    out
}

/// Source-first retrieval lane.
///
/// Given the sources that matched the query, it runs a source-scoped search for each one,
/// runs a global BM25 search, tags the two groups, and merges them with [`merge_dedup`].
/// Output is deterministic: sources are visited in the order given, then the global search.
#[derive(Debug, Clone)]
pub struct SourceFirst {
    source_ids: Vec<String>,
    /// Final result cap. `0` means "derive from the request limit" (`limit * 3`).
    max_candidates: usize,
}

impl SourceFirst {
    /// A lane over the given source ids (in priority order).
    pub fn new(source_ids: Vec<String>) -> Self {
        Self {
            source_ids,
            max_candidates: 0,
        }
    }

    /// Builder: an explicit final cap; `0` restores the `limit * 3` default.
    pub fn with_max_candidates(mut self, cap: usize) -> Self {
        self.max_candidates = cap;
        self
    }

    /// The source ids this lane will scope to.
    pub fn source_ids(&self) -> &[String] {
        &self.source_ids
    }

    /// Retrieve for `query`.
    ///
    /// Per-source limit is `limit / len(source_ids)`, at least 1. The global BM25 search
    /// uses `limit`. An empty source list degrades to global-only.
    pub fn retrieve(
        &self,
        searcher: &dyn LaneSearcher,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        if limit == 0 {
            return Err(RetrievalError::Invalid("limit must be >= 1".to_string()));
        }

        let mut raw: Vec<Candidate> = Vec::new();

        if !self.source_ids.is_empty() {
            let per_source = (limit / self.source_ids.len()).max(1);
            for source_id in &self.source_ids {
                let hits = searcher.search(query, per_source, Some(source_id))?;
                for mut hit in hits {
                    hit.lane = Lane::SourceFirst;
                    // Trust the backend's own source tag when it gives one; otherwise
                    // attribute the hit to the source we scoped to.
                    if hit.source_id.is_none() {
                        hit.source_id = Some(source_id.clone());
                    }
                    raw.push(hit);
                }
            }
        }

        for mut hit in searcher.search(query, limit, None)? {
            hit.lane = Lane::Bm25;
            raw.push(hit);
        }

        let merged = merge_dedup(raw);
        let cap = self.effective_cap(limit);
        Ok(merged.into_iter().take(cap).collect())
    }

    fn effective_cap(&self, limit: usize) -> usize {
        if self.max_candidates > 0 {
            self.max_candidates
        } else {
            limit.saturating_mul(3)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(doc_id: &str, score: f32, lane: Lane, source: Option<&str>) -> Candidate {
        Candidate {
            doc_id: doc_id.to_string(),
            score,
            lane,
            source_id: source.map(str::to_string),
            title: Some(format!("title-{doc_id}")),
            url: Some(format!("https://example.com/{doc_id}")),
        }
    }

    #[test]
    fn merge_dedup_keeps_max_score_and_drops_duplicates() {
        let merged = merge_dedup(vec![
            cand("a", 1.0, Lane::Bm25, None),
            cand("b", 5.0, Lane::Bm25, None),
            cand("a", 3.5, Lane::SourceFirst, Some("s1")),
        ]);
        assert_eq!(merged.len(), 2, "doc a duplicated twice must collapse to one");
        let a = merged.iter().find(|c| c.doc_id == "a").unwrap();
        assert_eq!(a.score, 3.5, "max score across duplicates wins");
        // First-seen provenance is preserved even though the higher score came later.
        assert_eq!(a.lane, Lane::Bm25);
        assert_eq!(a.source_id, None);
    }

    #[test]
    fn merge_dedup_orders_by_score_desc_then_first_seen() {
        let merged = merge_dedup(vec![
            cand("x", 2.0, Lane::Bm25, None),
            cand("y", 2.0, Lane::Bm25, None),
            cand("z", 9.0, Lane::Bm25, None),
        ]);
        let ids: Vec<&str> = merged.iter().map(|c| c.doc_id.as_str()).collect();
        // z has the top score; x and y tie at 2.0 and keep first-seen order.
        assert_eq!(ids, vec!["z", "x", "y"]);
    }

    #[test]
    fn merge_dedup_preserves_first_seen_source_metadata() {
        let merged = merge_dedup(vec![
            cand("d", 1.0, Lane::SourceFirst, Some("first")),
            cand("d", 9.0, Lane::SourceFirst, Some("later")),
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].source_id.as_deref(), Some("first"));
        assert_eq!(merged[0].score, 9.0);
    }

    // A stub searcher that records the calls it receives and returns a scripted set.
    struct StubSearcher {
        calls: std::sync::Mutex<Vec<(String, usize, Option<String>)>>,
        respond: Box<dyn Fn(&str, usize, Option<&str>) -> Vec<Candidate> + Send + Sync>,
    }

    impl StubSearcher {
        fn new(
            respond: impl Fn(&str, usize, Option<&str>) -> Vec<Candidate> + Send + Sync + 'static,
        ) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                respond: Box::new(respond),
            }
        }
        fn calls(&self) -> Vec<(String, usize, Option<String>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl LaneSearcher for StubSearcher {
        fn search(
            &self,
            query: &str,
            limit: usize,
            source_id: Option<&str>,
        ) -> Result<Vec<Candidate>, RetrievalError> {
            self.calls
                .lock()
                .unwrap()
                .push((query.to_string(), limit, source_id.map(str::to_string)));
            Ok((self.respond)(query, limit, source_id))
        }
    }

    #[test]
    fn source_first_scopes_per_source_and_merges_global() {
        // s1 and s2 each return one doc; global returns a third plus a duplicate of s1's.
        let searcher = StubSearcher::new(|_q, _limit, source_id| match source_id {
            Some("s1") => vec![cand("doc1", 2.0, Lane::Bm25, Some("s1"))],
            Some("s2") => vec![cand("doc2", 4.0, Lane::Bm25, Some("s2"))],
            None => vec![
                cand("doc1", 1.0, Lane::Bm25, Some("s1")),
                cand("doc3", 3.0, Lane::Bm25, Some("s3")),
            ],
            _ => vec![],
        });
        let lane = SourceFirst::new(vec!["s1".to_string(), "s2".to_string()]);
        let out = lane.retrieve(&searcher, "llm", 10).unwrap();

        // Calls: scoped s1, scoped s2, then global. Per-source limit = 10/2 = 5.
        let calls = searcher.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0], ("llm".to_string(), 5, Some("s1".to_string())));
        assert_eq!(calls[1], ("llm".to_string(), 5, Some("s2".to_string())));
        assert_eq!(calls[2], ("llm".to_string(), 10, None));

        // doc1 deduped (kept max score 2.0, first-seen SourceFirst lane).
        let ids: Vec<&str> = out.iter().map(|c| c.doc_id.as_str()).collect();
        assert_eq!(ids, vec!["doc2", "doc3", "doc1"]);
        let doc1 = out.iter().find(|c| c.doc_id == "doc1").unwrap();
        assert_eq!(doc1.score, 2.0);
        assert_eq!(doc1.lane, Lane::SourceFirst);
        let doc2 = out.iter().find(|c| c.doc_id == "doc2").unwrap();
        assert_eq!(doc2.lane, Lane::SourceFirst);
        let doc3 = out.iter().find(|c| c.doc_id == "doc3").unwrap();
        assert_eq!(doc3.lane, Lane::Bm25);
    }

    #[test]
    fn source_first_empty_sources_is_global_only() {
        let searcher = StubSearcher::new(|_q, _limit, source_id| match source_id {
            None => vec![cand("g1", 1.0, Lane::Bm25, None)],
            _ => panic!("no source-scoped search expected for an empty source list"),
        });
        let lane = SourceFirst::new(vec![]);
        let out = lane.retrieve(&searcher, "q", 5).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].doc_id, "g1");
        assert_eq!(out[0].lane, Lane::Bm25);
        assert_eq!(searcher.calls().len(), 1, "only the global search should run");
    }

    #[test]
    fn source_first_caps_results() {
        // Backend returns 4 fresh docs each call; cap must bound the merged output.
        let searcher = StubSearcher::new(|_q, _limit, source_id| {
            let base = source_id.unwrap_or("g");
            (0..4)
                .map(|i| {
                    cand(
                        &format!("{base}-{i}"),
                        10.0 - i as f32,
                        Lane::Bm25,
                        source_id,
                    )
                })
                .collect()
        });
        let lane = SourceFirst::new(vec!["s1".to_string()]).with_max_candidates(2);
        let out = lane.retrieve(&searcher, "q", 10).unwrap();
        assert_eq!(out.len(), 2, "explicit cap is honored");
    }

    #[test]
    fn source_first_default_cap_is_three_times_limit() {
        let searcher = StubSearcher::new(|_q, _limit, source_id| {
            let base = source_id.unwrap_or("g");
            (0..50)
                .map(|i| cand(&format!("{base}-{i}"), 1.0, Lane::Bm25, source_id))
                .collect()
        });
        let lane = SourceFirst::new(vec!["s1".to_string()]);
        let out = lane.retrieve(&searcher, "q", 4).unwrap();
        assert_eq!(out.len(), 12, "cap = limit * 3");
    }

    #[test]
    fn source_first_rejects_zero_limit() {
        let searcher = StubSearcher::new(|_q, _limit, _s| vec![]);
        let lane = SourceFirst::new(vec!["s1".to_string()]);
        assert!(matches!(
            lane.retrieve(&searcher, "q", 0),
            Err(RetrievalError::Invalid(_))
        ));
    }

    #[test]
    fn memory_searcher_ranks_by_bm25() {
        let mut s = MemoryLaneSearcher::new();
        s.add_doc(
            MemoryDoc::new("hot", "Rust async")
                .with_body("rust rust rust async")
                .with_source("s1"),
        );
        s.add_doc(
            MemoryDoc::new("cold", "Rust basics")
                .with_body("rust")
                .with_source("s1"),
        );
        s.add_doc(
            MemoryDoc::new("other", "Python")
                .with_body("python async")
                .with_source("s2"),
        );
        let out = s.search("rust", 10, None).unwrap();
        let ids: Vec<&str> = out.iter().map(|c| c.doc_id.as_str()).collect();
        assert_eq!(ids, vec!["hot", "cold"], "higher term frequency ranks first");
        assert!(out[0].score > out[1].score);
    }

    #[test]
    fn memory_searcher_scopes_by_source() {
        let mut s = MemoryLaneSearcher::new();
        s.add_doc(MemoryDoc::new("a", "t").with_body("rust").with_source("s1"));
        s.add_doc(MemoryDoc::new("b", "t").with_body("rust").with_source("s2"));
        let out = s.search("rust", 10, Some("s2")).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].doc_id, "b");
        assert_eq!(out[0].source_id.as_deref(), Some("s2"));
        // No match -> empty, not an error.
        assert!(s.search("nonexistent", 10, None).unwrap().is_empty());
    }

    #[test]
    fn memory_searcher_limits_hits() {
        let mut s = MemoryLaneSearcher::new();
        for i in 0..5 {
            s.add_doc(MemoryDoc::new(format!("d{i}"), "t").with_body("rust"));
        }
        assert_eq!(s.search("rust", 2, None).unwrap().len(), 2);
    }

    #[test]
    fn memory_searcher_drives_source_first_lane() {
        let mut s = MemoryLaneSearcher::new();
        s.add_doc(MemoryDoc::new("s1-a", "t").with_body("llm agents").with_source("s1"));
        s.add_doc(MemoryDoc::new("s2-b", "t").with_body("llm agents").with_source("s2"));
        s.add_doc(MemoryDoc::new("g-c", "t").with_body("llm agents").with_source("s3"));
        let lane = SourceFirst::new(vec!["s1".to_string()]);
        let out = lane.retrieve(&s, "llm", 10).unwrap();
        let ids: Vec<&str> = out.iter().map(|c| c.doc_id.as_str()).collect();
        assert!(ids.contains(&"s1-a"));
        assert!(ids.contains(&"g-c"));
        let s1a = out.iter().find(|c| c.doc_id == "s1-a").unwrap();
        assert_eq!(s1a.lane, Lane::SourceFirst);
    }
}
