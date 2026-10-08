//! Linear feature ranker + the rerank entry point (engine §22 "Ranking").
//!
//! Retrieval lanes produce candidates with a feature vector; a manually-tuned linear blend
//! scores them until a learning-to-rank model can replace it.

/// Per-candidate features handed to [`FeatureRanker`]. All expected in `[0, 1]` (penalties are
/// folded in upstream), though the ranker does not clamp — garbage in, garbage out.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Features {
    pub bm25: f32,
    pub dense: f32,
    pub source_authority: f32,
    pub freshness: f32,
    pub entity_match: f32,
    pub graph_hop: f32,
    pub title_match: f32,
}

impl Features {
    /// Feature vector in the canonical weight order used by [`FeatureRanker::weights`].
    pub fn to_vec(&self) -> [f32; 7] {
        [
            self.bm25,
            self.dense,
            self.source_authority,
            self.freshness,
            self.entity_match,
            self.graph_hop,
            self.title_match,
        ]
    }
}

/// A minimal rerank input. `libraries/retrieval` exposes `Candidate`, but it is being built
/// concurrently; `RerankInput` is the frozen Stage-2 shape (`doc_id` + `Features`) and lets a
/// `Candidate` be adapted into it without coupling the two crates' timelines.
#[derive(Debug, Clone, PartialEq)]
pub struct RerankInput {
    pub doc_id: String,
    pub features: Features,
}

impl RerankInput {
    pub fn new(doc_id: impl Into<String>, features: Features) -> Self {
        Self {
            doc_id: doc_id.into(),
            features,
        }
    }
}

/// Linear ranker: `score = dot(weights, feature_vector)`.
///
/// `weights` is indexed in the order **bm25, dense, source_authority, freshness,
/// entity_match, graph_hop, title_match**.
pub struct FeatureRanker {
    pub weights: Vec<f32>,
}

/// Weight order, exposed as a named constant so callers and docs cannot drift apart.
pub const FEATURE_ORDER: [&str; 7] = [
    "bm25",
    "dense",
    "source_authority",
    "freshness",
    "entity_match",
    "graph_hop",
    "title_match",
];

/// Default weights for LLM / tech search.
///
/// Rationale: lexical BM25 is the precision backbone (1.0); a dense lane catches paraphrase the
/// lexical lane misses but is noisier with the hashed default embedder (0.8). Entity match is a
/// strong signal for ambiguous LLM questions (0.6), as is an exact title hit for "what is X"
/// queries (0.4). Source authority is a mild tie-breaker (0.5), graph-hop expansion is the
/// weakest verified signal (0.2), and freshness matters only for time-sensitive classes (0.3).
pub const DEFAULT_LLM_WEIGHTS: [f32; 7] = [1.0, 0.8, 0.5, 0.3, 0.6, 0.2, 0.4];

impl Default for FeatureRanker {
    fn default() -> Self {
        Self::default_llm()
    }
}

impl FeatureRanker {
    pub fn default_llm() -> Self {
        Self {
            weights: DEFAULT_LLM_WEIGHTS.to_vec(),
        }
    }

    pub fn new(weights: Vec<f32>) -> Self {
        Self { weights }
    }

    /// Dot product of weights and the feature vector. If `weights` is shorter than 7 the
    /// trailing features are ignored; longer weights beyond the 7 known features are ignored.
    pub fn score(&self, f: &Features) -> f32 {
        let v = f.to_vec();
        self.weights
            .iter()
            .zip(v.iter())
            .map(|(w, x)| w * x)
            .sum()
    }
}

/// Score every candidate, sort by score descending (ties broken by `doc_id` ascending for
/// determinism), and truncate to `limit`.
pub fn rerank(cands: &[RerankInput], ranker: &FeatureRanker, limit: usize) -> Vec<(String, f32)> {
    let mut scored: Vec<(String, f32)> = cands
        .iter()
        .map(|c| (c.doc_id.clone(), ranker.score(&c.features)))
        .collect();

    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    scored.truncate(limit);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(bm25: f32, dense: f32, sa: f32, fr: f32, em: f32, gh: f32, tm: f32) -> Features {
        Features {
            bm25,
            dense,
            source_authority: sa,
            freshness: fr,
            entity_match: em,
            graph_hop: gh,
            title_match: tm,
        }
    }

    #[test]
    fn default_weights_are_documented_values() {
        let r = FeatureRanker::default_llm();
        assert_eq!(r.weights, vec![1.0, 0.8, 0.5, 0.3, 0.6, 0.2, 0.4]);
    }

    #[test]
    fn score_is_dot_product_and_orders_by_weight() {
        let r = FeatureRanker::default_llm();
        let only_bm25 = f(1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        let only_graph = f(0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0);
        assert!((r.score(&only_bm25) - 1.0).abs() < 1e-6);
        assert!((r.score(&only_graph) - 0.2).abs() < 1e-6);
        assert!(r.score(&only_bm25) > r.score(&only_graph));
        // Mixed vector: 1.0*0.5 + 0.8*0.5 + 0.6*1.0 = 0.5+0.4+0.6 = 1.5
        let mixed = f(0.5, 0.5, 0.0, 0.0, 1.0, 0.0, 0.0);
        assert!((r.score(&mixed) - 1.5).abs() < 1e-6);
    }

    #[test]
    fn rerank_limit_and_tie_break_by_doc_id() {
        let r = FeatureRanker::default_llm();
        let mk = |id: &str, bm: f32| RerankInput::new(id, f(0.0, bm, 0.0, 0.0, 0.0, 0.0, 0.0));
        // "b" and "a" tie on the dense feature; doc_id ascending breaks the tie.
        let cands = vec![mk("z", 1.0), mk("b", 0.5), mk("a", 0.5)];
        let out = rerank(&cands, &r, 2);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, "z");
        assert_eq!(out[1].0, "a"); // tie -> "a" before "b"
    }
}
