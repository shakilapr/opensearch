//! §8 Query planning and resource allocation. OWNER: engine-plan agent.
//!
//! Implements the conservative planning rules from
//! `engine/engine-architecture.md` §8 ("Query planning and resource allocation") and the
//! per-mode ceilings from §17 ("Budgets, deadlines, and stopping"). Frozen contract:
//! `docs/specs/ENGINE_SPEC.md` §8.
//!
//! Key §8 quotes this module encodes:
//! > Avoid universal fan-out: an exact error-code query may need lexical retrieval and a
//! > small rerank; a conceptual query may justify dense and sparse lanes. The normal plan
//! > still keeps a global recall path instead of making source routing an irreversible gate.
//! > Initial query expansion can include aliases and a small number of semantic variants.
//! > Search the original query in its own lane. Cap expansions, record their origin, and
//! > reject rewrites that remove negation, identifiers, dates, or mandatory constraints.

use crate::analyze::{PredicateKind, QueryAnalysis};
use crate::LaneId;

/// Search mode. Budget defaults per mode live in §17.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Fast,
    Auto,
    Deep,
}

/// Read-side index statistics the planner conditions on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IndexStats {
    pub docs: u64,
    pub avg_doc_len: f32,
    pub updated_at: i64,
}

/// A query variant and where it came from. `family` groups near-identical rewrites so fusion
/// can cap their combined weight (§10).
#[derive(Debug, Clone, PartialEq)]
pub struct QueryVariant {
    pub text: String,
    pub origin: VariantOrigin,
    pub family: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariantOrigin {
    Original,
    Alias,
    Phrase,
    Concept,
}

/// Stage budgets are shorter than the overall deadline (§17).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StageDeadlines {
    pub retrieve_ms: u64,
    pub fuse_ms: u64,
    pub rerank_ms: u64,
    pub finalize_ms: u64,
}

/// The physical plan (§8).
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub lanes: Vec<LaneId>,
    pub per_lane_top_k: usize,
    pub source_route_count: usize,
    pub rerank_count: usize,
    pub refresh_count: usize,
    pub query_variants: Vec<QueryVariant>,
    pub stage_deadlines_ms: StageDeadlines,
}

/// §8 "Cap expansions": the original plus at most three expansions. Every expansion is
/// recorded with its [`VariantOrigin`]; the cap is a static bound so planning is bounded.
pub const MAX_QUERY_VARIANTS: usize = 4;

/// The most alias candidates considered from the analysis before the cap trims them. A static
/// bound on the expansion loop (§8 "Cap expansions").
pub const MAX_ALIAS_CANDIDATES: usize = 8;

/// §17 "Overall server deadline": 200 ms / 3 s / 20 s.
pub fn mode_deadline_ms(mode: Mode) -> u64 {
    match mode {
        Mode::Fast => 200,
        Mode::Auto => 3_000,
        Mode::Deep => 20_000,
    }
}

/// §17 "Reserve finalization time before scheduling work": split the overall deadline across
/// the stages so the parts sum to **no more than** the overall deadline, with finalize
/// (access/tombstone recheck, finalization, trace) reserved up front.
///
/// Split: retrieve 45%, fuse 10%, finalize 20%, and the remainder (25%) to rerank. Integer
/// floor division can only shrink `retrieve`/`fuse`/`finalize`, so the assigned `rerank` is
/// always the non-negative remainder and the four always sum to exactly `total_ms`.
pub fn split_stage_deadlines(total_ms: u64) -> StageDeadlines {
    let finalize_ms = total_ms / 5;
    let retrieve_ms = total_ms * 45 / 100;
    let fuse_ms = total_ms * 10 / 100;
    let rerank_ms = total_ms - retrieve_ms - fuse_ms - finalize_ms;
    StageDeadlines {
        retrieve_ms,
        fuse_ms,
        rerank_ms,
        finalize_ms,
    }
}

/// Build a conservative plan from query features and index statistics (§8). A global recall
/// lane is always present; source routing is an accelerator, never an irreversible gate.
pub fn build_plan(analysis: &QueryAnalysis, stats: &IndexStats, mode: Mode) -> Plan {
    // §8: "an exact error-code query may need lexical retrieval and a small rerank; a
    // conceptual query may justify dense and sparse lanes." Classify the query as
    // lexical/exact-code vs conceptual. A lexical query is one carrying a hard lexical
    // surface: an identifier, a quoted phrase, an ExactPhrase predicate, or a direct-intent
    // that outranks semantic intent. Everything else is conceptual.
    let lexical = is_lexical_query(analysis);

    // §8: "The normal plan still keeps a global recall path instead of making source routing an
    // irreversible gate." `Lane::Bm25` IS the global recall path and is always present. The
    // retrieval crate (`search-retrieval`) exposes only `Lane::Bm25` and `Lane::SourceFirst`;
    // the spec's "dense and sparse lanes" map to the additional source-routing accelerator
    // lane here. It is only ADDED for conceptual queries, never substituted for the global lane.
    let mut lanes: Vec<LaneId> = vec![LaneId::Bm25];
    if !lexical {
        lanes.push(LaneId::SourceFirst);
    }

    // §17 mode ceilings (the table is the authority for the mode caps):
    //   Reranked document cap       30 / 100 / 300
    //   Sources searched in detail  10 / 30 / 100
    //   Live fetches total           0 /   8 /  30
    // `rerank_count` is the §17 reranked-document cap, reduced for a lexical/exact-code query
    // ("a small rerank") and left at the ceiling for a conceptual query ("a larger rerank").
    // It never exceeds the mode ceiling.
    let rerank_ceiling = match mode {
        Mode::Fast => 30,
        Mode::Auto => 100,
        Mode::Deep => 300,
    };
    let rerank_count = if lexical {
        (rerank_ceiling / 3).max(1)
    } else {
        rerank_ceiling
    };
    // §17 "Sources searched in detail": reserved routing capacity for the mode. Constant in the
    // query kind — the slot count is a budget, not a gate, so it is not zeroed by query class.
    let source_route_count = match mode {
        Mode::Fast => 10,
        Mode::Auto => 30,
        Mode::Deep => 100,
    };
    // §17 "Live fetches total": 0 / 8 / 30.
    let refresh_count = match mode {
        Mode::Fast => 0,
        Mode::Auto => 8,
        Mode::Deep => 30,
    };
    // Per-lane breadth is not a §17 row; §10's cascade gives "Parallel retrieval 100–300 hits
    // per active lane". Derive a per-mode base and a documented ceiling, then scale up modestly
    // with `IndexStats.docs` (+1 hit per 100k docs) and clamp at the ceiling so the mode cap is
    // never exceeded.
    let per_lane_top_k = per_lane_top_k(mode, stats.docs);

    // §8: "Search the original query in its own lane." Always exactly one `Original`, first,
    // family 0. Expansions follow, capped (MAX_QUERY_VARIANTS) and origin-tagged.
    let mut query_variants: Vec<QueryVariant> = Vec::with_capacity(MAX_QUERY_VARIANTS);
    query_variants.push(QueryVariant {
        text: analysis.original.clone(),
        origin: VariantOrigin::Original,
        family: 0,
    });
    // §8: "Initial query expansion can include aliases ... Cap expansions, record their origin,
    // and reject rewrites that remove negation, identifiers, dates, or mandatory constraints."
    // The frozen `QueryAnalysis` carries no dedicated alias field; the analysis-supplied
    // expansion strings live in `analysis.concepts`. Each accepted candidate is tagged
    // `VariantOrigin::Alias` under one shared, non-zero family id (1) so §10's family-cap can
    // bound the whole alias group's weight.
    let alias_family: u8 = 1;
    for candidate in analysis.concepts.iter().take(MAX_ALIAS_CANDIDATES) {
        if query_variants.len() >= MAX_QUERY_VARIANTS {
            break;
        }
        let text = candidate.trim();
        if text.is_empty() || text == analysis.original {
            continue;
        }
        if query_variants.iter().any(|v| v.text == text) {
            continue;
        }
        if !variant_preserves_constraints(text, analysis) {
            continue;
        }
        query_variants.push(QueryVariant {
            text: text.to_string(),
            origin: VariantOrigin::Alias,
            family: alias_family,
        });
    }

    Plan {
        lanes,
        per_lane_top_k,
        source_route_count,
        rerank_count,
        refresh_count,
        query_variants,
        // §17: stage deadlines are shorter than the overall deadline and sum to no more than it.
        stage_deadlines_ms: split_stage_deadlines(mode_deadline_ms(mode)),
    }
}

/// §8 lexical/exact-code detection: identifiers, quoted phrases, an ExactPhrase predicate, or
/// direct intent outranking semantic intent.
fn is_lexical_query(analysis: &QueryAnalysis) -> bool {
    !analysis.identifiers.is_empty()
        || !analysis.phrases.is_empty()
        || analysis
            .hard_predicates
            .iter()
            .any(|p| p.kind == PredicateKind::ExactPhrase)
        || analysis.intent.direct > analysis.intent.semantic
}

/// §8/§10 per-lane breadth. Base scaled modestly by corpus size, clamped at the mode ceiling.
fn per_lane_top_k(mode: Mode, docs: u64) -> usize {
    let ceiling = match mode {
        Mode::Fast => 100,
        Mode::Auto => 200,
        Mode::Deep => 300,
    };
    let base = match mode {
        Mode::Fast => 40,
        Mode::Auto => 100,
        Mode::Deep => 200,
    };
    let room = (ceiling - base) as u64;
    let bonus = (docs / 100_000).min(room) as usize;
    base + bonus
}

/// §8/§17 first-pass candidate-limit sizing, derived from the scan-cost attribution.
///
/// [`Plan::per_lane_top_k`] is the mode's *ceiling* (100–300 hits per lane). The first pass
/// does not need that depth: the eval attributes retrieval latency almost entirely to
/// `index_scan_limit`, and the result set is bounded by the caller's `limit`. A request for
/// `limit` results needs only a small multiple of that per lane — fusion and reranking work
/// on the head, and the coordinator widens (2×) when coverage is insufficient, so a shallow
/// first pass costs recall only on the queries that would have widened anyway. Floored per
/// mode so fusion and reranking still have a pool, and never above the plan's ceiling.
pub fn size_first_pass_top_k(mode: Mode, docs: u64, limit: usize) -> usize {
    let floor = match mode {
        Mode::Fast => 10,
        Mode::Auto => 20,
        Mode::Deep => 40,
    };
    let ceiling = per_lane_top_k(mode, docs);
    limit.saturating_mul(3).max(floor).min(ceiling)
}

/// §8: reject a rewrite that drops negation, an identifier, a date, or a mandatory constraint.
///
/// Concretely, the variant is rejected when it:
///  (a) drops any identifier from `analysis.identifiers` (case-insensitive containment);
///  (b) drops a negation that was in the original (`not`, `n't`, `no `);
///  (c) loses every digit while `analysis.temporal` is set, or while the original carried a
///      4-digit year;
///  (d) drops the exact text of any `ExactPhrase` predicate.
///
/// Checks (a), (b), (d) are case-insensitive containment; the markers are heuristic (a
/// substring `not` also matches `cannot`/`note`), which errs on the safe side of preserving a
/// constraint rather than on dropping one.
pub fn variant_preserves_constraints(variant: &str, analysis: &QueryAnalysis) -> bool {
    let v = variant.to_lowercase();

    // (a) identifiers must survive, case-insensitively.
    if analysis
        .identifiers
        .iter()
        .any(|id| !v.contains(&id.to_lowercase()))
    {
        return false;
    }

    // (b) negation must survive. Check each marker present in the original.
    let original_lc = analysis.original.to_lowercase();
    for marker in ["not", "n't", "no "] {
        if original_lc.contains(marker) && !v.contains(marker) {
            return false;
        }
    }

    // (c) a dated query must not lose every digit: either a resolved interval, or a 4-digit
    // year in the original.
    let dated = analysis.temporal.is_some() || contains_four_digit_year(&analysis.original);
    if dated && !v.chars().any(|c| c.is_ascii_digit()) {
        return false;
    }

    // (d) every ExactPhrase predicate's exact text must survive.
    if analysis.hard_predicates.iter().any(|p| {
        p.kind == PredicateKind::ExactPhrase && !v.contains(&p.value.to_lowercase())
    }) {
        return false;
    }

    true
}

/// Scan for a run of four consecutive ASCII digits (a 4-digit year heuristic). Bounded by the
/// input length; no regex, no allocation.
fn contains_four_digit_year(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut run = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            run += 1;
            if run >= 4 {
                return true;
            }
        } else {
            run = 0;
        }
        i += 1;
    }
    false
}
