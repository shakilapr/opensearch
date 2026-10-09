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
// `RuleSelector` ships in the crate's `coord` module but is not re-exported at
// the crate root; reach it through its module path (the engine crate is frozen
// and not owned here).
use search_engine::coord::RuleSelector;
use search_retrieval::{Candidate, Lane, LaneSearcher, RetrievalError};
use serde::{Deserialize, Serialize};
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
    }
}

/// Stable lowercase wire name for the engine's stop reason (§17).
pub fn stop_reason_name(reason: StopReason) -> &'static str {
    match reason {
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
pub struct SearcherLane<'a, S: Searcher + ?Sized> {
    searcher: &'a S,
    lane: Lane,
    retained: Arc<Mutex<HashMap<String, IndexHit>>>,
}

impl<'a, S: Searcher + ?Sized> SearcherLane<'a, S> {
    /// A lane adapter stamping every candidate with `lane`.
    pub fn new(searcher: &'a S, lane: Lane) -> Self {
        Self {
            searcher,
            lane,
            retained: Arc::new(Mutex::new(HashMap::new())),
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
}

impl<S: Searcher + ?Sized> LaneSearcher for SearcherLane<'_, S> {
    fn search(
        &self,
        query: &str,
        limit: usize,
        source_id: Option<&str>,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        let filters = source_id.map(|s| SearchFilters {
            source_id: Some(s.to_string()),
            ..Default::default()
        });
        let hits = self
            .searcher
            .search(query, limit, filters.as_ref())
            .map_err(RetrievalError::index)?;

        let mut out: Vec<Candidate> = Vec::with_capacity(hits.len());
        // Retain every returned hit so the caller can hydrate results, even the
        // ones dropped by the source backstop below (a superset is harmless).
        if let Ok(mut map) = self.retained.lock() {
            for h in &hits {
                map.entry(h.doc_id.clone()).or_insert_with(|| h.clone());
            }
        }
        for h in hits {
            if let Some(src) = source_id {
                if h.source_id.as_deref() != Some(src) {
                    continue;
                }
            }
            out.push(Candidate {
                doc_id: h.doc_id,
                score: h.score,
                lane: self.lane,
                source_id: h.source_id,
                title: Some(h.title),
                url: Some(h.url),
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
/// passage identifiers"). The lanes, however, retain the full `IndexHit` — title
/// and snippet — for exactly those ids. This adapter resolves each id to
/// "<title>. <snippet>" at answer time, which happens after the lanes have run.
///
/// A document whose retained hit carries no text resolves to its id, so the
/// answerer sees a missing passage rather than invented content.
pub struct SnippetAnswerer<'a, A: Answerer + ?Sized> {
    inner: &'a A,
    retained: Vec<Arc<Mutex<HashMap<String, IndexHit>>>>,
}

impl<'a, A: Answerer + ?Sized> SnippetAnswerer<'a, A> {
    /// Wrap `inner`, resolving passages against the lanes' retained hit maps.
    pub fn new(inner: &'a A, retained: Vec<Arc<Mutex<HashMap<String, IndexHit>>>>) -> Self {
        Self { inner, retained }
    }

    /// The passage text for one doc id: title + snippet from the first retained
    /// map that knows the id, or the id itself when no text was retained.
    fn passage(&self, doc_id: &str) -> String {
        for map in &self.retained {
            let Ok(guard) = map.lock() else { continue };
            let Some(hit) = guard.get(doc_id) else { continue };
            let mut text = String::with_capacity(hit.title.len() + hit.snippet.len() + 2);
            if !hit.title.trim().is_empty() {
                text.push_str(hit.title.trim());
                text.push_str(". ");
            }
            if !hit.snippet.trim().is_empty() {
                text.push_str(hit.snippet.trim());
            }
            if !text.trim().is_empty() {
                return text;
            }
        }
        doc_id.to_string()
    }
}

impl<A: Answerer + ?Sized> Answerer for SnippetAnswerer<'_, A> {
    fn answer(
        &self,
        query: &str,
        evidence: &[search_engine::evidence::EvidenceRecord],
        passages: &[String],
    ) -> Result<String, String> {
        let resolved: Vec<String> = passages.iter().map(|id| self.passage(id)).collect();
        self.inner.answer(query, evidence, &resolved)
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
pub fn run_engine_search(
    searcher: &dyn Searcher,
    req: EngineSearchRequest,
    now_ms: i64,
    answerer: Option<&dyn Answerer>,
) -> AppResult<EngineSearchResponse> {
    let started = Instant::now();

    let query = req.base.query.trim().to_string();
    if query.is_empty() {
        return Err(AppError::Invalid("query must not be empty".into()));
    }
    let limit = normalize_limit(req.base.limit)?;

    let manifest = searcher.manifest()?;

    // §8/§17 stats the planner conditions on. `avg_doc_len` is not exposed by the
    // API's manifest, so it is reported as `0.0` (unknown) rather than invented.
    let stats = IndexStats {
        docs: manifest.doc_count,
        avg_doc_len: 0.0,
        updated_at: manifest.created_at,
    };

    // Two lane adapters: the global lexical lane and the source-first
    // accelerator (both stamped so per-result `lane_ranks` is meaningful).
    let lane_bm25 = SearcherLane::new(searcher, Lane::Bm25);
    let lane_source = SearcherLane::new(searcher, Lane::SourceFirst);
    let retained_bm25 = lane_bm25.retained();
    let retained_source = lane_source.retained();
    let lanes: [&dyn LaneSearcher; 2] = [&lane_bm25, &lane_source];

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
    let wrapped = answerer.map(|a| SnippetAnswerer::new(a, retained_maps));

    let deps = Deps {
        lanes: &lanes,
        selector: &selector,
        // The engine hands the answerer doc ids (§ hermetic); resolve them to
        // real title+snippet text from the hits the lanes just retained.
        answerer: wrapped.as_ref().map(|w| w as &dyn Answerer),
        aliases: &aliases,
        stats,
        now: now_ms,
    };

    let engine_resp = search_engine::run_search(core_req, &deps);

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
            let signals = build_signals(hit, &entities);
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
        trace: engine_resp.trace,
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
