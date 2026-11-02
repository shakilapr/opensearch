//! search-engine — the query-time algorithmic core.
//!
//! Implements the algorithm half of `docs/legacy/engine-architecture.md` (§6–§18) above the
//! crawl/index spine. Frozen contract: `docs/specs/ENGINE_SPEC.md`.
//!
//! Determinism rules (see the spec): no network, no clock reads (time is `now: i64`), no
//! `unwrap`/`panic!`/`todo!` on request paths, and every loop statically bounded.

pub mod analyze;
pub mod budget;
pub mod coord;
pub mod diversify;
pub mod evidence;
pub mod fuse;
pub mod plan;
pub mod planner;
pub mod route;
pub mod timing;
pub mod units;

pub use analyze::{
    analyze_query, link_entities, AliasPriors, EntityHypothesis, EntityMention, Intent, Predicate,
    PredicateKind, Preference, QueryAnalysis, TimeInterval,
};
pub use budget::{Budget, StopReason};
pub use coord::{
    run_search, Action, Answerer, Deps, ResultRow, SearchRequest, SearchResponse, Selector,
};
pub use diversify::{diversify, DiversityConfig, Scored};
pub use evidence::{
    evidence_coverage, independent_origins, normalize_tokens, title_supports_unit, Coverage,
    EvidenceRecord, EvidenceVerifier, Support,
};
pub use fuse::{fuse, run_cascade, CascadeInput, CascadeOutput, Fused, FusionPolicy, LaneHits};
pub use plan::{
    build_plan, variant_preserves_constraints, IndexStats, Mode, Plan, QueryVariant,
    StageDeadlines, VariantOrigin,
};
pub use planner::{PlannedQueries, QueryPlanner};
pub use units::{decompose, joins, JoinKey, QueryUnit, UnitKind};

/// Lane identity is the retrieval crate's lane enum (§9).
pub type LaneId = search_retrieval::Lane;

/// §7 caps: a decomposition never exceeds this many units.
pub const MAX_UNITS: usize = 8;
