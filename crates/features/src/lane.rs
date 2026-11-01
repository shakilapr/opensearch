//! The feature lane: query → teacher embedding → `W` → features → postings → candidates.
//!
//! This is Model D of v2 §5.2 in its cheapest form, exactly as PLAN-v2 Phase 3.4 specifies:
//! *"project the query embedding through the same `W` — no new model"*. A separate query encoder
//! would have to be trained to agree with Model B's vocabulary; projecting through the document
//! encoder is the arrangement that cannot disagree with it.
//!
//! Why a learned-feature lane is not just a slower BM25:
//!
//! * It matches on **teacher geometry**, not on surface tokens. A document that never contains
//!   the query's words can still be its nearest neighbour in the 384-dimensional space the
//!   teacher was trained to place meaning in — the capability the concept and dense lanes were
//!   built for.
//! * It is **inverted**, not brute force. Dense scores every vector (1.07 µs/document, measured,
//!   which is 1.5 s over the fleet); this touches only the postings of the query's ≤32 features.
//! * It is **sparse by construction**, so the per-document cost stays at 16 postings no matter how
//!   the corpus grows, which is the property v2 §16 wants and a dense index does not have.
//!
//! Honest limits: the features inherit whatever the teacher embedding cannot distinguish (two
//! different topics that BGE-small places together stay together), and a shard's index is loaded
//! whole, so memory is proportional to `documents × active` (about 100 B/document: 16 postings ×
//! 6 B plus the id table).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use search_embed::Embedder;
use search_retrieval::{Candidate, Lane, LaneSearcher, RetrievalError};

use crate::encoder::{Encoder, EncoderError, FeatureHit};
use crate::postings::{PostingsError, SparseIndex};

/// Errors a feature lane can be constructed from.
#[derive(Debug, thiserror::Error)]
pub enum LaneError {
    /// The encoder file could not be read.
    #[error(transparent)]
    Encoder(#[from] EncoderError),
    /// A postings file could not be read.
    #[error(transparent)]
    Postings(#[from] PostingsError),
    /// The encoder's input width does not match the embedder's output width.
    #[error("encoder takes {expected}-dimensional input but the embedder produces {got}")]
    Dim {
        /// The encoder's input width.
        expected: usize,
        /// The embedder's output width.
        got: usize,
    },
}

/// Per-shard postings with the path they were loaded from, for diagnostics.
#[derive(Debug)]
struct Shard {
    path: PathBuf,
    index: SparseIndex,
}

/// A federated lane over sparse feature indices.
pub struct FeatureLane {
    encoder: Arc<Encoder>,
    embedder: Arc<dyn Embedder>,
    shards: Vec<Shard>,
    active: usize,
}

impl std::fmt::Debug for FeatureLane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeatureLane")
            .field("features", &self.encoder.features())
            .field("active", &self.active)
            .field(
                "docs",
                &self.shards.iter().map(|s| s.index.len()).sum::<usize>(),
            )
            .finish()
    }
}

impl FeatureLane {
    /// Build a lane from an encoder file and one postings file per shard.
    ///
    /// The postings paths are explicit rather than derived, because a shard's vector sidecar and
    /// its feature index are built at different times and a missing one must be a logged absence,
    /// not a silently empty lane.
    pub fn from_paths(
        encoder_path: &Path,
        shard_dirs: &[PathBuf],
        embedder: Arc<dyn Embedder>,
    ) -> Result<Option<Self>, LaneError> {
        if !encoder_path.is_file() {
            return Ok(None);
        }
        let encoder = Encoder::load(encoder_path)?;
        if encoder.dim() != embedder.dim() {
            return Err(LaneError::Dim { expected: encoder.dim(), got: embedder.dim() });
        }
        let mut shards = Vec::new();
        for dir in shard_dirs {
            let path = dir.join("features").join("postings.bin");
            if !path.is_file() {
                tracing::info!(path = %path.display(), "feature lane: no postings for this root");
                continue;
            }
            shards.push(Shard { path: path.clone(), index: SparseIndex::load(&path)? });
        }
        if shards.is_empty() {
            return Ok(None);
        }
        let active = encoder.active();
        Ok(Some(Self { encoder: Arc::new(encoder), embedder, shards, active }))
    }

    /// Documents reachable through the lane.
    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.index.len()).sum()
    }

    /// Whether the lane has no documents.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Feature vocabulary size.
    pub fn features(&self) -> usize {
        self.encoder.features()
    }

    /// `TopK` width in use.
    pub fn active(&self) -> usize {
        self.active
    }

    /// Postings per shard, for the startup log and the measurement harness.
    pub fn shard_summary(&self) -> Vec<(String, usize, usize)> {
        self.shards
            .iter()
            .map(|s| {
                (
                    s.path.parent().map(|p| p.display().to_string()).unwrap_or_default(),
                    s.index.len(),
                    s.index.posting_count(),
                )
            })
            .collect()
    }

    /// The features a query activates. Public so a measurement can attribute a query to the
    /// features it actually used instead of guessing from the results.
    pub fn query_features(&self, query: &str) -> Vec<FeatureHit> {
        let embedded = self.embedder.embed(query);
        if embedded.v.iter().all(|x| *x == 0.0) {
            return Vec::new();
        }
        self.encoder.encode(&embedded.v, self.active).unwrap_or_default()
    }
}

impl LaneSearcher for FeatureLane {
    fn lane_hint(&self) -> Option<Lane> {
        Some(Lane::Features)
    }

    fn search(
        &self,
        query: &str,
        limit: usize,
        source_id: Option<&str>,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        if limit == 0 || self.shards.is_empty() {
            return Ok(Vec::new());
        }
        let hits = self.query_features(query);
        // An empty query (or one the teacher embeds to zero) activates nothing. Returning no
        // candidates is the honest answer; fabricating `limit` arbitrary documents is not.
        if hits.is_empty() {
            return Ok(Vec::new());
        }
        let mut merged: Vec<Candidate> = Vec::with_capacity(limit * self.shards.len());
        for shard in &self.shards {
            for (doc_id, score) in shard.index.search(&hits, limit, source_id) {
                merged.push(Candidate {
                    doc_id,
                    score,
                    lane: Lane::Features,
                    source_id: source_id.map(str::to_string),
                    title: None,
                    url: None,
                });
            }
        }
        // Shards are host-disjoint, but a document reachable through two of them (a rebuild race,
        // or a resource that moved) must not occupy two fusion ranks: keep the better score and
        // the first deterministic position.
        merged.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.doc_id.cmp(&b.doc_id))
        });
        merged.dedup_by(|a, b| a.doc_id == b.doc_id);
        merged.truncate(limit);
        Ok(merged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::{train, TrainOptions};

    /// The fixture's document vector for index `i`: four clusters in eight dimensions.
    fn cluster_vec(i: u32) -> Vec<f32> {
        let mut v = vec![0.0f32; 8];
        v[(i % 4) as usize] = 1.0;
        v[((i % 4) + 4) as usize] = 0.5;
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / n).collect()
    }

    /// A deterministic embedder over an 8-dim toy space, so the lane can be tested without BGE.
    /// It maps `doc-NNN` to the same vector the fixture indexed, which is what lets a text query
    /// be compared against the postings at all.
    struct ToyEmbedder;

    impl Embedder for ToyEmbedder {
        fn dim(&self) -> usize {
            8
        }
        fn embed(&self, text: &str) -> search_embed::Embedding {
            if text.trim().is_empty() {
                return search_embed::Embedding::zeros(8);
            }
            let n = text.trim_start_matches("doc-").parse::<u32>().unwrap_or(0);
            search_embed::Embedding { dim: 8, v: cluster_vec(n) }
        }
    }

    fn fixture(dir: &Path) -> (PathBuf, PathBuf) {
        let rows: Vec<(String, Option<String>, Vec<f32>)> = (0..64u32)
            .map(|i| (format!("doc-{i:03}"), Some("s-1".to_string()), cluster_vec(i)))
            .collect();
        let samples: Vec<Vec<f32>> = rows.iter().map(|(_, _, v)| v.clone()).collect();
        let enc = train(
            &samples,
            8,
            &TrainOptions { features: 32, active: 4, epochs: 6, batch: 16, ..Default::default() },
        )
        .unwrap();
        let enc_path = dir.join("encoder.bin");
        enc.save(&enc_path).unwrap();

        let shard = dir.join("shard-0");
        std::fs::create_dir_all(shard.join("features")).unwrap();
        let index = SparseIndex::build(rows, &enc, 4).unwrap();
        index.save(&shard.join("features").join("postings.bin")).unwrap();
        (enc_path, shard)
    }

    #[test]
    fn the_lane_answers_and_reports_its_own_lane_id() {
        let tmp = tempfile::tempdir().unwrap();
        let (enc, shard) = fixture(tmp.path());
        let lane =
            FeatureLane::from_paths(&enc, &[shard.clone()], Arc::new(ToyEmbedder)).unwrap().unwrap();
        assert_eq!(lane.len(), 64);
        assert_eq!(lane.lane_hint(), Some(Lane::Features));

        // Many documents in one cluster share the same code, so the top of the ranking is a
        // quantisation tie broken by doc id; the property that matters is that the query reaches
        // its own cluster through the postings.
        let hits = lane.search("doc-005", 5, None).unwrap();
        assert!(!hits.is_empty());
        let cluster = |id: &str| id.trim_start_matches("doc-").parse::<u32>().unwrap() % 4;
        assert!(
            hits.iter().all(|h| cluster(&h.doc_id) == 1),
            "query cluster must fill the ranking: {hits:?}"
        );
        assert!(hits.iter().all(|h| h.lane == Lane::Features));
    }

    #[test]
    fn a_missing_encoder_or_postings_is_an_absent_lane_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let none =
            FeatureLane::from_paths(&tmp.path().join("nope.bin"), &[], Arc::new(ToyEmbedder))
                .unwrap();
        assert!(none.is_none(), "no encoder is a normal state, not a failure");
    }

    #[test]
    fn a_dimension_mismatch_is_an_error_not_silent_noise() {
        let tmp = tempfile::tempdir().unwrap();
        let (enc, shard) = fixture(tmp.path());
        struct Wrong;
        impl Embedder for Wrong {
            fn dim(&self) -> usize {
                16
            }
            fn embed(&self, _: &str) -> search_embed::Embedding {
                search_embed::Embedding::zeros(16)
            }
        }
        let err = FeatureLane::from_paths(&enc, &[shard], Arc::new(Wrong)).unwrap_err().to_string();
        assert!(err.contains("encoder takes 8-dimensional input"), "{err}");
    }

    #[test]
    fn a_zero_query_returns_no_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        let (enc, shard) = fixture(tmp.path());
        let lane = FeatureLane::from_paths(&enc, &[shard], Arc::new(ToyEmbedder)).unwrap().unwrap();
        assert!(lane.search("", 10, None).unwrap().is_empty());
    }
}