//! Stage-4 deep navigation: a real [`SearchExecutor`] over the pipeline's
//! index + retrieval lanes, the per-phase trace emitter, and the
//! [`Pipeline::deep_search`] entry point that drives [`search_nav::Navigator`].
//!
//! ## What this wires
//!
//! * [`PipelineExecutor`] implements `search_nav::SearchExecutor` against the
//!   pipeline's own [`Searcher`] (the source-first lane over the live Tantivy
//!   index). Candidates are derived from index hits; `Retrieve`, `FollowLink`,
//!   `Widen`, `Rerank`, `ExpandGraph` and `VerifyJoin` all run real index
//!   searches. `Refresh` performs **no** network I/O — the default build is
//!   hermetic and offline — and honestly records a no-op note.
//! * The navigator runs a policy chain of `RulesPolicy` + `AlgorithmPolicy`
//!   plus a counting LLM-tier *probe* that is only consulted when every cheaper
//!   tier abstained. A simple query never reaches it (see the fast-path test),
//!   so the model-call count stays **0** whenever rules/policy suffice
//!   (`docs/STAGE4_SPEC.md` exit gate).
//! * [`plan_budget`] gives a simple (short, single-entity) query a tight budget
//!   so it stays entirely in the fast path; a complex/multi-hop query scales
//!   with `max_hops` and may loop more.
//!
//! ## Trace emission (`PolicyTrace`)
//!
//! `docs/STAGE8_SPEC.md` freezes `search_trace::{Phase, TraceEvent, PolicyTrace}`.
//! The `search-trace` crate is still a stub at the time of writing, so — as the
//! integration brief permits — the emit call is defined behind the small local
//! trait [`PhaseEmitter`] (with [`NoopEmitter`] as the default). When
//! `search-trace` lands, an adapter `impl PhaseEmitter for search_trace::PolicyTrace`
//! (mapping [`PhaseEvent`] → `TraceEvent`) replaces the local types without
//! touching the navigation code.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::OptionalExtension;

use search_api::{DeepSearchResponse, DeepSearcher, DocResponse, IndexHit, Searcher};
use search_core::{AppError, AppResult};
use search_nav::{
    plan_budget, Action, AlgorithmPolicy, NavConfig, NavContext, NavError, NavOutcome, Navigator,
    PolicyTier, RulesPolicy, SearchAction, SearchExecutor, SearchPolicy, SearchState, StopReason,
};

use crate::pipeline::Pipeline;

// ---------------------------------------------------------------------------
// Trace emission (STAGE8 `PolicyTrace` interface, behind a local trait)
// ---------------------------------------------------------------------------

/// Phase of one navigation request (mirrors `search_trace::Phase`,
/// `docs/STAGE8_SPEC.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Query,
    Analyze,
    Route,
    Retrieve,
    Refresh,
    Rerank,
    Navigate,
    Answer,
}

impl Phase {
    /// Stable lowercase tag (matches the STAGE8 wire form).
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Query => "query",
            Phase::Analyze => "analyze",
            Phase::Route => "route",
            Phase::Retrieve => "retrieve",
            Phase::Refresh => "refresh",
            Phase::Rerank => "rerank",
            Phase::Navigate => "navigate",
            Phase::Answer => "answer",
        }
    }
}

/// One structured phase event (mirrors `search_trace::TraceEvent`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseEvent {
    pub ts_ms: u64,
    pub query_id: String,
    pub phase: Phase,
    pub action: String,
    pub detail: BTreeMap<String, String>,
    pub policy_tier: Option<String>,
    pub cost_ms: u64,
}

/// The one method a trace sink needs (mirrors `search_trace::PolicyTrace`).
pub trait PhaseEmitter: Send + Sync {
    fn emit(&self, ev: &PhaseEvent);
}

/// Discards every event; the default sink for the hermetic build.
pub struct NoopEmitter;

impl PhaseEmitter for NoopEmitter {
    fn emit(&self, _ev: &PhaseEvent) {}
}

/// A deterministic query id (no randomness, so traces are reproducible).
fn query_id(query: &str, now: i64) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    query.hash(&mut h);
    format!("q-{now}-{:016x}", h.finish())
}

// ---------------------------------------------------------------------------
// The executor
// ---------------------------------------------------------------------------

/// Hits accumulated across the run, shared out of the navigator (which owns the
/// boxed executor). Keyed by `doc_id`; `titles` powers `FollowLink`.
#[derive(Default)]
struct Retrieved {
    hits: HashMap<String, IndexHit>,
    titles: HashMap<String, String>,
}

impl Retrieved {
    fn record(&mut self, hit: &IndexHit) {
        if !hit.title.is_empty() {
            self.titles
                .entry(hit.doc_id.clone())
                .or_insert_with(|| hit.title.clone());
        }
        self.hits.entry(hit.doc_id.clone()).or_insert_with(|| hit.clone());
    }

    fn score_of(&self, doc_id: &str) -> f32 {
        self.hits.get(doc_id).map(|h| h.score).unwrap_or(0.0)
    }
}

/// A real [`SearchExecutor`] over the pipeline's index + retrieval lanes.
///
/// Candidates come from index hits: the initial `Retrieve` for the query, then
/// (only for a multi-hop query) `Widen` to broaden recall and `FollowLink` on
/// the current best documents to gather related evidence. A simple query is
/// marked solved immediately after its first retrieve, so it never widens.
pub struct PipelineExecutor {
    searcher: Arc<dyn Searcher + Send + Sync>,
    limit: usize,
    /// `false` for a simple query: the fast path (retrieve once, then stop).
    multihop: bool,
    shared: Arc<Mutex<Retrieved>>,
}

impl PipelineExecutor {
    fn new(
        searcher: Arc<dyn Searcher + Send + Sync>,
        limit: usize,
        multihop: bool,
        shared: Arc<Mutex<Retrieved>>,
    ) -> Self {
        Self {
            searcher,
            limit: limit.max(1),
            multihop,
            shared,
        }
    }

    /// Run one index search, record every hit, add fresh docs best-first to the
    /// state, and advance the virtual clock.
    fn gather(
        &self,
        query: &str,
        limit: usize,
        state: &mut SearchState,
        ctx: &mut NavContext,
    ) -> Result<usize, NavError> {
        let hits = self
            .searcher
            .search(query, limit.max(1), None)
            .map_err(|e| NavError::execution(e.to_string()))?;
        let mut added = 0usize;
        {
            let mut shared = self.shared.lock().expect("retrieved mutex poisoned");
            for h in &hits {
                shared.record(h);
            }
        }
        for h in &hits {
            if state.add_result(h.doc_id.clone()) {
                added += 1;
            }
        }
        // One action costs one virtual millisecond (hermetic; no real timers).
        ctx.charge_ms(1);
        Ok(added)
    }
}

impl SearchExecutor for PipelineExecutor {
    fn execute(
        &mut self,
        action: &Action,
        state: &mut SearchState,
        ctx: &mut NavContext,
    ) -> Result<(), NavError> {
        match action {
            Action::Retrieve { query } => {
                self.gather(query, self.limit, state, ctx)?;
                // A simple query is answered by the first retrieve: stay in the
                // fast path (`StopReason::Solved`, zero escalation).
                if !self.multihop {
                    ctx.mark_solved();
                }
            }
            Action::Widen => {
                // Broaden recall: rerun the query with a wider window.
                let wider = self.limit.saturating_mul(4).max(8);
                let query = state.query.clone();
                self.gather(&query, wider, state, ctx)?;
            }
            Action::FollowLink { doc_id } => {
                // Follow the document to its related evidence: search by its
                // title (the only relatedness signal the index exposes).
                let title = self
                    .shared
                    .lock()
                    .expect("retrieved mutex poisoned")
                    .titles
                    .get(doc_id)
                    .cloned()
                    .unwrap_or_else(|| doc_id.clone());
                self.gather(&title, self.limit, state, ctx)?;
            }
            Action::ExpandGraph { entity_id } => {
                self.gather(entity_id, self.limit, state, ctx)?;
            }
            Action::VerifyJoin { key } => {
                self.gather(key, self.limit, state, ctx)?;
            }
            Action::Rerank => {
                // Re-score the current pool: order the state's results by the
                // accumulated index score (best-first), ties by doc_id.
                let shared = self.shared.lock().expect("retrieved mutex poisoned");
                state.results.sort_by(|a, b| {
                    shared
                        .score_of(b)
                        .partial_cmp(&shared.score_of(a))
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.cmp(b))
                });
            }
            Action::Refresh { url } => {
                // Hermetic default: the navigator never fetches the network.
                ctx.notes
                    .push(format!("refresh skipped (offline default): {url}"));
            }
        }
        // Keep the ledger's virtual time in sync after every action.
        ctx.budget.used_ms = ctx.budget.used_ms.max(ctx.elapsed_ms);
        Ok(())
    }

    fn candidates(&self, state: &SearchState, _hints: &[String]) -> Vec<SearchAction> {
        let mut out: Vec<SearchAction> = Vec::new();

        // The opening retrieve (once).
        let initial = Action::Retrieve {
            query: state.query.clone(),
        };
        if state.results.is_empty() && !state.seen(&initial) {
            out.push(SearchAction::new(initial, 2.0, 0.1));
            return out;
        }

        if state.results.is_empty() {
            return out;
        }

        if self.multihop {
            // Broaden recall first (highest gain/cost), then follow the best docs.
            if !state.seen(&Action::Widen) {
                out.push(SearchAction::new(Action::Widen, 3.0, 0.3));
            }
            for doc_id in state.results.iter().take(3) {
                let a = Action::FollowLink {
                    doc_id: doc_id.clone(),
                };
                if !state.seen(&a) {
                    out.push(SearchAction::new(a, 1.0, 0.2));
                }
            }
        }
        // A cheap, low-value rerank is always available but never dominates.
        if !state.seen(&Action::Rerank) {
            out.push(SearchAction::new(Action::Rerank, 0.05, 0.05));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// The counting LLM-tier probe (proves the model tier is never reached)
// ---------------------------------------------------------------------------

/// Counts how often the model tier was actually consulted, then abstains. It is
/// ordered last (`PolicyTier::Llm`), so it is only reached when rules *and* the
/// algorithmic tier both returned `None` — the exit gate requires this count to
/// stay 0 for a simple query.
struct CountingProbe {
    calls: Arc<AtomicUsize>,
}

impl SearchPolicy for CountingProbe {
    fn tier(&self) -> PolicyTier {
        PolicyTier::Llm
    }

    fn choose(&self, _state: &SearchState, _actions: &[SearchAction]) -> Option<SearchAction> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        None
    }
}

// ---------------------------------------------------------------------------
// Public result shapes (mirror docs/STAGE7_SPEC.md)
// ---------------------------------------------------------------------------

/// The result of a `deep_search`: the hits plus the navigation trace strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepResult {
    pub hits: Vec<DeepHit>,
    pub trace: Vec<String>,
}

/// One deep-search hit (mirrors the `docs/STAGE7_SPEC.md` wire shape
/// `{doc_id,url,title,snippet,provenance}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepHit {
    pub doc_id: String,
    pub url: String,
    pub title: String,
    pub snippet: String,
    pub provenance: String,
}

/// Normalized document text for one `doc_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocText {
    pub doc_id: String,
    pub text: String,
}

/// The full outcome of one deep-search run, including the observability the exit
/// gate asserts on (stop reason, model-call count, step count).
#[derive(Debug, Clone)]
pub struct DeepSearchReport {
    pub result: DeepResult,
    pub stop: StopReason,
    /// How often the model (LLM) tier was consulted. `0` for a simple query.
    pub model_calls: u32,
    /// Number of executed navigation steps.
    pub steps: usize,
    /// Actions charged against the budget.
    pub used_actions: u32,
}

impl DeepSearchReport {
    /// Whether the run stayed on the fast path (solved after a single retrieve).
    pub fn is_fast_path(&self) -> bool {
        self.stop == StopReason::Solved && self.steps == 1 && self.model_calls == 0
    }
}

// ---------------------------------------------------------------------------
// The entry point
// ---------------------------------------------------------------------------

/// Run a deep search over `searcher` and return the full report.
///
/// `limit` bounds the per-action retrieval window; the navigation may gather
/// more documents than a one-shot `limit` when the query is multi-hop.
pub fn run_deep_search(
    searcher: Arc<dyn Searcher + Send + Sync>,
    query: &str,
    limit: usize,
    now: i64,
    emitter: &dyn PhaseEmitter,
) -> AppResult<DeepSearchReport> {
    let qid = query_id(query, now);
    let emit = |phase: Phase, action: &str, tier: Option<&str>, cost_ms: u64, detail: &[(&str, String)]| {
        let mut d = BTreeMap::new();
        for (k, v) in detail {
            d.insert((*k).to_string(), v.clone());
        }
        emitter.emit(&PhaseEvent {
            ts_ms: now.max(0) as u64 * 1000,
            query_id: qid.clone(),
            phase,
            action: action.to_string(),
            detail: d,
            policy_tier: tier.map(str::to_string),
            cost_ms,
        });
    };

    emit(Phase::Query, "start", None, 0, &[("query", query.to_string())]);

    let config = NavConfig::default();
    let budget = plan_budget(query, &config);
    // `plan_budget` gives a tight budget (`max_actions == 2`) exactly for a
    // simple query; anything larger is treated as multi-hop.
    let multihop = budget.max_actions > 2;
    emit(
        Phase::Analyze,
        if multihop { "multihop" } else { "simple" },
        None,
        0,
        &[("max_actions", budget.max_actions.to_string())],
    );

    let shared = Arc::new(Mutex::new(Retrieved::default()));
    let executor = PipelineExecutor::new(searcher, limit, multihop, shared.clone());

    let calls = Arc::new(AtomicUsize::new(0));
    let policies: Vec<Box<dyn SearchPolicy>> = vec![
        Box::new(RulesPolicy),
        Box::new(AlgorithmPolicy),
        Box::new(CountingProbe {
            calls: calls.clone(),
        }),
    ];

    let mut nav = Navigator::new(policies, Box::new(executor), budget, config);
    let outcome: NavOutcome = nav.run(query);

    // Emit one Navigate (or Retrieve/Rerank/Refresh) event per executed step.
    for step in &outcome.trace {
        let phase = match step.action {
            Action::Retrieve { .. } => Phase::Retrieve,
            Action::Rerank => Phase::Rerank,
            Action::Refresh { .. } => Phase::Refresh,
            _ => Phase::Navigate,
        };
        emit(
            phase,
            step.action.slug(),
            Some(tier_tag(step.used_policy)),
            1,
            &[("reason", step.reason.clone())],
        );
    }

    // Assemble hits best-first from the final state's result ordering.
    let shared = shared.lock().expect("retrieved mutex poisoned");
    let hits: Vec<DeepHit> = outcome
        .state
        .results
        .iter()
        .filter_map(|doc_id| shared.hits.get(doc_id))
        .map(|h| DeepHit {
            doc_id: h.doc_id.clone(),
            url: h.url.clone(),
            title: h.title.clone(),
            snippet: h.snippet.clone(),
            provenance: h.provenance.as_str().to_string(),
        })
        .collect();

    let trace: Vec<String> = outcome.trace.iter().map(|s| s.reason.clone()).collect();
    let model_calls = calls.load(Ordering::SeqCst) as u32;

    emit(
        Phase::Answer,
        "done",
        None,
        0,
        &[
            ("hits", hits.len().to_string()),
            ("stop", stop_tag(outcome.stop).to_string()),
            ("model_calls", model_calls.to_string()),
        ],
    );

    Ok(DeepSearchReport {
        result: DeepResult { hits, trace },
        stop: outcome.stop,
        model_calls,
        steps: outcome.trace.len(),
        used_actions: outcome_executed(&outcome) as u32,
    })
}

/// Number of actions the navigator actually dispatched (from the trace).
fn outcome_executed(o: &NavOutcome) -> usize {
    o.trace.len()
}

fn tier_tag(t: PolicyTier) -> &'static str {
    match t {
        PolicyTier::Rules => "rules",
        PolicyTier::Classifier => "classifier",
        PolicyTier::LocalModel => "local_model",
        PolicyTier::Llm => "llm",
    }
}

/// Stable tag for a stop reason (trace/detail use only).
pub fn stop_tag(s: StopReason) -> &'static str {
    match s {
        StopReason::Solved => "solved",
        StopReason::NoActions => "no_actions",
        StopReason::Deadline => "deadline",
        StopReason::Budget => "budget",
        StopReason::Plateau => "plateau",
    }
}

// ---------------------------------------------------------------------------
// Pipeline entry points
// ---------------------------------------------------------------------------

impl Pipeline {
    /// Run a deep (navigation-driven) search over the live index.
    ///
    /// A simple query stays in the fast path (tight budget, `RulesPolicy`, zero
    /// model calls); an ambiguous / multi-hop query may loop more.
    pub fn deep_search(&self, query: &str, limit: usize, now: i64) -> AppResult<DeepResult> {
        Ok(self.deep_search_report(query, limit, now)?.result)
    }

    /// Like [`Pipeline::deep_search`] but returns the observability report too.
    pub fn deep_search_report(
        &self,
        query: &str,
        limit: usize,
        now: i64,
    ) -> AppResult<DeepSearchReport> {
        run_deep_search(self.searcher(), query, limit, now, &NoopEmitter)
    }

    /// Like [`Pipeline::deep_search_report`] with an explicit trace emitter
    /// (e.g. a `PolicyTrace` sink).
    pub fn deep_search_report_with(
        &self,
        query: &str,
        limit: usize,
        now: i64,
        emitter: &dyn PhaseEmitter,
    ) -> AppResult<DeepSearchReport> {
        run_deep_search(self.searcher(), query, limit, now, emitter)
    }

    /// Read the normalized text of a previously-ingested document.
    ///
    /// `doc_id` is the stable document id (the resource id `crawl_one` assigns).
    /// Reads the latest `document_version`'s content text path.
    pub fn read_doc(&self, doc_id: &str) -> AppResult<DocText> {
        let text_path: Option<String> = self.with_meta_conn(|conn| {
            conn.query_row(
                "SELECT c.text_path
                   FROM document_version dv
                   JOIN content c ON c.content_id = dv.content_id
                  WHERE dv.resource_id = ?1
                  ORDER BY dv.version_no DESC
                  LIMIT 1",
                rusqlite::params![doc_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(search_metadata::db_err)
            .map(|o| o.flatten())
        })?;
        let path = text_path.ok_or_else(|| {
            AppError::NotFound(format!("no normalized document for doc_id {doc_id}"))
        })?;
        let text = std::fs::read_to_string(&path)?;
        Ok(DocText {
            doc_id: doc_id.to_string(),
            text,
        })
    }
}

// ---------------------------------------------------------------------------
// API adapter
// ---------------------------------------------------------------------------

/// Adapts [`Pipeline`] to the API crate's [`DeepSearcher`] trait so the HTTP
/// `/search_deep` and `/read` endpoints can be served from the real pipeline.
pub struct PipelineDeepSearcher {
    pipeline: Arc<Pipeline>,
}

impl PipelineDeepSearcher {
    pub fn new(pipeline: Arc<Pipeline>) -> Self {
        Self { pipeline }
    }
}

impl DeepSearcher for PipelineDeepSearcher {
    fn deep_search(&self, query: &str, limit: usize) -> AppResult<DeepSearchResponse> {
        let now = search_core::time::now_unix_s();
        let report = self.pipeline.deep_search_report(query, limit, now)?;
        Ok(DeepSearchResponse {
            hits: report
                .result
                .hits
                .into_iter()
                .map(|h| search_api::DeepHit {
                    doc_id: h.doc_id,
                    url: h.url,
                    title: h.title,
                    snippet: h.snippet,
                    provenance: h.provenance,
                })
                .collect(),
            trace: report.result.trace,
        })
    }

    fn read(&self, doc_id: &str) -> AppResult<DocResponse> {
        let d = self.pipeline.read_doc(doc_id)?;
        Ok(DocResponse {
            doc_id: d.doc_id,
            text: d.text,
        })
    }
}
