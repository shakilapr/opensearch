//! Engine adapter: drive the §6–§18 `search-engine` core over the API's
//! [`Searcher`] and expose the per-signal provenance the UI's "Why this
//! result?" panel needs.
//!
//! This module is the only place that adapts the API's tiny [`Searcher`] trait
//! (see [`crate::query`]) onto `search_retrieval::LaneSearcher`, so the engine —
//! which was built to sit *below* `search-api` — never learns about `IndexHit`
//! or `SearchFilters`.
//!
//! Determinism/clock rule from the engine spec (`docs/ENGINE_SPEC.md`, ground
//! rules): the engine never reads a clock. The HTTP boundary computes `now` from
//! `SystemTime::now()` and passes it in; `now_ms` here is that value in **unix
//! seconds** (the engine's `Deps.now` unit), despite the historical `_ms` name.
//!
//! No `unwrap`/`expect`/`panic!` on any request path.

use crate::query::{
    analyze, build_signals, normalize_limit, IndexHit, Searcher, GRAPH_VERSION, MAX_LIMIT,
    QUERY_PLAN_VERSION, RANKING_POLICY_VERSION,
};
use search_core::{AppError, AppResult, QueryAnalysis, SearchFilters, SearchRequest, SearchResult};
use search_engine::{
    AliasPriors, Answerer, Deps, IndexStats, Mode as EngineCoreMode, StopReason,
};
use search_engine::evidence::EvidenceVerifier;
// `RuleSelector` ships in the crate's `coord` module but is not re-exported at
// the crate root; reach it through its module path (the engine crate is frozen
// and not owned here).
use search_engine::coord::RuleSelector;
use search_retrieval::{Candidate, Lane, LaneSearcher, RetrievalError};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Instant;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// Search mode requested on the wire (`"fast"` | `"auto"` | `"deep"`), defaulting
/// to `Auto` when absent. A distinct type from the engine's own `Mode` so the
/// crate boundary stays explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineMode {
    Fast,
    #[default]
    Auto,
    Deep,
}

impl EngineMode {
    /// Map the wire mode onto the engine core's mode.
    pub fn to_core(self) -> EngineCoreMode {
        match self {
            EngineMode::Fast => EngineCoreMode::Fast,
            EngineMode::Auto => EngineCoreMode::Auto,
            EngineMode::Deep => EngineCoreMode::Deep,
        }
    }
}

/// `POST /search` request used when the engine route is enabled.
///
/// `base` is the frozen [`SearchRequest`] **flattened** into the same JSON object
/// (so the existing body keeps working unchanged); `engine`/`mode` are additive
/// fields the legacy [`crate::query::execute`] path ignores.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineSearchRequest {
    #[serde(flatten)]
    pub base: SearchRequest,
    /// Opt-in to the §6–§18 pipeline. Absent/false ⇒ bit-for-bit legacy behaviour.
    #[serde(default)]
    pub engine: bool,
    /// Requested mode; defaults to [`EngineMode::Auto`].
    #[serde(default)]
    pub mode: EngineMode,
}

/// One engine result: the frozen [`SearchResult`] fields (flattened, so the wire
/// shape is unchanged) plus the additive per-signal provenance the engine
/// produces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineResult {
    #[serde(flatten)]
    pub base: SearchResult,
    /// `lane name -> best rank` in that lane (`bm25` / `source_first`).
    pub lane_ranks: BTreeMap<String, usize>,
    /// Query units this document supports (e.g. `["U1"]`).
    pub unit_support: Vec<String>,
}

/// Serializable mirror of the engine's `Coverage` (§16).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineCoverage {
    pub coverage: f32,
    pub mandatory_ok: bool,
    pub unsupported: Vec<String>,
    pub contradicted_units: Vec<String>,
}

/// `POST /search` response when `engine: true`: all of the legacy response
/// fields, plus the engine's coverage/stop-reason/trace/answer provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineSearchResponse {
    pub query_analysis: QueryAnalysis,
    pub results: Vec<EngineResult>,
    pub timing_ms: u64,
    pub index_manifest_id: String,
    pub overlay_watermark: u64,
    pub graph_version: String,
    pub ranking_policy_version: String,
    pub query_plan_version: String,
    pub snapshot_id: String,
    pub complete: bool,
    pub degradations: Vec<String>,
    /// Always `true` on this shape.
    pub engine: bool,
    pub coverage: EngineCoverage,
    pub stop_reason: String,
    pub trace: Vec<String>,
    pub answer: Option<String>,
}

/// Stable lowercase wire name for a lane (§9). Used for `lane_ranks` keys —
/// never the Rust `Debug` form.
pub fn lane_name(lane: Lane) -> &'static str {
    match lane {
        Lane::Bm25 => "bm25",
        Lane::SourceFirst => "source_first",
        Lane::Dense => "dense",
        Lane::Graph => "graph",
    }
}

/// Stable lowercase wire name for the engine's stop reason (§17).
pub fn stop_reason_name(reason: StopReason) -> &'static str {
    match reason {
        StopReason::LimitReached => "limit_reached",
        StopReason::Deadline => "deadline",
        StopReason::BudgetExhausted => "budget_exhausted",
        StopReason::SufficientCoverage => "sufficient_coverage",
        StopReason::NoAdmissibleAction => "no_admissible_action",
        StopReason::Cancelled => "cancelled",
        StopReason::Plateau => "plateau",
    }
}

// ---------------------------------------------------------------------------
// Lane adapter
// ---------------------------------------------------------------------------

/// Adapts the API's [`Searcher`] onto a single `search_retrieval::LaneSearcher`
/// stamped with a fixed [`Lane`].
///
/// `source_id` filtering: the third argument is **pushed into** [`SearchFilters`]
/// (the API's `Searcher` already understands `filters.source_id`; the real
/// `IndexerSearcher` pushes it into the index query). Because a `Searcher` MAY
/// ignore filters, the returned hits are **also** post-filtered by `source_id`
/// as a correctness backstop — a hit whose `source_id` is unknown is treated as
/// unmatched, mirroring `query::passes_filters`.
///
/// Every returned hit is retained in a shared `doc_id -> IndexHit` map so the
/// caller can re-hydrate the engine's doc ids into full results (see
/// [`run_engine_search`]).
///
/// The two index-backed lanes (bm25 and source_first) wrap the *same* [`Searcher`] and,
/// absent source routing, are handed the same `(query, limit, source)` on every
/// coordinator round. Left uncached, each round therefore issues the identical index scan
/// twice — measured as the dominant cost of the whole request. A request-scoped cache
/// keyed on the exact arguments makes the second lane reuse the first lane's hits. The
/// candidates each lane returns are unchanged (the lane stamp is applied after the cache),
/// so fusion, provenance, and ranking are byte-identical.
pub struct SearcherLane<'a, S: Searcher + ?Sized> {
    searcher: &'a S,
    lane: Lane,
    retained: Arc<Mutex<HashMap<String, IndexHit>>>,
    cache: SearchCache,
}

/// Cache key: the exact arguments that determine the underlying index result.
type SearchKey = (String, usize, Option<String>);
/// Request-scoped memo of successful `Searcher::search` calls, shared by the lanes.
pub type SearchCache = Arc<Mutex<HashMap<SearchKey, Arc<Vec<IndexHit>>>>>;

// ---------------------------------------------------------------------------
// Diagnostic gauges
// ---------------------------------------------------------------------------

thread_local! {
    /// Diagnostic scalars for the current query — the candidate limit actually passed to
    /// `Searcher::search`, the base segment count — as `(name, value)` in first-seen order.
    ///
    /// Populated only while `search_engine::timing` is enabled, so a normal request is
    /// byte-identical to before (the same determinism rule the engine's timing module uses).
    static GAUGES: RefCell<Vec<(&'static str, u64)>> = const { RefCell::new(Vec::new()) };
}

/// Record one diagnostic scalar for the current query. No-op unless engine timing is on.
pub fn record_gauge(name: &'static str, value: u64) {
    if search_engine::timing::enabled() {
        GAUGES.with(|g| g.borrow_mut().push((name, value)));
    }
}

/// Drain the accumulated `(name, value)` gauges for the current query.
pub fn take_gauges() -> Vec<(&'static str, u64)> {
    GAUGES.with(|g| std::mem::take(&mut *g.borrow_mut()))
}

impl<'a, S: Searcher + ?Sized> SearcherLane<'a, S> {
    /// A lane adapter stamping every candidate with `lane`, with its own (unshared) cache.
    pub fn new(searcher: &'a S, lane: Lane) -> Self {
        Self {
            searcher,
            lane,
            retained: Arc::new(Mutex::new(HashMap::new())),
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// A lane adapter sharing `cache` with the other lanes of the same request.
    pub fn with_cache(searcher: &'a S, lane: Lane, cache: SearchCache) -> Self {
        Self {
            searcher,
            lane,
            retained: Arc::new(Mutex::new(HashMap::new())),
            cache,
        }
    }

    /// The lane this adapter stamps onto its candidates.
    pub fn lane(&self) -> Lane {
        self.lane
    }

    /// A handle to the shared `doc_id -> IndexHit` map this lane populates.
    pub fn retained(&self) -> Arc<Mutex<HashMap<String, IndexHit>>> {
        Arc::clone(&self.retained)
    }

    /// The underlying hits for `(query, limit, source)`, reusing a prior lane's result when
    /// the same scan has already run this request. Errors are not cached.
    fn cached_hits(
        &self,
        query: &str,
        limit: usize,
        source_id: Option<&str>,
    ) -> Result<Arc<Vec<IndexHit>>, RetrievalError> {
        let key: SearchKey = (query.to_string(), limit, source_id.map(str::to_string));
        if let Ok(guard) = self.cache.lock() {
            if let Some(hits) = guard.get(&key) {
                search_engine::timing::count("cache_hits");
                return Ok(Arc::clone(hits));
            }
        }
        search_engine::timing::count("index_scans");
        // Attribute the scan: the exact candidate limit handed to the index. The engine's
        // per-lane breadth (up to 200 for Auto) is what the index scan cost scales with, so
        // recording it here is what turns "one scan is 426.9 ms" into a cause.
        record_gauge("index_scan_limit", limit as u64);
        let filters = source_id.map(|s| SearchFilters {
            source_id: Some(s.to_string()),
            ..Default::default()
        });
        let hits = self
            .searcher
            .search(query, limit, filters.as_ref())
            .map_err(RetrievalError::index)?;
        let arc = Arc::new(hits);
        if let Ok(mut guard) = self.cache.lock() {
            guard.entry(key).or_insert_with(|| Arc::clone(&arc));
        }
        Ok(arc)
    }
}

impl<S: Searcher + ?Sized> LaneSearcher for SearcherLane<'_, S> {
    fn search(
        &self,
        query: &str,
        limit: usize,
        source_id: Option<&str>,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        let hits = self.cached_hits(query, limit, source_id)?;

        let mut out: Vec<Candidate> = Vec::with_capacity(hits.len());
        // Retain every returned hit so the caller can hydrate results, even the
        // ones dropped by the source backstop below (a superset is harmless).
        if let Ok(mut map) = self.retained.lock() {
            for h in hits.iter() {
                map.entry(h.doc_id.clone()).or_insert_with(|| h.clone());
            }
        }
        for h in hits.iter() {
            if let Some(src) = source_id {
                if h.source_id.as_deref() != Some(src) {
                    continue;
                }
            }
            out.push(Candidate {
                doc_id: h.doc_id.clone(),
                score: h.score,
                lane: self.lane,
                source_id: h.source_id.clone(),
                title: Some(h.title.clone()),
                url: Some(h.url.clone()),
            });
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Passage text for the answerer
// ---------------------------------------------------------------------------

/// Wraps the injected [`Answerer`] so it receives real passage **text**.
///
/// The engine is hermetic: it knows a result only by `doc_id`, so the `passages`
/// it hands the answerer are document ids, and an LLM given ids alone cannot
/// ground anything (a live run answered "the evidence contains no content—only
/// passage identifiers").
///
/// Resolution per id, best first:
/// 1. the lane-retained hit's **query-relevant fragment** (`snippet`), prefixed
///    with the title — the index built it around the query terms, so it is the
///    page's answer-bearing text rather than its navigation block;
/// 2. an **excerpt of the stored body** centred on the first query-term match
///    (falling back to the head when no term is found) — the real content;
/// 3. the id itself, so a missing passage is visible as a missing passage and
///    never invented.
pub struct SnippetAnswerer<'a, A: Answerer + ?Sized> {
    inner: &'a A,
    searcher: &'a dyn Searcher,
    retained: Vec<Arc<Mutex<HashMap<String, IndexHit>>>>,
}

/// Characters kept from one document for the answer prompt (`search-answer`
/// truncates again to its own per-passage cap).
const MAX_PASSAGE_CHARS: usize = 1500;

/// Characters of leading context kept before the first query-term match.
const PASSAGE_LEAD_CHARS: usize = 200;

impl<'a, A: Answerer + ?Sized> SnippetAnswerer<'a, A> {
    /// Wrap `inner`, resolving passages against the index and the lanes' hits.
    pub fn new(
        inner: &'a A,
        searcher: &'a dyn Searcher,
        retained: Vec<Arc<Mutex<HashMap<String, IndexHit>>>>,
    ) -> Self {
        Self {
            inner,
            searcher,
            retained,
        }
    }

    /// The passage text for one doc id (see the type docs for the order).
    fn passage(&self, doc_id: &str, terms: &[String]) -> String {
        let (title, snippet) = self.retained_hit(doc_id);
        let mut text = String::new();
        if !title.trim().is_empty() {
            text.push_str(title.trim());
            text.push_str(". ");
        }
        if !snippet.trim().is_empty() {
            text.push_str(snippet.trim());
        }
        // Add the page's own text around the query terms when it still fits: a
        // 300-character fragment can be a link list on a documentation site,
        // and the answerer needs something it can actually quote.
        if let Ok(Some(body)) = self.searcher.doc_text(doc_id) {
            let trimmed = body.trim();
            if !trimmed.is_empty() {
                let used = text.chars().count();
                if used < MAX_PASSAGE_CHARS {
                    let excerpt = excerpt_around(trimmed, terms, MAX_PASSAGE_CHARS - used);
                    if !excerpt.is_empty() && !text.contains(&excerpt) {
                        if !text.is_empty() {
                            text.push_str(" … ");
                        }
                        text.push_str(&excerpt);
                    }
                }
            }
        }
        if text.trim().is_empty() {
            // Nothing was retained and the index has no body: show the id, so a
            // missing passage stays visible instead of being invented.
            text.push_str(doc_id);
        }
        text
    }

    /// The retained title and query-relevant fragment for a doc id, when a lane
    /// kept the hit.
    fn retained_hit(&self, doc_id: &str) -> (String, String) {
        for map in &self.retained {
            let Ok(guard) = map.lock() else { continue };
            let Some(hit) = guard.get(doc_id) else { continue };
            return (hit.title.clone(), hit.snippet.clone());
        }
        (String::new(), String::new())
    }
}

/// A bounded excerpt of `body` centred on the first `terms` occurrence.
///
/// The head of a scraped page is navigation, so the excerpt starts
/// [`PASSAGE_LEAD_CHARS`] before the first match and runs to
/// `max_chars`. With no match at all the head is used (better than nothing, and
/// honest about what it is).
fn excerpt_around(body: &str, terms: &[String], max_chars: usize) -> String {
    let hit = first_term_char_index(body, terms);
    let start = match hit {
        Some(i) => i.saturating_sub(PASSAGE_LEAD_CHARS),
        None => 0,
    };
    let mut out = String::new();
    for c in body.chars().skip(start).take(max_chars) {
        out.push(c);
    }
    out.trim().to_string()
}

/// Character index of the first occurrence of any term (lowercased,
/// alphanumeric-trimmed) in `body`.
fn first_term_char_index(body: &str, terms: &[String]) -> Option<usize> {
    let lower = body.to_lowercase();
    terms
        .iter()
        .filter_map(|t| lower.find(t.as_str()).map(|byte| lower[..byte].chars().count()))
        .min()
}

impl<A: Answerer + ?Sized> Answerer for SnippetAnswerer<'_, A> {
    fn answer(
        &self,
        query: &str,
        evidence: &[search_engine::evidence::EvidenceRecord],
        passages: &[String],
    ) -> Result<String, String> {
        let terms = search_core::text::content_tokens(query);
        let resolved: Vec<String> = passages.iter().map(|id| self.passage(id, &terms)).collect();
        self.inner.answer(query, evidence, &resolved)
    }
}

// ---------------------------------------------------------------------------
// §16 evidence verification against real content
// ---------------------------------------------------------------------------

/// [`EvidenceVerifier`] over the retrieved documents' stored text.
///
/// The engine is hermetic and can only compare a unit against a document
/// *title*, which is why a real query like "retrieval augmented generation"
/// scored zero coverage against 115 matching documents. This implementation has
/// the text the engine does not: it normalises both sides to alphanumeric
/// tokens and requires every unit token to be present in the document's stored
/// body (falling back to the lane-retained title+snippet when the index cannot
/// serve the body).
///
/// Bounded per call: one stored-body read, then a token-set comparison.
pub struct TextVerifier<'a> {
    searcher: &'a dyn Searcher,
    retained: Vec<Arc<Mutex<HashMap<String, IndexHit>>>>,
    cache: DocCache,
}

impl<'a> TextVerifier<'a> {
    /// A verifier reading `searcher`, with the lanes' retained hits as fallback.
    pub fn new(searcher: &'a dyn Searcher, retained: Vec<Arc<Mutex<HashMap<String, IndexHit>>>>) -> Self {
        Self {
            searcher,
            retained,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// The best text this layer can obtain for a document id.
    fn text_of(&self, doc_id: &str) -> String {
        if let Ok(Some(body)) = self.searcher.doc_text(doc_id) {
            if !body.trim().is_empty() {
                return body;
            }
        }
        for map in &self.retained {
            let Ok(guard) = map.lock() else { continue };
            if let Some(hit) = guard.get(doc_id) {
                return format!("{} {}", hit.title, hit.snippet);
            }
        }
        String::new()
    }
}

/// Maximum document tokens considered by the proximity check. Support for a
/// unit is a *local* property: a term pair 40 KB apart in a scraped page is a
/// coincidence, not evidence, so the scan is bounded and locality is required.
const VERIFY_SCAN_TOKENS: usize = 8_000;

/// Largest token distance that still counts as "the unit is stated here".
const PROXIMITY_WINDOW: usize = 80;

/// Documents whose tokenised body has already been computed, so a unit×candidate
/// sweep tokenises each document once.
type DocCache = Mutex<HashMap<String, Arc<Vec<String>>>>;

/// Cached tokenisation of a document's best available text.
fn doc_tokens(cache: &DocCache, text: &str, doc_id: &str) -> Arc<Vec<String>> {
    if let Ok(guard) = cache.lock() {
        if let Some(v) = guard.get(doc_id) {
            return Arc::clone(v);
        }
    }
    let mut v: Vec<String> = Vec::new();
    for tok in text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
    {
        v.push(tok.to_string());
        if v.len() >= VERIFY_SCAN_TOKENS {
            break;
        }
    }
    let arc = Arc::new(v);
    if let Ok(mut guard) = cache.lock() {
        if guard.len() < 128 {
            guard.insert(doc_id.to_string(), Arc::clone(&arc));
        }
    }
    arc
}

/// True when every `terms` entry occurs inside one window of
/// [`PROXIMITY_WINDOW`] tokens. Equivalent to "the page states this unit",
/// whereas mere presence anywhere in a 100 KB page means almost nothing.
///
/// Positions are capped per term so the sweep stays bounded on pathological
/// documents (a page repeating one word thousands of times).
fn window_contains_all(doc: &[String], terms: &[String]) -> bool {
    const MAX_POS: usize = 2_000;
    if terms.len() == 1 {
        return doc.iter().any(|t| t == &terms[0]);
    }
    // positions per term, in document order
    let mut positions: Vec<Vec<usize>> = vec![Vec::new(); terms.len()];
    let mut seen = 0usize;
    for (i, tok) in doc.iter().enumerate() {
        for (ti, term) in terms.iter().enumerate() {
            if tok == term {
                if positions[ti].len() < MAX_POS {
                    positions[ti].push(i);
                    seen += 1;
                }
            }
        }
        if seen >= MAX_POS * terms.len() {
            break;
        }
    }
    if positions.iter().any(|p| p.is_empty()) {
        return false;
    }
    // Merge into one ascending event stream and slide a window over it.
    let mut events: Vec<(usize, usize)> = Vec::new();
    for (ti, pos) in positions.iter().enumerate() {
        for p in pos {
            events.push((*p, ti));
        }
    }
    events.sort_unstable();
    let mut counts = vec![0usize; terms.len()];
    let mut have = 0usize;
    let mut left = 0usize;
    for right in 0..events.len() {
        let (pos_right, ti) = events[right];
        counts[ti] += 1;
        if counts[ti] == 1 {
            have += 1;
        }
        while have == terms.len() {
            let (pos_left, tl) = events[left];
            if pos_right - pos_left <= PROXIMITY_WINDOW {
                return true;
            }
            counts[tl] -= 1;
            if counts[tl] == 0 {
                have -= 1;
            }
            left += 1;
        }
    }
    false
}

impl EvidenceVerifier for TextVerifier<'_> {
    fn supports(&self, unit_text: &str, doc_id: &str) -> bool {
        let unit = search_core::text::content_tokens(unit_text);
        if unit.is_empty() {
            return false;
        }
        let text = self.text_of(doc_id);
        if text.trim().is_empty() {
            return false;
        }
        let doc = doc_tokens(&self.cache, &text, doc_id);
        window_contains_all(&doc, &unit)
    }
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

/// Run the §6–§18 pipeline over the real index and map the engine's result rows
/// back onto full API results.
///
/// Hydration choice: the lane adapters retain every `IndexHit` they see in a
/// shared `doc_id -> IndexHit` map; after the run the engine's doc ids are looked
/// up in that map. As a defensive fallback for any id the lanes did not retain,
/// one `Searcher::search` re-fetch is issued (never per-result). Rows that still
/// cannot be hydrated are dropped rather than fabricated.
/// Extra lanes injected by the caller (e.g. the dense lane over a vector store).
///
/// The engine fuses whatever lanes it is given by rank, so adding one is a wiring decision, not
/// an engine change: the lane carries its own [`search_retrieval::Lane`] id and therefore its own
/// name in `lane_ranks`.
pub type ExtraLanes = [Arc<dyn search_retrieval::LaneSearcher + Send + Sync>];

/// Documents to ask an expanding lane for when building provenance. Generous on purpose: a document
/// only needs to be *in* this set to be reported as graph-reached, and the lane is cheap (a bounded
/// walk), so the cost of a large probe is a few milliseconds against a mislabelled signal.
const GRAPH_EXPANSION_PROBE: usize = 200;

pub fn run_engine_search(
    searcher: &dyn Searcher,
    req: EngineSearchRequest,
    now_ms: i64,
    answerer: Option<&dyn Answerer>,
    planner: Option<&dyn search_engine::QueryPlanner>,
    extra_lanes: &ExtraLanes,
) -> AppResult<EngineSearchResponse> {
    let started = Instant::now();
    // Diagnostic timer: everything before the engine loop (graph probe, manifest, lane wiring).
    let t_api_pre = search_engine::timing::start();

    let query = req.base.query.trim().to_string();
    if query.is_empty() {
        return Err(AppError::Invalid("query must not be empty".into()));
    }

    // Which documents did graph expansion reach for this query? Fusion keeps one lane per document,
    // so a graph-reached page that also matched lexically is labelled lexical-only; asking the lane
    // directly is what keeps `graph_expansion` an observation rather than a constant.
    let graph_docs: std::collections::HashSet<String> = extra_lanes
        .iter()
        .filter_map(|lane| lane.as_expanding())
        .flat_map(|lane| lane.expanded_docs(&query, GRAPH_EXPANSION_PROBE))
        .collect();
    let limit = normalize_limit(req.base.limit)?;

    let manifest = searcher.manifest()?;
    // Attribution: the index layout the scan runs against. A single-segment base is what
    // blocks Tantivy's multi-segment executor path, so the count belongs next to the limit.
    record_gauge("base_segments", manifest.base_segments);
    record_gauge("index_docs", manifest.doc_count);

    // §8/§17 stats the planner conditions on. `avg_doc_len` is not exposed by the
    // API's manifest, so it is reported as `0.0` (unknown) rather than invented.
    let stats = IndexStats {
        docs: manifest.doc_count,
        avg_doc_len: 0.0,
        updated_at: manifest.created_at,
    };

    // Two lane adapters: the global lexical lane and the source-first
    // accelerator (both stamped so per-result `lane_ranks` is meaningful). They wrap the
    // same searcher and are issued the same query each round, so they share one
    // request-scoped cache: the second lane reuses the first's hits instead of
    // repeating the index scan (see `SearcherLane`).
    let search_cache: SearchCache = Arc::new(Mutex::new(HashMap::new()));
    let lane_bm25 = SearcherLane::with_cache(searcher, Lane::Bm25, Arc::clone(&search_cache));
    let lane_source = SearcherLane::with_cache(searcher, Lane::SourceFirst, Arc::clone(&search_cache));
    let retained_bm25 = lane_bm25.retained();
    let retained_source = lane_source.retained();
    // The two index-backed lanes always run; any injected lane (today: the dense lane over the
    // vector store) is fused alongside them and named in `lane_ranks` by its own lane id.
    // §17's fast budget is 200 ms, so fast mode runs only what pays for itself. Measured on the
    // 600-case corpus set: the graph lane adds +261 ms p50 and moves nDCG by -0.4 pt (886 -> 1147 ms,
    // 0.4913 -> 0.4871), so it is not run in fast mode. The dense lane costs +9 ms and is kept.
    // The discriminator is the lane's own provenance view — only the §9.4 graph lane exposes one —
    // so this needs no lane registry and cannot accidentally drop another lane.
    let fast_mode = matches!(req.mode, EngineMode::Fast);
    let graph_skipped = fast_mode && extra_lanes.iter().any(|l| l.as_expanding().is_some());
    let mut lanes: Vec<&dyn LaneSearcher> = vec![&lane_bm25, &lane_source];
    for lane in extra_lanes.iter() {
        if fast_mode && lane.as_expanding().is_some() {
            continue;
        }
        lanes.push(&**lane as &dyn LaneSearcher);
    }

    let aliases = AliasPriors::empty();
    let selector = RuleSelector::new();
    let mode = req.mode.to_core();
    // §17: a client deadline may be tighter than the mode default; otherwise use
    // the mode default so the budget is never zero (which would stop instantly).
    let deadline_ms = req
        .base
        .deadline_ms
        .unwrap_or_else(|| search_engine::plan::mode_deadline_ms(mode));

    let core_req = search_engine::SearchRequest {
        query: query.clone(),
        limit,
        mode,
        deadline_ms,
        // The frozen `SearchRequest.timezone` is a name; without a full tz
        // database the API passes UTC (offset 0). Relative-date parsing then
        // resolves against `now` in UTC, matching the legacy path's default.
        tz_offset_min: 0,
    };

    // Passage-text adapter: see [`SnippetAnswerer`]. Built from the same shared
    // maps the lanes populate, so it is read only after they have run.
    let retained_maps = vec![Arc::clone(&retained_bm25), Arc::clone(&retained_source)];
    let wrapped = answerer.map(|a| SnippetAnswerer::new(a, searcher, retained_maps.clone()));
    // §16: verify units against real document text, not just titles.
    let verifier = TextVerifier::new(searcher, retained_maps);

    let deps = Deps {
        lanes: &lanes,
        selector: &selector,
        verifier: Some(&verifier),
        // The engine hands the answerer doc ids (§ hermetic); resolve them to
        // real title+snippet text from the hits the lanes just retained.
        answerer: wrapped.as_ref().map(|w| w as &dyn Answerer),
        aliases: &aliases,
        stats,
        now: now_ms,
        planner,
    };

    search_engine::timing::record("api_pre", t_api_pre);
    let engine_resp = search_engine::run_search(core_req, &deps);
    // Diagnostic timer: hydrating doc ids, re-analysing for signals, and assembling the response.
    let t_api_assembly = search_engine::timing::start();

    // ---- Hydrate the engine's doc ids back onto real hits. ----
    let mut docmap: HashMap<String, IndexHit> = HashMap::new();
    for arc in [retained_bm25, retained_source] {
        if let Ok(map) = arc.lock() {
            for (k, v) in map.iter() {
                docmap.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
    }
    let any_missing = engine_resp
        .results
        .iter()
        .any(|r| !docmap.contains_key(&r.doc_id));
    if any_missing {
        if let Ok(hits) = searcher.search(&query, MAX_LIMIT, None) {
            for h in hits {
                docmap.entry(h.doc_id.clone()).or_insert(h);
            }
        }
    }

    let analysis = analyze(&query);
    let entities = analysis.entities.clone();

    let results: Vec<EngineResult> = engine_resp
        .results
        .iter()
        .filter_map(|row| {
            let hit = docmap.get(&row.doc_id)?;
            let signals = build_signals(hit, &entities, &graph_docs);
            let mut base = SearchResult::from(hit.clone());
            base.signals = signals;
            base.score = row.score as f32;
            let lane_ranks: BTreeMap<String, usize> = row
                .lane_ranks
                .iter()
                .map(|(lane, rank)| (lane_name(*lane).to_string(), *rank))
                .collect();
            Some(EngineResult {
                base,
                lane_ranks,
                unit_support: row.unit_support.clone(),
            })
        })
        .collect();

    // Degradations and reproducibility tuple, mirroring the legacy path.
    let mut degradations = searcher.degradations();
    if manifest.generation == 0 && !degradations.iter().any(|d| d == "index_unpublished") {
        degradations.push("index_unpublished".to_string());
    }
    let snapshot_id = snapshot_id(
        &manifest.manifest_id,
        manifest.watermark,
        GRAPH_VERSION,
        RANKING_POLICY_VERSION,
        QUERY_PLAN_VERSION,
    );

    let cov = &engine_resp.coverage;
    let coverage = EngineCoverage {
        coverage: cov.coverage,
        mandatory_ok: cov.mandatory_ok,
        unsupported: cov.unsupported.clone(),
        contradicted_units: cov.contradicted_units.clone(),
    };

    search_engine::timing::record("api_assembly", t_api_assembly);
    Ok(EngineSearchResponse {
        query_analysis: analysis,
        results,
        timing_ms: started.elapsed().as_millis() as u64,
        index_manifest_id: manifest.manifest_id,
        overlay_watermark: manifest.watermark,
        graph_version: GRAPH_VERSION.to_string(),
        ranking_policy_version: RANKING_POLICY_VERSION.to_string(),
        query_plan_version: QUERY_PLAN_VERSION.to_string(),
        snapshot_id,
        complete: degradations.is_empty(),
        degradations,
        engine: true,
        coverage,
        stop_reason: stop_reason_name(engine_resp.stop_reason).to_string(),
        trace: {
            // Auditable: how many documents graph expansion reached for this query, so a reader of a
            // trace can tell "the graph had nothing" from "the graph lane is not wired".
            let mut trace = engine_resp.trace;
            if graph_skipped {
                trace.push(
                    "graph lane: skipped in fast mode (§17 budget; measured +261 ms p50 for no nDCG \
                     gain on the 600-case set)"
                        .to_string(),
                );
            }
            trace.push(format!(
                "provenance: graph expansion reached {} document(s)",
                graph_docs.len()
            ));
            trace
        },
        answer: engine_resp.answer,
    })
}

/// Deterministic FNV-1a 64-bit hash over the reproducibility tuple.
///
/// Mirrors `query::snapshot_id` (which is private to `query.rs` and out of this
/// agent's ownership) so the engine response carries the same reproducible id.
fn snapshot_id(
    manifest_id: &str,
    watermark: u64,
    graph_version: &str,
    ranking_policy_version: &str,
    query_plan_version: &str,
) -> String {
    let mut acc: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |bytes: &[u8]| {
        for b in bytes {
            acc ^= *b as u64;
            acc = acc.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    mix(manifest_id.as_bytes());
    mix(&watermark.to_le_bytes());
    mix(graph_version.as_bytes());
    mix(ranking_policy_version.as_bytes());
    mix(query_plan_version.as_bytes());
    format!("{acc:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<String> {
        s.split_whitespace().map(|t| t.to_string()).collect()
    }

    #[test]
    fn proximity_accepts_a_locally_stated_unit() {
        let doc = toks("retrieval augmented generation grounds the model");
        assert!(window_contains_all(&doc, &toks("retrieval augmented generation")));
    }

    #[test]
    fn proximity_rejects_terms_scattered_across_a_long_page() {
        // All three terms are present, but never together: that is a
        // coincidence on a scraped page, not support.
        let mut parts = vec!["retrieval".to_string()];
        parts.extend(std::iter::repeat("filler".to_string()).take(200));
        parts.push("augmented".to_string());
        parts.extend(std::iter::repeat("filler".to_string()).take(200));
        parts.push("generation".to_string());
        assert!(!window_contains_all(&parts, &toks("retrieval augmented generation")));
    }

    #[test]
    fn proximity_requires_every_term() {
        let doc = toks("retrieval augmented");
        assert!(!window_contains_all(&doc, &toks("retrieval augmented generation")));
    }

    #[test]
    fn single_term_units_are_presence_checks() {
        let mut parts = vec!["transformer".to_string()];
        parts.extend(std::iter::repeat("filler".to_string()).take(500));
        assert!(window_contains_all(&parts, &toks("transformer")));
    }

    #[test]
    fn excerpt_starts_near_the_match_not_at_the_page_head() {
        let mut body = "nav home about contact ".repeat(40);
        body.push_str(" Paged Attention stores the KV cache in fixed-size blocks. ");
        let terms = search_core::text::content_tokens("paged attention");
        let out = excerpt_around(&body, &terms, MAX_PASSAGE_CHARS);
        assert!(out.contains("Paged Attention stores"), "{out}");
        assert!(out.chars().count() <= MAX_PASSAGE_CHARS, "bounded excerpt");
        // It must not begin at the navigation block.
        assert!(!out.starts_with("nav home about"), "{out}");
    }

    #[test]
    fn excerpt_falls_back_to_the_head_when_no_term_occurs() {
        let body = "alpha beta gamma ".repeat(200);
        let out = excerpt_around(&body, &["zzz".to_string()], 100);
        assert!(out.starts_with("alpha beta gamma"), "{out}");
        assert_eq!(out.chars().count(), 100);
    }

    #[test]
    fn document_tokenisation_is_bounded_and_cached() {
        let cache = Mutex::new(HashMap::new());
        let long = "word ".repeat(VERIFY_SCAN_TOKENS + 5_000);
        let a = doc_tokens(&cache, &long, "d1");
        assert_eq!(a.len(), VERIFY_SCAN_TOKENS);
        let b = doc_tokens(&cache, &long, "d1");
        assert!(Arc::ptr_eq(&a, &b), "second call must reuse the cached tokens");
    }

    #[test]
    fn identical_lane_searches_share_one_index_scan() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingSearcher {
            calls: AtomicUsize,
        }
        impl Searcher for CountingSearcher {
            fn search(
                &self,
                _q: &str,
                limit: usize,
                _f: Option<&SearchFilters>,
            ) -> AppResult<Vec<IndexHit>> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let _ = limit;
                Ok(vec![IndexHit {
                    doc_id: "d1".to_string(),
                    version: 1,
                    resource_id: "r1".to_string(),
                    work_id: None,
                    content_id: None,
                    url: "https://example.test/d1".to_string(),
                    title: "Doc One".to_string(),
                    snippet: "snippet".to_string(),
                    source_id: Some("news".to_string()),
                    doc_type: None,
                    language: None,
                    published_at: None,
                    modified_at: None,
                    fetched_at: 0,
                    lane: "lexical_base".to_string(),
                    provenance: search_core::Provenance::Snapshot,
                    score: 1.0,
                    signals: Vec::new(),
                }])
            }
        }

        let s = CountingSearcher { calls: AtomicUsize::new(0) };
        let cache: SearchCache = Arc::new(Mutex::new(HashMap::new()));
        let bm25 = SearcherLane::with_cache(&s, Lane::Bm25, Arc::clone(&cache));
        let source = SearcherLane::with_cache(&s, Lane::SourceFirst, Arc::clone(&cache));

        let a = bm25.search("rust async", 5, None).unwrap();
        // The second lane issues the identical scan: it must reuse the first's hits.
        let b = source.search("rust async", 5, None).unwrap();

        assert_eq!(s.calls.load(Ordering::SeqCst), 1, "identical scan runs once");
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
        assert_eq!(a[0].doc_id, b[0].doc_id);
        // Each lane still stamps its own lane id, so per-result provenance is preserved.
        assert_eq!(a[0].lane, Lane::Bm25);
        assert_eq!(b[0].lane, Lane::SourceFirst);

        // A different limit is a different scan.
        let _ = bm25.search("rust async", 6, None).unwrap();
        assert_eq!(s.calls.load(Ordering::SeqCst), 2);

        // Same query and limit but a source filter is a different scan.
        let _ = bm25.search("rust async", 5, Some("news")).unwrap();
        assert_eq!(s.calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn engine_records_scan_limit_and_segment_count_only_when_timing_is_enabled() {
        struct ManifestSearcher;
        impl Searcher for ManifestSearcher {
            fn search(
                &self,
                _q: &str,
                _limit: usize,
                _f: Option<&SearchFilters>,
            ) -> AppResult<Vec<IndexHit>> {
                Ok(vec![IndexHit {
                    doc_id: "d1".to_string(),
                    version: 1,
                    resource_id: "r1".to_string(),
                    work_id: None,
                    content_id: None,
                    url: "https://example.test/d1".to_string(),
                    title: "Doc One".to_string(),
                    snippet: "snippet".to_string(),
                    source_id: Some("news".to_string()),
                    doc_type: None,
                    language: None,
                    published_at: None,
                    modified_at: None,
                    fetched_at: 0,
                    lane: "lexical_base".to_string(),
                    provenance: search_core::Provenance::Snapshot,
                    score: 1.0,
                    signals: Vec::new(),
                }])
            }
            fn manifest(&self) -> AppResult<search_core::IndexManifest> {
                Ok(search_core::IndexManifest {
                    manifest_id: "gen-7".to_string(),
                    generation: 7,
                    doc_count: 235_204,
                    base_segments: 1,
                    watermark: 7,
                    embedding_model_ids: Vec::new(),
                    metadata_version: 7,
                    created_at: 0,
                    published_at: None,
                })
            }
        }

        let req = || EngineSearchRequest {
            base: SearchRequest {
                query: "rust async runtime".to_string(),
                limit: 10,
                offset: 0,
                timezone: "UTC".to_string(),
                deadline_ms: None,
                filters: None,
            },
            engine: true,
            mode: EngineMode::Auto,
        };
        let no_lanes: Vec<Arc<dyn search_retrieval::LaneSearcher + Send + Sync>> = Vec::new();

        // Timing off: the request path reads no clock and records nothing.
        search_engine::timing::disable();
        let _ = take_gauges();
        let _ = run_engine_search(&ManifestSearcher, req(), 0, None, None, &no_lanes).unwrap();
        assert!(take_gauges().is_empty(), "gauges are a no-op unless timing is enabled");

        // Timing on: the exact candidate limit and the index layout are recorded.
        search_engine::timing::enable();
        let _ = take_gauges();
        let _ = run_engine_search(&ManifestSearcher, req(), 0, None, None, &no_lanes).unwrap();
        let gauges = take_gauges();
        search_engine::timing::disable();

        let value = |name: &str| gauges.iter().find(|(n, _)| *n == name).map(|(_, v)| *v);
        assert_eq!(value("base_segments"), Some(1), "segment count is attributed");
        assert_eq!(value("index_docs"), Some(235_204));
        // Auto: base 100 + one per 100k docs (room 100) = 102 on this corpus.
        assert_eq!(
            value("index_scan_limit"),
            Some(102),
            "the engine's per-lane breadth is the candidate limit the scan ran with"
        );
    }
}
