# Engine Spec — the algorithmic core (`search-engine`)

Frozen interfaces for `crates/engine`. This implements the query-time half of
`engine/engine-architecture.md` (§6–§18), which was specified but never built. The
crawl/index spine (Stages 0–10) stays as-is; this crate sits **above** it and **below**
`search-api`.

Do not change Stage-0..10 contracts. Do not depend on `search-agent0` (that is the binary);
`search-api` depends on this crate, not the other way round.

## Ground rules

- Deterministic and hermetic: no network, no clock reads. Time is an input (`now: i64`,
  unix seconds). Every LLM decision is injected via a trait; the crate ships a
  deterministic stub for tests.
- No `unwrap`/`expect`/`panic!`/`todo!` on request paths. Errors are returned.
- Bounded: every loop has a statically reachable bound (budget caps, lane caps, unit caps).
- The original query string is preserved verbatim on every derived artifact (§6).

## Dependencies

`search-core`, `search-retrieval` (lanes: `Lane`, `Candidate`, `LaneSearcher`),
`search-rerank` (`Rrf`, `Mmr`), `search-indexer` (stats + the lexical lane adapter).
Never: `search-agent0`, `search-api`, `search-metadata`.

## Modules and frozen signatures

### `analyze` (§6) — query analysis and belief state

```rust
pub struct QueryAnalysis {
    pub original: String,             // immutable, verbatim
    pub normalized: String,           // casefolded, unicode-normalized, whitespace-collapsed
    pub language: Option<String>,
    pub phrases: Vec<String>,         // quoted spans, verbatim (case preserved)
    pub identifiers: Vec<String>,     // versions, error codes, symbols — case preserved
    pub terms: Vec<String>,           // normalized searchable terms
    pub entities: Vec<EntityMention>,
    pub concepts: Vec<String>,
    pub hard_predicates: Vec<Predicate>,
    pub soft_preferences: Vec<Preference>,
    pub temporal: Option<TimeInterval>,
    pub intent: Intent,
    pub units: Vec<QueryUnit>,
}

pub struct EntityMention { pub text: String, pub start: usize, pub end: usize }
pub struct Predicate { pub kind: PredicateKind, pub value: String }
pub enum PredicateKind { ExactPhrase, Identifier, Domain, Language, DateAfter, DateBefore }
pub struct Preference { pub text: String, pub weight: f32 }
pub struct TimeInterval { pub start: i64, pub end: i64 }   // UTC seconds, half-open
pub struct Intent { pub direct: f32, pub semantic: f32, pub ambiguous: f32, pub multihop: f32 }

/// Parse a query. `now` and `tz_offset_min` resolve relative dates; `now` is the
/// query start time. Never mutates the query.
pub fn analyze_query(query: &str, now: i64, tz_offset_min: i32) -> QueryAnalysis;

/// Entity linking keeps several hypotheses and always keeps an "unknown" option (§6).
pub struct EntityHypothesis { pub mention: String, pub candidates: Vec<(String, f32)>, pub chosen: Option<String> }
pub struct AliasPriors { pub aliases: std::collections::BTreeMap<String, Vec<(String, f32)>> }
impl AliasPriors { pub fn empty() -> Self; pub fn insert(&mut self, mention: &str, entity: &str, prior: f32); }
pub fn link_entities(mentions: &[EntityMention], priors: &AliasPriors) -> Vec<EntityHypothesis>;
```

### `units` (§7) — constraint decomposition and evidence joins

```rust
pub struct QueryUnit {
    pub id: String,                   // "U1", "U2", ... stable within a query
    pub text: String,
    pub depends_on: Vec<String>,
    pub mandatory: bool,
    pub weight: f32,
    pub kind: UnitKind,
}
pub enum UnitKind { Identify, Reason, Destination, Quantity, Attribute, Relation, Other }

/// Decompose ONLY when relationships or several independent requirements justify it (§7);
/// a simple query yields exactly one unit. Never more than `MAX_UNITS` (8).
pub fn decompose(analysis: &QueryAnalysis) -> Vec<QueryUnit>;

/// §7 join: candidate keys for two units may be joined only on a verified shared key;
/// `order_ok` encodes the required time ordering. Mirrors nav::evidence_join semantics.
pub struct JoinKey { pub unit_a: String, pub unit_b: String, pub key: String }
pub fn joins(units: &[QueryUnit], shared: &[JoinKey]) -> Vec<JoinKey>;
```

### `plan` (§8) — query planning and resource allocation

```rust
pub enum Mode { Fast, Auto, Deep }
pub struct IndexStats { pub docs: u64, pub avg_doc_len: f32, pub updated_at: i64 }

pub struct Plan {
    pub lanes: Vec<search_retrieval::Lane>,
    pub per_lane_top_k: usize,
    pub source_route_count: usize,
    pub rerank_count: usize,
    pub refresh_count: usize,
    pub query_variants: Vec<QueryVariant>,
    pub stage_deadlines_ms: StageDeadlines,
}
pub struct QueryVariant { pub text: String, pub origin: VariantOrigin, pub family: u8 }
pub enum VariantOrigin { Original, Alias, Phrase, Concept }
pub struct StageDeadlines { pub retrieve_ms: u64, pub fuse_ms: u64, pub rerank_ms: u64, pub finalize_ms: u64 }

/// Conservative rules first (§8): an identifier/exact-code query gets lexical + tiny rerank;
/// a conceptual query may justify dense. A global recall lane is always present.
pub fn build_plan(analysis: &QueryAnalysis, stats: &IndexStats, mode: Mode) -> Plan;

/// Reject a rewrite that drops negation, an identifier, a date, or a mandatory constraint (§8).
pub fn variant_preserves_constraints(variant: &str, analysis: &QueryAnalysis) -> bool;
```

### `budget` (§17) — budgets, deadlines, stopping

```rust
pub struct Budget { /* mode defaults from §17 */ }
impl Budget {
    pub fn for_mode(mode: Mode, deadline_ms: u64) -> Self;
    pub fn should_stop(&self, now_ms: u64) -> Option<StopReason>;
    pub fn charge_round(&mut self) -> bool;      // false when no rounds remain
    pub fn remaining_ms(&self, now_ms: u64) -> u64;
}
pub enum StopReason { Deadline, BudgetExhausted, SufficientCoverage, LimitReached, NoAdmissibleAction, Cancelled, Plateau }
// Read-back priority when several hold at once: Deadline > LimitReached > SufficientCoverage > BudgetExhausted.
```

### `fuse` (§10) — candidate fusion and ranking cascade

```rust
pub struct LaneHits { pub lane: search_retrieval::Lane, pub ranked: Vec<String> }   // doc ids, best first
pub struct FusionPolicy { pub c: f64, pub weights: std::collections::BTreeMap<LaneId, f64>, pub variant_family_cap: f64 }
pub struct Fused { pub doc_id: String, pub score: f64 }

/// Weighted RRF: score(d) = sum_l w_l / (c + rank_l(d)); ranks start at 1; missing lane = 0.
/// A variant family's total weight is capped so near-identical rewrites cannot outvote the original.
pub fn fuse(lanes: &[LaneHits], policy: &FusionPolicy) -> Vec<Fused>;

/// The cascade stages from §10. Each stage takes at most its cap and returns survivors in order.
pub fn run_cascade(stages: CascadeInput) -> CascadeOutput;
```

### `evidence` (§16) — evidence scoring

```rust
pub enum Support { Supported, Contradicted, Unknown }
pub struct EvidenceRecord { pub unit_id: String, pub doc_id: String, pub status: Support, pub span: Option<(usize, usize)>, pub doc_version: String }
pub struct Coverage { pub coverage: f32, pub mandatory_ok: bool, pub unsupported: Vec<String> }

/// coverage = sum(unit_weight * supported) / sum(unit_weight); every mandatory unit must pass
/// individually — high average coverage never overrides an unsatisfied mandatory predicate (§16/§7).
pub fn evidence_coverage(units: &[QueryUnit], records: &[EvidenceRecord]) -> Coverage;

/// Independent origins, not syndicated copies, count as corroboration (§16).
pub fn independent_origins(records: &[EvidenceRecord], origin_of: &dyn Fn(&str) -> String) -> usize;

/// §16 content verification, injected by the layer above (which can see document text).
/// Injected through `Deps.verifier`; consulted first, with the title matcher as fallback.
pub trait EvidenceVerifier: Send + Sync {
    fn supports(&self, unit_text: &str, doc_id: &str) -> bool;
}

/// Deterministic title fallback: lowercase both sides, map every non-alphanumeric char to a
/// space, split on whitespace; the title supports the unit when all of its tokens appear (units
/// of <= 4 tokens) or at least ceil(0.8 * token_count) do (longer units; capped at 15 tokens).
pub fn title_supports_unit(unit_text: &str, title: &str) -> bool;
```

### `diversify` (§16) — greedy maximal marginal relevance

```rust
pub struct Scored { pub doc_id: String, pub relevance: f32, pub uncovered_gain: f32, pub cluster: Option<String>, pub domain: String }
pub struct DiversityConfig { pub redundancy_weight: f32, pub uncovered_weight: f32, pub max_per_domain: usize }
pub fn diversify(candidates: Vec<Scored>, k: usize, cfg: &DiversityConfig) -> Vec<Scored>;
```

### `coord` (§18) — the coordinator loop

```rust
/// §12 — the least-costly decision. Implemented by the caller (rules, then a small model);
/// the crate ships `RuleSelector` (deterministic) for tests.
pub trait Selector: Send + Sync {
    fn choose(&self, actions: &[Action], state: &EngineState) -> Option<usize>;
}
pub enum Action { Widen { lane: LaneId }, Expand { unit_id: String }, Refresh { source_id: String }, Abstain }

/// §22.1 — the answer generator, injected; `None` in lexical mode.
pub trait Answerer: Send + Sync {
    fn answer(&self, query: &str, evidence: &[EvidenceRecord], passages: &[String]) -> Result<String, String>;
}

pub struct Deps<'a> { pub lanes: &'a [&'a dyn search_retrieval::LaneSearcher], pub selector: &'a dyn Selector, pub answerer: Option<&'a dyn Answerer>, pub aliases: &'a AliasPriors, pub stats: IndexStats, pub now: i64 }

pub struct SearchRequest { pub query: String, pub limit: usize, pub mode: Mode, pub deadline_ms: u64, pub tz_offset_min: i32 }
pub struct SearchResponse { pub results: Vec<Result_>, pub coverage: Coverage, pub stop_reason: StopReason, pub trace: Vec<String> }
pub struct Result_ { pub doc_id: String, pub score: f64, pub lane_ranks: std::collections::BTreeMap<LaneId, usize>, pub unit_support: Vec<String> }

/// The §18 loop: analyze -> plan -> launch -> merge -> fuse -> rank -> evidence -> stop? ->
/// choose action -> revalidate -> launch. A request with no neural lane must still return its
/// best eligible lexical results; no wait may extend past the deadline.
pub fn run_search(req: SearchRequest, deps: &Deps<'_>) -> SearchResponse;
```

## Exit gate

- `cargo test -p search-engine` green, hermetic, ≥25 tests covering §6–§18.
- `search-api` can serve `POST /search` through `run_search` with a `RuleSelector` and a
  lexical-only lane set, returning per-result lane ranks and unit support.
- The workspace stays green (`cargo test --workspace`).
