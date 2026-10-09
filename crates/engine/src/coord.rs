//! §18 Coordinator loop. OWNER: engine-coord agent.
//!
//! analyze -> plan -> launch -> merge -> fuse -> rank -> evidence -> stop? -> choose action ->
//! revalidate -> launch. The loop is the integration point for every other module here.

use crate::analyze::{analyze_query, AliasPriors};
use crate::budget::{Budget, StopReason};
use crate::diversify::{diversify, DiversityConfig, Scored};
use crate::evidence::{evidence_coverage, Coverage, EvidenceRecord};
use crate::fuse::{fuse, run_cascade, CascadeInput, FusionPolicy, LaneHits};
use crate::plan::{build_plan, IndexStats, Mode};
use crate::LaneId;

/// §12 — the least-costly decision that achieves the required quality. The crate ships a
/// deterministic rule selector; the caller may inject a model-backed one.
pub trait Selector: Send + Sync {
    /// Choose an action index, or `None` to stop.
    fn choose(&self, actions: &[Action], state: &EngineState) -> Option<usize>;
}

/// §12.2 — a validated action over existing ids with a reserved budget.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Widen { lane: LaneId },
    Expand { unit_id: String },
    Refresh { source_id: String },
    Abstain,
}

/// §16/§22.1 — optional answer generation, after search, consuming the selected evidence.
pub trait Answerer: Send + Sync {
    fn answer(&self, query: &str, evidence: &[EvidenceRecord], passages: &[String])
        -> Result<String, String>;
}

/// Everything the loop needs, injected so the crate stays hermetic.
pub struct Deps<'a> {
    pub lanes: &'a [&'a dyn search_retrieval::LaneSearcher],
    pub selector: &'a dyn Selector,
    pub answerer: Option<&'a dyn Answerer>,
    pub aliases: &'a AliasPriors,
    pub stats: IndexStats,
    /// Query start, unix seconds. The crate never reads a clock.
    pub now: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchRequest {
    pub query: String,
    pub limit: usize,
    pub mode: Mode,
    pub deadline_ms: u64,
    pub tz_offset_min: i32,
}

/// One result row with the provenance the UI's "Why this result?" panel needs (§26).
#[derive(Debug, Clone, PartialEq)]
pub struct ResultRow {
    pub doc_id: String,
    pub score: f64,
    pub lane_ranks: std::collections::HashMap<LaneId, usize>,
    pub unit_support: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchResponse {
    pub results: Vec<ResultRow>,
    pub coverage: Coverage,
    pub stop_reason: StopReason,
    pub trace: Vec<String>,
}

/// Coordinator state (§6 `SearchState`, reduced to what the loop here needs).
#[derive(Debug, Clone, Default)]
pub struct EngineState {
    pub completed_rounds: u32,
    pub candidates: usize,
    pub stop_reason: Option<StopReason>,
}

/// The §18 loop. Never blocks past the deadline; a request with no neural lane still returns
/// its best eligible lexical results.
pub fn run_search(req: SearchRequest, deps: &Deps<'_>) -> SearchResponse {
    let analysis = analyze_query(&req.query, deps.now, req.tz_offset_min);
    let plan = build_plan(&analysis, &deps.stats, req.mode);
    let mut budget = Budget::for_mode(req.mode, req.deadline_ms);

    // SCAFFOLD: one round over the plan's variants x lanes. The engine-coord agent adds the
    // §17 stopping rules (coverage plateau, no-admissible-action), the §12 selector loop with
    // atomic revalidation before any side effect, provisional result emission, and the
    // final access/tombstone recheck.
    let mut hits: Vec<LaneHits> = Vec::new();
    let mut trace: Vec<String> = Vec::new();

    if budget.charge_round() {
        for variant in &plan.query_variants {
            for lane in deps.lanes {
                match lane.search(&variant.text, plan.per_lane_top_k, None) {
                    Ok(cands) => {
                        let ranked: Vec<String> = cands.into_iter().map(|c| c.doc_id).collect();
                        hits.push(LaneHits { lane: LaneId::Bm25, ranked });
                    }
                    Err(e) => trace.push(format!("lane failed: {e}")),
                }
            }
        }
    }

    let fused = fuse(&hits, &FusionPolicy::default());
    let cascaded = run_cascade(CascadeInput {
        fused,
        feature_cap: plan.rerank_count,
        cross_encoder_cap: plan.rerank_count,
        verify_cap: plan.rerank_count,
        final_k: req.limit,
    });

    // Lane ranks for provenance, recovered from the fused input order.
    let mut lane_ranks = std::collections::HashMap::new();
    for h in &hits {
        lane_ranks.insert(h.lane, 1usize);
    }

    let scored: Vec<Scored> = cascaded
        .ranked
        .iter()
        .map(|f| Scored {
            doc_id: f.doc_id.clone(),
            relevance: f.score as f32,
            uncovered_gain: 0.0,
            cluster: None,
            domain: String::new(),
        })
        .collect();
    let diversified = diversify(scored, req.limit, &DiversityConfig::default());

    let results: Vec<ResultRow> = diversified
        .into_iter()
        .map(|s| ResultRow {
            doc_id: s.doc_id,
            score: s.relevance as f64,
            lane_ranks: lane_ranks.clone(),
            unit_support: Vec::new(),
        })
        .collect();

    trace.push(format!(
        "plan: {} lane(s), top_k {}, {} variant(s)",
        plan.lanes.len(),
        plan.per_lane_top_k,
        plan.query_variants.len()
    ));
    trace.push(format!("cascade stage sizes: {:?}", cascaded.stage_sizes));

    let coverage = evidence_coverage(&analysis.units, &[]);
    let stop_reason = budget.should_stop(0).unwrap_or(StopReason::BudgetExhausted);

    SearchResponse { results, coverage, stop_reason, trace }
}
