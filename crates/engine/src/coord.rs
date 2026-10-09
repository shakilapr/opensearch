//! §18 Coordinator loop. OWNER: engine-coord agent.
//!
//! analyze -> plan -> launch -> merge -> fuse -> rank -> evidence -> stop? -> choose action ->
//! revalidate -> launch. The loop is the integration point for every other module here.
//!
//! Determinism: the crate never reads a clock. The coordinator uses a *relative* clock taken
//! from the budget's own accounting — `now_ms = completed_rounds * ROUND_COST_MS`, starting at
//! 0 — so the same request against the same corpus yields byte-identical traces. A request with
//! no neural lane still returns its best eligible lexical results; a request with zero
//! candidates still considers widening/expanding before declaring a gap (§18, last paragraph).

use std::collections::{BTreeMap, HashMap};

use crate::analyze::{analyze_query, AliasPriors};
use crate::budget::{Budget, StopReason};
use crate::diversify::{diversify, DiversityConfig, Scored};
use crate::evidence::{evidence_coverage, EvidenceRecord, Support};
use crate::fuse::{fuse, run_cascade, CascadeInput, FusionPolicy, LaneHits};
use crate::plan::{build_plan, IndexStats, Mode, Plan};
use crate::units::QueryUnit;
use crate::LaneId;

/// Static bound on the round loop. The budget caps rounds at 5 (Deep); this is a hard ceiling so
/// the loop is statically bounded even if a budget is misconfigured (§17/§18).
const MAX_ROUNDS: u32 = 8;
/// Static bound on the action frontier offered to the selector (§12.2 "restrict the action space").
const MAX_ACTIONS: usize = 16;
/// Cap on sources considered for a `Refresh` action.
const MAX_ROUTE_SOURCES: usize = 8;
/// Deterministic per-round cost of the relative clock (§17 accounting).
const ROUND_COST_MS: u64 = 1;
/// Coverage at or above which the coordinator stops early (§17 "sufficient supported coverage").
const SUFFICIENT_COVERAGE: f32 = 1.0;
/// "Two rounds without evidence improvement" is a heuristic (§17) and is logged as such.
const PLATEAU_ROUNDS: u32 = 2;

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
    pub lane_ranks: HashMap<LaneId, usize>,
    pub unit_support: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchResponse {
    pub results: Vec<ResultRow>,
    pub coverage: crate::evidence::Coverage,
    pub stop_reason: StopReason,
    pub trace: Vec<String>,
    /// §16/§22.1 — the optional generated answer. `None` when no `Answerer` is injected, when
    /// the answerer errors, or when a lexical-only request is made. It never changes `results`.
    pub answer: Option<String>,
}

/// Coordinator state (§6 `SearchState`, reduced to what the loop here needs).
///
/// Extra fields beyond the frozen scaffold are additive only and drive the §12 action frontier:
/// they let a selector reason about what has already been tried and what remains uncovered.
#[derive(Debug, Clone, Default)]
pub struct EngineState {
    pub completed_rounds: u32,
    pub candidates: usize,
    pub stop_reason: Option<StopReason>,
    /// Mandatory units still lacking support, with their weights (highest first is *not*
    /// guaranteed here; selectors should compare `f32` weights directly).
    pub uncovered_mandatory: Vec<(String, f32)>,
    /// Lanes already widened this request (§12: widen a lane at most once).
    pub widened: Vec<LaneId>,
    /// Sources already refreshed this request.
    pub refreshed: Vec<String>,
    /// The lane treated as the global lexical recall lane.
    pub global_lexical: LaneId,
}

impl EngineState {
    fn new() -> Self {
        Self { global_lexical: LaneId::Bm25, ..Default::default() }
    }
}

/// §12 level-0 "rules" selector: deterministic, no model. Prefers to `Expand` the
/// highest-weight uncovered mandatory unit, else `Widen` the global lexical lane once, else
/// `Abstain`. This is the default for tests and the API's lexical tier.
#[derive(Debug, Default, Clone, Copy)]
pub struct RuleSelector;

impl RuleSelector {
    pub fn new() -> Self {
        Self
    }
}

impl Selector for RuleSelector {
    fn choose(&self, actions: &[Action], state: &EngineState) -> Option<usize> {
        // 1. Expand the highest-weight uncovered mandatory unit present in the action list.
        let mut best: Option<(usize, f32)> = None;
        for (i, a) in actions.iter().enumerate() {
            if let Action::Expand { unit_id } = a {
                if let Some((_, w)) = state.uncovered_mandatory.iter().find(|(u, _)| u == unit_id) {
                    if best.map_or(true, |(_, bw)| *w > bw) {
                        best = Some((i, *w));
                    }
                }
            }
        }
        if let Some((i, _)) = best {
            return Some(i);
        }
        // 2. Widen the global lexical lane once.
        if let Some(i) = actions.iter().position(|a| {
            matches!(a, Action::Widen { lane } if *lane == state.global_lexical)
        }) {
            return Some(i);
        }
        // Fallback: any other widenable lane.
        if let Some(i) = actions.iter().position(|a| matches!(a, Action::Widen { .. })) {
            return Some(i);
        }
        // 3. Abstain.
        actions.iter().position(|a| matches!(a, Action::Abstain))
    }
}

/// The §18 loop. Never blocks past the deadline; a request with no neural lane still returns
/// its best eligible lexical results.
pub fn run_search(req: SearchRequest, deps: &Deps<'_>) -> SearchResponse {
    let analysis = analyze_query(&req.query, deps.now, req.tz_offset_min);
    let plan = build_plan(&analysis, &deps.stats, req.mode);
    let mut budget = Budget::for_mode(req.mode, req.deadline_ms);
    budget.start(0);

    let mut trace: Vec<String> = Vec::new();
    trace.push(format!(
        "plan: mode={:?} lanes={:?} top_k={} variants={} rerank={} refresh={}",
        req.mode, plan.lanes, plan.per_lane_top_k, plan.query_variants.len(),
        plan.rerank_count, plan.refresh_count
    ));

    let mut state = EngineState::new();

    // Cross-round accumulators.
    let mut raw_hits: Vec<LaneHits> = Vec::new();
    let mut titles: BTreeMap<String, String> = BTreeMap::new();
    let mut searcher_lanes: Vec<Vec<LaneId>> = vec![Vec::new(); deps.lanes.len()];
    let mut seen_sources: Vec<String> = Vec::new();
    let mut prev_coverage: Option<f32> = None;
    let mut plateau: u32 = 0;
    let mut records: Vec<EvidenceRecord> = Vec::new();
    let mut coverage = evidence_coverage(&analysis.units, &records);

    // ---- Round 1: launch the initial lanes from the plan (§18 "reserve_and_launch(initial)"). ----
    // The LEXICAL lane is always launched: every injected searcher is driven once over the
    // original query, so at least one lexical run happens even if the plan is empty.
    if budget.charge_round() {
        state.completed_rounds = 1;
        trace.push(format!(
            "round 1: launch {} searcher(s) over the original query {:?}",
            deps.lanes.len(), req.query
        ));
        let mut launched = 0usize;
        for idx in 0..deps.lanes.len() {
            let searcher = deps.lanes[idx];
            match call_searcher(
                idx, searcher, &req.query, plan.per_lane_top_k, None,
                &mut raw_hits, &mut titles, &mut searcher_lanes, &mut seen_sources,
            ) {
                Ok(n) => launched += n,
                Err(e) => trace.push(format!("lane failed: {e}")),
            }
        }
        state.candidates = launched;
        refresh_evidence(&analysis.units, &titles, &mut records, &mut coverage, &mut state);
        plateau = note_coverage(&mut prev_coverage, coverage.coverage, plateau);
    } else {
        state.stop_reason = Some(StopReason::BudgetExhausted);
    }

    // ---- Follow-up round loop. Bounded statically by MAX_ROUNDS and, in practice, by the
    //      budget's `charge_round` (§17/§18). ----
    let mut round: u32 = state.completed_rounds;
    while round < MAX_ROUNDS {
        let now_ms = (state.completed_rounds as u64).saturating_mul(ROUND_COST_MS);

        // §17 stopping: deadline, sufficient coverage, then the coverage-plateau heuristic.
        if let Some(reason) = early_stop(&budget, now_ms) {
            state.stop_reason = Some(reason);
            trace.push(format!("stop: {reason:?} (deadline)"));
            break;
        }
        if coverage.mandatory_ok && coverage.coverage >= SUFFICIENT_COVERAGE {
            state.stop_reason = Some(StopReason::SufficientCoverage);
            trace.push("stop: SufficientCoverage (all units supported)".to_string());
            break;
        }
        if plateau >= PLATEAU_ROUNDS {
            state.stop_reason = Some(StopReason::Plateau);
            trace.push(format!(
                "stop: Plateau (heuristic: {PLATEAU_ROUNDS} rounds without evidence improvement)"
            ));
            break;
        }

        // §12: bounded action frontier, then the injected selector.
        let actions = admissible_next_actions(&plan, &state, &budget, &seen_sources);
        let choice = deps.selector.choose(&actions, &state);
        let action = match choice.and_then(|i| actions.get(i)) {
            Some(a) => a.clone(),
            None => {
                state.stop_reason = Some(StopReason::NoAdmissibleAction);
                trace.push("stop: NoAdmissibleAction (selector returned None)".to_string());
                break;
            }
        };
        if action == Action::Abstain {
            state.stop_reason = Some(StopReason::NoAdmissibleAction);
            trace.push("stop: NoAdmissibleAction (selector abstained)".to_string());
            break;
        }

        // §12.2 "validate that the action is still legal when execution begins": revalidate the
        // choice against the *current* state and budget before any side effect.
        if !revalidate(&action, &state, &budget) {
            state.stop_reason = Some(StopReason::NoAdmissibleAction);
            trace.push(format!("rejected {action:?}: no longer legal"));
            break;
        }
        // Atomic reservation before dispatch (§17): charge the round, or do not run at all.
        if !budget.charge_round() {
            state.stop_reason = Some(StopReason::BudgetExhausted);
            trace.push(format!("rejected {action:?}: no rounds remaining (not executed)"));
            break;
        }

        round = round.saturating_add(1);
        state.completed_rounds = round;

        match &action {
            Action::Widen { lane } => {
                state.widened.push(*lane);
                trace.push(format!("round {round}: action Widen {{{lane:?}}}"));
                let limit = plan.per_lane_top_k.saturating_mul(2).max(plan.per_lane_top_k + 1);
                let mut targets: Vec<usize> = Vec::new();
                for i in 0..deps.lanes.len() {
                    if searcher_lanes[i].contains(lane) {
                        targets.push(i);
                    }
                }
                if targets.is_empty() {
                    for i in 0..deps.lanes.len() {
                        targets.push(i);
                    }
                }
                for idx in targets {
                    let searcher = deps.lanes[idx];
                    if let Err(e) = call_searcher(
                        idx, searcher, &req.query, limit, None,
                        &mut raw_hits, &mut titles, &mut searcher_lanes, &mut seen_sources,
                    ) {
                        trace.push(format!("lane failed: {e}"));
                    }
                }
            }
            Action::Expand { unit_id } => {
                trace.push(format!("round {round}: action Expand {{{unit_id}}}"));
                let text = analysis
                    .units
                    .iter()
                    .find(|u| &u.id == unit_id)
                    .map(|u| u.text.clone())
                    .unwrap_or_else(|| req.query.clone());
                for idx in 0..deps.lanes.len() {
                    let searcher = deps.lanes[idx];
                    if let Err(e) = call_searcher(
                        idx, searcher, &text, plan.per_lane_top_k, None,
                        &mut raw_hits, &mut titles, &mut searcher_lanes, &mut seen_sources,
                    ) {
                        trace.push(format!("lane failed: {e}"));
                    }
                }
            }
            Action::Refresh { source_id } => {
                state.refreshed.push(source_id.clone());
                budget.fetches_remaining = budget.fetches_remaining.saturating_sub(1);
                trace.push(format!("round {round}: action Refresh {{{source_id}}}"));
                for idx in 0..deps.lanes.len() {
                    let searcher = deps.lanes[idx];
                    if let Err(e) = call_searcher(
                        idx, searcher, &req.query, plan.per_lane_top_k, Some(source_id.as_str()),
                        &mut raw_hits, &mut titles, &mut searcher_lanes, &mut seen_sources,
                    ) {
                        trace.push(format!("lane failed: {e}"));
                    }
                }
            }
            Action::Abstain => {}
        }

        state.candidates = raw_hits.iter().map(|h| h.ranked.len()).sum();
        refresh_evidence(&analysis.units, &titles, &mut records, &mut coverage, &mut state);
        plateau = note_coverage(&mut prev_coverage, coverage.coverage, plateau);
    }

    if state.stop_reason.is_none() {
        state.stop_reason = Some(StopReason::BudgetExhausted);
    }
    let stop_reason = state.stop_reason.unwrap_or(StopReason::BudgetExhausted);
    trace.push(format!("stop reason: {stop_reason:?}"));

    // ---- Finalize: fuse, cascade, diversify, emit rows (§18 tail). ----
    let collapsed = collapse_lane_hits(&raw_hits);
    let fused = fuse(&collapsed, &FusionPolicy::default());
    let cap = plan.rerank_count.max(req.limit);
    let cascaded = run_cascade(CascadeInput {
        fused,
        feature_cap: cap,
        cross_encoder_cap: cap,
        verify_cap: cap,
        final_k: req.limit,
    });
    trace.push(format!("cascade stage sizes: {:?}", cascaded.stage_sizes));

    let lane_ranks = build_lane_ranks(&collapsed);
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
        .map(|s| {
            let ranks = lane_ranks.get(&s.doc_id).cloned().unwrap_or_default();
            let support: Vec<String> = records
                .iter()
                .filter(|r| r.doc_id == s.doc_id && r.status == Support::Supported)
                .map(|r| r.unit_id.clone())
                .collect();
            ResultRow {
                doc_id: s.doc_id,
                score: s.relevance as f64,
                lane_ranks: ranks,
                unit_support: support,
            }
        })
        .collect();

    trace.push(format!("results: {} of limit {}", results.len(), req.limit));

    // ---- Answer generation (§16/§22.1): strictly after selection; must not alter eligibility. ----
    let mut answer: Option<String> = None;
    if let Some(a) = deps.answerer {
        let passages: Vec<String> = results.iter().map(|r| r.doc_id.clone()).collect();
        match a.answer(&req.query, &records, &passages) {
            Ok(text) => {
                trace.push(format!("answer: {} char(s) from the selected evidence", text.len()));
                answer = Some(text);
            }
            Err(e) => trace.push(format!("answer failed: {e}")),
        }
    }

    SearchResponse { results, coverage, stop_reason, trace, answer }
}

/// Deadline-only early stop. `Budget::should_stop` is consulted, but its `BudgetExhausted`
/// verdict is enforced through the atomic `charge_round` at reservation time (§17 "charge the
/// ledger atomically before dispatch") so the selector still gets one honest revalidation pass.
fn early_stop(budget: &Budget, now_ms: u64) -> Option<StopReason> {
    match budget.should_stop(now_ms) {
        Some(StopReason::Deadline) => Some(StopReason::Deadline),
        _ => None,
    }
}

/// §12.2 legality recheck immediately before execution.
fn revalidate(action: &Action, state: &EngineState, budget: &Budget) -> bool {
    match action {
        Action::Widen { lane } => !state.widened.contains(lane),
        Action::Expand { unit_id } => state.uncovered_mandatory.iter().any(|(u, _)| u == unit_id),
        Action::Refresh { source_id } => {
            budget.fetches_remaining > 0 && !state.refreshed.contains(source_id)
        }
        Action::Abstain => false,
    }
}

/// Build the bounded §12.2 action frontier from the current state.
fn admissible_next_actions(
    plan: &Plan,
    state: &EngineState,
    budget: &Budget,
    seen_sources: &[String],
) -> Vec<Action> {
    let mut v: Vec<Action> = Vec::new();
    // Widen: an available lane not yet widened.
    for lane in &plan.lanes {
        if v.len() >= MAX_ACTIONS {
            break;
        }
        if !state.widened.contains(lane) {
            v.push(Action::Widen { lane: *lane });
        }
    }
    // Expand: each uncovered mandatory unit, highest weight first (deterministic tie-break by id).
    let mut unc: Vec<&(String, f32)> = state.uncovered_mandatory.iter().collect();
    unc.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    for (u, _) in unc {
        if v.len() >= MAX_ACTIONS {
            break;
        }
        v.push(Action::Expand { unit_id: u.clone() });
    }
    // Refresh: a source observed in candidates with refresh budget left.
    if budget.fetches_remaining > 0 {
        for s in seen_sources.iter().take(MAX_ROUTE_SOURCES) {
            if v.len() >= MAX_ACTIONS {
                break;
            }
            if !state.refreshed.contains(s) {
                v.push(Action::Refresh { source_id: s.clone() });
            }
        }
    }
    // Abstain is always available.
    if v.len() < MAX_ACTIONS {
        v.push(Action::Abstain);
    }
    v
}

/// Collapse raw per-call lane hits into one [`LaneHits`] per lane, keeping each document's best
/// (lowest) rank. Deterministic: lanes in first-appearance order, docs by rank then id.
fn collapse_lane_hits(raw: &[LaneHits]) -> Vec<LaneHits> {
    let mut order: Vec<LaneId> = Vec::new();
    let mut maps: Vec<HashMap<String, usize>> = Vec::new();
    for lh in raw {
        let pos = match order.iter().position(|l| *l == lh.lane) {
            Some(p) => p,
            None => {
                order.push(lh.lane);
                maps.push(HashMap::new());
                order.len() - 1
            }
        };
        let map = &mut maps[pos];
        for (i, doc) in lh.ranked.iter().enumerate() {
            let rank = i + 1;
            let e = map.entry(doc.clone()).or_insert(rank);
            if rank < *e {
                *e = rank;
            }
        }
    }
    let mut out: Vec<LaneHits> = Vec::new();
    for (lane, map) in order.iter().zip(maps.iter()) {
        let mut v: Vec<(usize, String)> = map.iter().map(|(d, r)| (*r, d.clone())).collect();
        v.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        out.push(LaneHits { lane: *lane, ranked: v.into_iter().map(|(_, d)| d).collect() });
    }
    out
}

/// Provenance: doc_id -> { lane -> best rank }.
fn build_lane_ranks(collapsed: &[LaneHits]) -> HashMap<String, HashMap<LaneId, usize>> {
    let mut out: HashMap<String, HashMap<LaneId, usize>> = HashMap::new();
    for lh in collapsed {
        for (i, doc) in lh.ranked.iter().enumerate() {
            out.entry(doc.clone()).or_default().insert(lh.lane, i + 1);
        }
    }
    out
}

/// Drive one searcher and fold its candidates into the accumulators. Groups a single call's
/// candidates by their lane tag, preserving the searcher's own ranking within each lane.
#[allow(clippy::too_many_arguments)]
fn call_searcher(
    idx: usize,
    searcher: &dyn search_retrieval::LaneSearcher,
    query: &str,
    limit: usize,
    source: Option<&str>,
    raw_hits: &mut Vec<LaneHits>,
    titles: &mut BTreeMap<String, String>,
    searcher_lanes: &mut Vec<Vec<LaneId>>,
    seen_sources: &mut Vec<String>,
) -> Result<usize, String> {
    let cands = searcher.search(query, limit, source).map_err(|e| e.to_string())?;
    let mut lane_order: Vec<LaneId> = Vec::new();
    let mut groups: Vec<Vec<String>> = Vec::new();
    for c in cands {
        if let Some(src) = &c.source_id {
            if !seen_sources.contains(src) {
                seen_sources.push(src.clone());
            }
        }
        if !searcher_lanes.get(idx).map_or(false, |l| l.contains(&c.lane))
            && idx < searcher_lanes.len()
        {
            searcher_lanes[idx].push(c.lane);
        }
        let pos = match lane_order.iter().position(|l| *l == c.lane) {
            Some(p) => p,
            None => {
                lane_order.push(c.lane);
                groups.push(Vec::new());
                lane_order.len() - 1
            }
        };
        if !groups[pos].contains(&c.doc_id) {
            groups[pos].push(c.doc_id.clone());
        }
        titles.entry(c.doc_id.clone()).or_insert_with(|| c.title.clone().unwrap_or_default());
    }
    let mut count = 0usize;
    for (lane, docs) in lane_order.into_iter().zip(groups.into_iter()) {
        count += docs.len();
        raw_hits.push(LaneHits { lane, ranked: docs });
    }
    Ok(count)
}

/// Recompute evidence records and coverage from the retained document metadata, then refresh the
/// state's uncovered-mandatory list.
///
/// No `EvidenceVerifier` is injected through [`Deps`], so the coordinator derives deterministic
/// evidence from document *titles*: a unit is `Supported` when its normalized text is a
/// case-insensitive substring of a retrieved document's title. This is a conservative stand-in
/// for the §16 verifier and never marks a unit supported on speculation.
fn refresh_evidence(
    units: &[QueryUnit],
    titles: &BTreeMap<String, String>,
    records: &mut Vec<EvidenceRecord>,
    coverage: &mut crate::evidence::Coverage,
    state: &mut EngineState,
) {
    records.clear();
    for u in units {
        let needle = u.text.trim().to_lowercase();
        if needle.is_empty() {
            continue;
        }
        for (doc_id, title) in titles {
            if title.to_lowercase().contains(&needle) {
                records.push(EvidenceRecord {
                    unit_id: u.id.clone(),
                    doc_id: doc_id.clone(),
                    status: Support::Supported,
                    span: None,
                    doc_version: "v1".to_string(),
                });
                break;
            }
        }
    }
    *coverage = evidence_coverage(units, records);
    state.uncovered_mandatory = units
        .iter()
        .filter(|u| coverage.unsupported.contains(&u.id))
        .map(|u| (u.id.clone(), u.weight))
        .collect();
}

/// Track the coverage-plateau heuristic: return the number of consecutive rounds with no
/// evidence improvement (unchanged coverage), and update `prev`.
fn note_coverage(prev: &mut Option<f32>, current: f32, plateau: u32) -> u32 {
    let next = match *prev {
        Some(p) if (current - p).abs() <= f32::EPSILON => plateau.saturating_add(1),
        _ => 0,
    };
    *prev = Some(current);
    next
}
