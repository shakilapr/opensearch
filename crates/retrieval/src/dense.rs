//! The dense lane: embed the query, rank stored document vectors by cosine.
//!
//! This is what turns a vector sidecar into retrieval. It implements the same
//! [`LaneSearcher`] contract as the lexical and source-first lanes, so the engine fuses it with
//! the others by rank (RRF) without knowing it is dense — no engine change is needed to add it,
//! which is the point of the lane interface.
//!
//! Two honest limits, both visible in the type:
//!
//! * **Brute force.** Every stored vector is scored per query. At 12K documents × 384 dimensions
//!   that is ~5M multiply-adds, which is why there is no ANN structure yet; `crates/ann` exists
//!   for when the corpus makes this the bottleneck.
//! * **The embedder must match the vectors.** A store built with BGE and queried with the hash
//!   embedder returns noise, so [`DenseLane::from_path`] takes the embedder and callers are
//!   expected to pair them (the backfill records which one it used in its log line).

use std::path::Path;
use std::sync::Arc;

use search_embed::{Embedder, VectorStore};

use crate::{Candidate, Lane, LaneSearcher, RetrievalError};

/// A lane over a document-vector store.
pub struct DenseLane {
    store: Arc<VectorStore>,
    embedder: Arc<dyn Embedder>,
}

/// A `Debug` impl is needed because callers hold `Result<Option<DenseLane>, _>`; the embedder
/// trait object is not `Debug`, so the lane reports its shape instead of its contents.
impl std::fmt::Debug for DenseLane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DenseLane")
            .field("docs", &self.store.len())
            .field("dim", &self.store.dim())
            .finish()
    }
}

impl DenseLane {
    /// Build a lane over an already-loaded store.
    ///
    /// The store's dimension must match the embedder's; when it does not, `search` returns no
    /// hits rather than mis-ranking, and [`DenseLane::from_path`] rejects the pair outright.
    pub fn new(store: Arc<VectorStore>, embedder: Arc<dyn Embedder>) -> Self {
        Self { store, embedder }
    }

    /// Load the sidecar at `path` and pair it with `embedder`.
    ///
    /// Returns `Ok(None)` when the file does not exist — an un-backfilled corpus is a normal
    /// state, not an error — and an error only when the file exists and cannot be read, or when
    /// its dimension does not match the embedder (which would otherwise rank noise silently).
    pub fn from_path(
        path: &Path,
        embedder: Arc<dyn Embedder>,
    ) -> Result<Option<Self>, RetrievalError> {
        if !path.exists() {
            return Ok(None);
        }
        let store = VectorStore::load(path).map_err(|e| RetrievalError::index(e.to_string()))?;
        if store.dim() != embedder.dim() {
            return Err(RetrievalError::Invalid(format!(
                "vector store {} is {}-dimensional but the embedder produces {}: rebuild the store \
                 with the same embedder (`agent0 embed build`)",
                path.display(),
                store.dim(),
                embedder.dim()
            )));
        }
        Ok(Some(Self::new(Arc::new(store), embedder)))
    }

    /// Documents in the store.
    pub fn len(&self) -> usize {
        self.store.len()
    }

    /// Whether the lane has no vectors to search.
    pub fn is_empty(&self) -> bool {
        self.store.is_empty()
    }
}

impl LaneSearcher for DenseLane {
    fn search(
        &self,
        query: &str,
        limit: usize,
        source_id: Option<&str>,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        if limit == 0 || self.store.is_empty() {
            return Ok(Vec::new());
        }
        let embedded = self.embedder.embed(query);
        // An empty or punctuation-only query embeds to the zero vector; every similarity is then
        // 0.0 and the lane would inject arbitrary documents into the fusion. Return nothing.
        if embedded.v.iter().all(|x| *x == 0.0) {
            return Ok(Vec::new());
        }
        Ok(self
            .store
            .top_k_scoped(&embedded.v, limit, source_id)
            .into_iter()
            .map(|(doc_id, score)| Candidate {
                doc_id,
                score,
                lane: Lane::Dense,
                source_id: None,
                title: None,
                url: None,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use search_embed::HashEmbedder;

    fn store_with(dim: usize, docs: &[(&str, Option<&str>, &str)]) -> Arc<VectorStore> {
        let emb = HashEmbedder::new(dim);
        let mut store = VectorStore::new(dim);
        for (id, source, text) in docs {
            store
                .insert(*id, source.map(|s| s.to_string()), emb.embed(text).v)
                .unwrap();
        }
        store.sort();
        Arc::new(store)
    }

    #[test]
    fn the_lane_ranks_the_matching_document_first() {
        let store = store_with(
            256,
            &[
                ("d1", None, "paged attention reduces KV cache memory in vLLM"),
                ("d2", None, "sourdough bread flour water salt starter"),
            ],
        );
        let lane = DenseLane::new(store, Arc::new(HashEmbedder::new(256)));
        let hits = lane.search("paged attention KV cache", 5, None).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].doc_id, "d1", "{hits:?}");
        assert_eq!(hits[0].lane, Lane::Dense);
        assert!(hits[0].score >= hits[1].score);
    }

    #[test]
    fn a_source_scoped_call_never_returns_another_source() {
        let store = store_with(
            256,
            &[
                ("a", Some("source-a"), "paged attention"),
                ("b", Some("source-b"), "paged attention"),
            ],
        );
        let lane = DenseLane::new(store, Arc::new(HashEmbedder::new(256)));
        let hits = lane.search("paged attention", 10, Some("source-b")).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].doc_id, "b");
    }

    #[test]
    fn an_empty_query_yields_no_hits_rather_than_noise() {
        let store = store_with(256, &[("d1", None, "paged attention")]);
        let lane = DenseLane::new(store, Arc::new(HashEmbedder::new(256)));
        assert!(lane.search("   ", 5, None).unwrap().is_empty());
        assert!(lane.search("", 5, None).unwrap().is_empty());
        // A zero limit is honoured too.
        assert!(lane.search("paged attention", 0, None).unwrap().is_empty());
    }

    #[test]
    fn a_missing_store_is_not_an_error_and_a_mismatched_one_is() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope.bin");
        assert!(DenseLane::from_path(&missing, Arc::new(HashEmbedder::new(256)))
            .unwrap()
            .is_none());

        // Written at 128 dimensions, opened with a 256-dimension embedder: refuse, because the
        // alternative is silently ranking noise.
        let store = store_with(128, &[("d1", None, "paged attention")]);
        let path = tmp.path().join("v.bin");
        store.save(&path).unwrap();
        let err = DenseLane::from_path(&path, Arc::new(HashEmbedder::new(256)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("dimensional"), "{err}");

        // The matching embedder opens it.
        assert!(DenseLane::from_path(&path, Arc::new(HashEmbedder::new(128)))
            .unwrap()
            .is_some());
    }
}
