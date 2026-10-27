//! §10 Candidate fusion and the ranking cascade. OWNER: engine-rank agent.
//!
//! Frozen contract: `docs/ENGINE_SPEC.md` §10 and `engine/engine-architecture.md` §10.
//!
//! Two entry points:
//! * [`fuse`] — weighted reciprocal rank fusion over one family of lanes (the primitive).
//! * [`fuse_families`] — the §10 rule that a *variant family* (near-identical query
//!   rewrites) cannot outvote the original query family.
//! * [`run_cascade`] — the named §10 cascade stages with per-stage caps and recorded sizes.

use crate::LaneId;
use std::cmp::Ordering;
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
    /// Defaults, with every weight overridable from the environment.
    ///
    /// The defaults are what the shipped engine has always used (BM25 1.0, source-first 0.8,
    /// dense and graph absent, i.e. weight 0). They are read from the environment because the
    /// weights are an *experiment*, not a constant: `SEARCH_FUSION_DENSE` and `SEARCH_FUSION_GRAPH`
    /// exist so the semantic lanes can be weighted and measured against the graded query set
    /// without a rebuild, and reverted by unsetting a variable. A malformed or negative value is
    /// ignored rather than fatal — a bad experiment flag must not change ranking silently.
    fn default() -> Self {
        let mut weights = HashMap::new();
        weights.insert(LaneId::Bm25, weight_from_env("SEARCH_FUSION_BM25", 1.0));
        weights.insert(
            LaneId::SourceFirst,
            weight_from_env("SEARCH_FUSION_SOURCE_FIRST", 0.8),
        );
        weights.insert(LaneId::Dense, weight_from_env("SEARCH_FUSION_DENSE", 0.0));
        weights.insert(LaneId::Graph, weight_from_env("SEARCH_FUSION_GRAPH", 0.0));
        Self {
            c: weight_from_env("SEARCH_FUSION_C", 60.0),
            weights,
            variant_family_cap: weight_from_env("SEARCH_FUSION_FAMILY_CAP", 2.0),
        }
    }
}

/// A non-negative `f64` from `name`, else `default`. Unset, unparseable and negative values all
/// fall back to the default.
fn weight_from_env(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(default)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fused {
    pub doc_id: String,
    pub score: f64,
}

/// Weighted reciprocal rank fusion (§10): `score(d) = sum_l w_l / (c + rank_l(d))`.
/// Ranks begin at 1; a lane missing `d` contributes zero, and so does a lane whose weight is
/// absent from `policy.weights` (an unknown lane has weight 0). The result is
/// score-descending with `doc_id` ascending as the deterministic tie-break.
///
/// This is the single-family primitive: it fuses the lanes it is given. The variant-family
/// cap lives in [`fuse_families`].
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
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.doc_id.cmp(&b.doc_id))
    });
    out
}

/// Fuse several *variant families* and merge them into one ranking, enforcing the §10 rule:
/// "Prevent numerous near-identical query rewrites from outvoting the original query: fuse
/// within a variant family first or cap that family's total weight."
///
/// Each `families` entry is a `(family_id, lanes)` pair; the lanes of one family are fused
/// with [`fuse`] first. The family with the **smallest id** is treated as the original query
/// family (by convention `family == 0` is the original, consistent with
/// `plan::QueryVariant.family`). Every non-original family's total score mass is capped at
/// `policy.variant_family_cap` times the original family's total mass, scaling the whole
/// family proportionally when it exceeds the cap. The capped families are then merged and
/// sorted exactly like [`fuse`] (score-descending, `doc_id` ascending).
///
/// The cap is a *ratio* against the original family; to guarantee that no rewrite family can
/// outvote the original, a coordinator sets `variant_family_cap <= 1.0`. The scaffold default
/// of `2.0` merely bounds a runaway family to twice the original's mass. If the original
/// family is empty or has non-positive mass the cap is not applied (a rewrite is not zeroed
/// out just because the original returned nothing).
pub fn fuse_families(families: &[(u8, &[LaneHits])], policy: &FusionPolicy) -> Vec<Fused> {
    if families.is_empty() {
        return Vec::new();
    }

    let fused: Vec<(u8, Vec<Fused>)> = families
        .iter()
        .map(|(id, lanes)| (*id, fuse(lanes, policy)))
        .collect();

    // The original family is the one with the smallest id.
    let mut original_idx: usize = 0;
    for (i, (id, _)) in fused.iter().enumerate() {
        if *id < fused[original_idx].0 {
            original_idx = i;
        }
    }
    let original_total: f64 = fused[original_idx].1.iter().map(|f| f.score).sum();

    let mut merged: BTreeMap<String, f64> = BTreeMap::new();
    for (i, (_, docs)) in fused.iter().enumerate() {
        let family_total: f64 = docs.iter().map(|f| f.score).sum();
        let scale = if i == original_idx || original_total <= 0.0 {
            1.0
        } else if family_total > policy.variant_family_cap * original_total {
            (policy.variant_family_cap * original_total) / family_total
        } else {
            1.0
        };
        for f in docs {
            *merged.entry(f.doc_id.clone()).or_insert(0.0) += f.score * scale;
        }
    }

    let mut out: Vec<Fused> = merged
        .into_iter()
        .map(|(doc_id, score)| Fused { doc_id, score })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
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

/// Stage 1 of the cascade (§10 "Feature ranker", "Up to 300"): fast relevance,
/// source/type/time and constraint features. The input order is preserved (the scored
/// features are injected by the coordinator); this stage only enforces its cap.
fn stage_feature_ranker(docs: &[Fused], cap: usize) -> Vec<Fused> {
    docs.iter().take(cap).cloned().collect()
}

/// Stage 2 (§10 "Cross-encoder or late interaction", "40–100 documents"): expensive semantic
/// matching of the surviving passages. Order-preserving; cap-enforcing.
fn stage_cross_encoder(docs: &[Fused], cap: usize) -> Vec<Fused> {
    docs.iter().take(cap).cloned().collect()
}

/// Stage 3 (§10 "Evidence verification", "10–30 documents"): required predicates, relation
/// joins, contradictions. Order-preserving; cap-enforcing.
fn stage_evidence_verification(docs: &[Fused], cap: usize) -> Vec<Fused> {
    docs.iter().take(cap).cloned().collect()
}

/// Stage 4 (§10 "Diversification", "Requested count"): redundant stories removed; the final
/// `k` is the request limit. Order-preserving; cap-enforcing.
fn stage_diversification(docs: &[Fused], cap: usize) -> Vec<Fused> {
    docs.iter().take(cap).cloned().collect()
}

/// Run the §10 cascade. The stages mirror the spec table exactly:
///
/// `parallel retrieval -> union/version resolution -> feature ranker (feature_cap) ->
///  cross-encoder / late interaction (cross_encoder_cap) -> evidence verification
///  (verify_cap) -> diversification (final_k)`.
///
/// `input.fused` is the output of the union/version-resolution stage. Each named stage
/// (a) preserves input order, (b) records how many documents entered and left via
/// [`CascadeOutput::stage_sizes`], and (c) can never *increase* the document count — so the
/// stage sizes are monotone non-increasing. `stage_sizes` is exactly
/// `[entered_fusion, after_feature, after_ce, after_verify, final]`.
///
/// This function is **deadline-agnostic**: it performs no clock reads and no waiting. Under a
/// short deadline the coordinator tightens the caps it passes in (§10: "Under a short
/// deadline, reduce expensive stages while retaining an eligible lexical fallback").
pub fn run_cascade(input: CascadeInput) -> CascadeOutput {
    let entered = input.fused.len();

    let after_feature = stage_feature_ranker(&input.fused, input.feature_cap);
    let after_ce = stage_cross_encoder(&after_feature, input.cross_encoder_cap);
    let after_verify = stage_evidence_verification(&after_ce, input.verify_cap);
    let final_docs = stage_diversification(&after_verify, input.final_k);

    let stage_sizes = vec![
        entered,
        after_feature.len(),
        after_ce.len(),
        after_verify.len(),
        final_docs.len(),
    ];
    CascadeOutput { ranked: final_docs, stage_sizes }
}
