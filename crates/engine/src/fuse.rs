//! §10 Candidate fusion and the ranking cascade. OWNER: engine-rank agent.

use crate::LaneId;
use std::collections::{BTreeMap, HashMap};

/// One lane's ranked document ids, best first (rank 1 = index 0).
#[derive(Debug, Clone, PartialEq)]
pub struct LaneHits {
    pub lane: LaneId,
    pub ranked: Vec<String>,
}

/// Fusion knobs (§10). `c` starts near 60 as a tuning candidate, not a calibrated confidence.
#[derive(Debug, Clone, PartialEq)]
pub struct FusionPolicy {
    pub c: f64,
    pub weights: HashMap<LaneId, f64>,
    /// Cap on the combined weight of one variant family (near-identical rewrites).
    pub variant_family_cap: f64,
}

impl Default for FusionPolicy {
    fn default() -> Self {
        let mut weights = HashMap::new();
        weights.insert(LaneId::Bm25, 1.0);
        weights.insert(LaneId::SourceFirst, 0.8);
        Self { c: 60.0, weights, variant_family_cap: 2.0 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fused {
    pub doc_id: String,
    pub score: f64,
}

/// Weighted reciprocal rank fusion (§10): `score(d) = sum_l w_l / (c + rank_l(d))`.
/// Ranks begin at 1; a lane missing `d` contributes zero. Result is score-descending,
/// ties broken by doc_id so the order is deterministic.
pub fn fuse(lanes: &[LaneHits], policy: &FusionPolicy) -> Vec<Fused> {
    let mut acc: BTreeMap<String, f64> = BTreeMap::new();
    for lh in lanes {
        let w = policy.weights.get(&lh.lane).copied().unwrap_or(0.0);
        if w == 0.0 {
            continue;
        }
        for (i, doc) in lh.ranked.iter().enumerate() {
            let rank = (i + 1) as f64;
            *acc.entry(doc.clone()).or_insert(0.0) += w / (policy.c + rank);
        }
    }
    let mut out: Vec<Fused> = acc
        .into_iter()
        .map(|(doc_id, score)| Fused { doc_id, score })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.doc_id.cmp(&b.doc_id))
    });
    out
}

/// Inputs to the §10 cascade. Each `*_cap` is the stage's document budget.
#[derive(Debug, Clone, PartialEq)]
pub struct CascadeInput {
    pub fused: Vec<Fused>,
    pub feature_cap: usize,
    pub cross_encoder_cap: usize,
    pub verify_cap: usize,
    pub final_k: usize,
}

/// Surviving order per stage, plus the number of documents each stage admitted.
#[derive(Debug, Clone, PartialEq)]
pub struct CascadeOutput {
    pub ranked: Vec<Fused>,
    pub stage_sizes: Vec<usize>,
}

/// Run the cascade: fusion survivors -> feature ranker cap -> reranker cap -> verification cap.
/// Stage sizes are recorded so the trace can show where documents were dropped (§10: a
/// reranker cannot recover documents that never reached its candidate set).
pub fn run_cascade(input: CascadeInput) -> CascadeOutput {
    let n = input.fused.len();
    let after_feature = n.min(input.feature_cap);
    let after_ce = after_feature.min(input.cross_encoder_cap);
    let after_verify = after_ce.min(input.verify_cap);
    let final_n = after_verify.min(input.final_k);
    let ranked = input.fused.into_iter().take(final_n).collect();
    CascadeOutput {
        ranked,
        stage_sizes: vec![n, after_feature, after_ce, after_verify, final_n],
    }
}
