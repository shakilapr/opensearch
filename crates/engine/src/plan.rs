//! §8 Query planning and resource allocation. OWNER: engine-plan agent.

use crate::analyze::QueryAnalysis;
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

/// Build a conservative plan from query features and index statistics (§8). A global recall
/// lane is always present; source routing is an accelerator, never an irreversible gate.
pub fn build_plan(analysis: &QueryAnalysis, stats: &IndexStats, mode: Mode) -> Plan {
    // SCAFFOLD: the conservative default. The engine-plan agent implements the §8 rules
    // (identifier/exact-code -> lexical + small rerank; conceptual -> dense/sparse lanes;
    // per-mode caps from §17; variant caps + origin tagging).
    let _ = stats;
    let top_k = match mode {
        Mode::Fast => 30,
        Mode::Auto => 100,
        Mode::Deep => 300,
    };
    Plan {
        lanes: vec![LaneId::Bm25],
        per_lane_top_k: top_k,
        source_route_count: match mode {
            Mode::Fast => 10,
            Mode::Auto => 30,
            Mode::Deep => 100,
        },
        rerank_count: top_k,
        refresh_count: match mode {
            Mode::Fast => 0,
            Mode::Auto => 8,
            Mode::Deep => 30,
        },
        query_variants: vec![QueryVariant {
            text: analysis.original.clone(),
            origin: VariantOrigin::Original,
            family: 0,
        }],
        stage_deadlines_ms: StageDeadlines::default(),
    }
}

/// §8: reject a rewrite that drops negation, an identifier, a date, or a mandatory constraint.
pub fn variant_preserves_constraints(variant: &str, analysis: &QueryAnalysis) -> bool {
    let v = variant.to_lowercase();
    if analysis.identifiers.iter().any(|id| !v.contains(&id.to_lowercase())) {
        return false;
    }
    if let Some(t) = &analysis.temporal {
        let _ = t;
        if !v.chars().any(|c| c.is_ascii_digit()) {
            // A dated query whose rewrite lost every digit lost the date.
            return false;
        }
    }
    if analysis.original.contains("not ") && !v.contains("not ") {
        return false;
    }
    true
}
