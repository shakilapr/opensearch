//! Hermetic integration tests for the §18 coordinator loop (`crates/engine/src/coord.rs`).
//!
//! No network, no clock reads, no sleeps. Every backend is an in-memory
//! [`MemoryLaneSearcher`] (or a deterministic wrapper around one).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use search_engine::analyze::AliasPriors;
use search_engine::coord::{
    run_search, Action, Answerer, Deps, EngineState, RuleSelector, SearchRequest, Selector,
};
use search_engine::evidence::{EvidenceRecord, EvidenceVerifier};
use search_engine::plan::{IndexStats, Mode};
use search_engine::{LaneId, StopReason};
use search_retrieval::{
    Candidate, LaneSearcher, MemoryDoc, MemoryLaneSearcher, RetrievalError,
};

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

fn stats() -> IndexStats {
    IndexStats { docs: 10, avg_doc_len: 4.0, updated_at: 0 }
}

fn request(query: &str, limit: usize, mode: Mode) -> SearchRequest {
    SearchRequest {
        query: query.to_string(),
        limit,
        mode,
        deadline_ms: 200,
        tz_offset_min: 0,
    }
}

/// Two docs match "rust"; the decoy matches neither term. Titles deliberately do NOT contain the
/// whole query string, so the coordinator's title-derived coverage stays at zero and the plateau
/// heuristic is exercised deterministically.
fn corpus() -> MemoryLaneSearcher {
    MemoryLaneSearcher::with_docs(vec![
        MemoryDoc::new("d1", "rust").with_body("async runtime for systems"),
        MemoryDoc::new("d2", "python").with_body("data science toolkit"),
        MemoryDoc::new("d3", "rust").with_body("systems language"),
    ])
}

fn deps<'a>(
    lanes: &'a [&'a dyn LaneSearcher],
    selector: &'a dyn Selector,
    answerer: Option<&'a dyn Answerer>,
    aliases: &'a AliasPriors,
) -> Deps<'a> {
    Deps { lanes, selector, answerer, aliases, stats: stats(), now: 0, verifier: None }
}

/// The same as [`deps`] but with an injected §16 content verifier.
fn deps_with_verifier<'a>(
    lanes: &'a [&'a dyn LaneSearcher],
    selector: &'a dyn Selector,
    aliases: &'a AliasPriors,
    verifier: &'a dyn EvidenceVerifier,
) -> Deps<'a> {
    Deps {
        lanes,
        selector,
        answerer: None,
        aliases,
        stats: stats(),
        now: 0,
        verifier: Some(verifier),
    }
}

// ---------------------------------------------------------------------------------------------
// Test backends
// ---------------------------------------------------------------------------------------------

/// A [`MemoryLaneSearcher`] that records every query it is asked to run and counts calls.
struct Recorder {
    inner: MemoryLaneSearcher,
    calls: AtomicUsize,
    queries: Mutex<Vec<String>>,
}

impl Recorder {
    fn new(inner: MemoryLaneSearcher) -> Self {
        Self { inner, calls: AtomicUsize::new(0), queries: Mutex::new(Vec::new()) }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    fn queries(&self) -> Vec<String> {
        self.queries.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

impl LaneSearcher for Recorder {
    fn search(
        &self,
        query: &str,
        limit: usize,
        source_id: Option<&str>,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut g) = self.queries.lock() {
            g.push(query.to_string());
        }
        self.inner.search(query, limit, source_id)
    }
}

/// A lane that always fails, to prove a lane error never aborts the search.
struct Failing;

impl LaneSearcher for Failing {
    fn search(
        &self,
        _query: &str,
        _limit: usize,
        _source_id: Option<&str>,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        Err(RetrievalError::Index("boom".to_string()))
    }
}

/// Returns nothing on its first call, real hits afterwards: models an initially-empty result set
/// that the coordinator must keep working on (widen/expand) rather than giving up.
struct Grower {
    inner: MemoryLaneSearcher,
    calls: AtomicUsize,
}

impl Grower {
    fn new(inner: MemoryLaneSearcher) -> Self {
        Self { inner, calls: AtomicUsize::new(0) }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl LaneSearcher for Grower {
    fn search(
        &self,
        query: &str,
        limit: usize,
        source_id: Option<&str>,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Ok(Vec::new())
        } else {
            self.inner.search(query, limit, source_id)
        }
    }
}

/// A selector that always abstains from choosing.
struct NeverSelector;

impl Selector for NeverSelector {
    fn choose(&self, _actions: &[Action], _state: &EngineState) -> Option<usize> {
        None
    }
}

/// A selector that always picks index 0 (used to exercise revalidation of a budget-exhausted pick).
struct FirstSelector;

impl Selector for FirstSelector {
    fn choose(&self, actions: &[Action], _state: &EngineState) -> Option<usize> {
        if actions.is_empty() {
            None
        } else {
            Some(0)
        }
    }
}

/// Records that it was called and returns a fixed answer.
struct RecordingAnswerer {
    calls: AtomicUsize,
}

impl RecordingAnswerer {
    fn new() -> Self {
        Self { calls: AtomicUsize::new(0) }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Answerer for RecordingAnswerer {
    fn answer(
        &self,
        _query: &str,
        _evidence: &[EvidenceRecord],
        _passages: &[String],
    ) -> Result<String, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok("ANSWER".to_string())
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[test]
fn empty_index_returns_empty_with_valid_stop_reason() {
    let searcher = Recorder::new(MemoryLaneSearcher::new());
    let lanes: Vec<&dyn LaneSearcher> = vec![&searcher];
    let selector = RuleSelector::new();
    let aliases = AliasPriors::empty();
    let d = deps(&lanes, &selector, None, &aliases);

    let resp = run_search(request("anything", 10, Mode::Fast), &d);

    assert!(resp.results.is_empty(), "empty index must yield no results");
    assert!(resp.answer.is_none());
    // A definite stop reason is always reported (never left unset / panicking).
    assert!(matches!(
        resp.stop_reason,
        StopReason::Deadline
            | StopReason::BudgetExhausted
            | StopReason::SufficientCoverage
            | StopReason::NoAdmissibleAction
            | StopReason::Cancelled
            | StopReason::Plateau
    ));
}

#[test]
fn populated_index_returns_ranked_results_with_provenance() {
    let searcher = Recorder::new(corpus());
    let lanes: Vec<&dyn LaneSearcher> = vec![&searcher];
    let selector = RuleSelector::new();
    let aliases = AliasPriors::empty();
    let d = deps(&lanes, &selector, None, &aliases);

    let resp = run_search(request("rust async", 5, Mode::Fast), &d);

    assert!(!resp.results.is_empty(), "populated index must return hits");
    let top = &resp.results[0];
    assert_eq!(top.doc_id, "d1", "the doc matching both terms ranks first");
    assert!(top.score > 0.0);
    assert!(!top.lane_ranks.is_empty(), "lane_ranks must be populated");
    assert!(
        top.lane_ranks.contains_key(&LaneId::Bm25),
        "lexical rank provenance must be present"
    );
}

#[test]
fn results_never_exceed_limit() {
    let mut mem = MemoryLaneSearcher::new();
    for i in 0..5 {
        mem.add_doc(MemoryDoc::new(format!("doc{i}"), "rust").with_body("async"));
    }
    let searcher = Recorder::new(mem);
    let lanes: Vec<&dyn LaneSearcher> = vec![&searcher];
    let selector = RuleSelector::new();
    let aliases = AliasPriors::empty();
    let d = deps(&lanes, &selector, None, &aliases);

    let resp = run_search(request("rust async", 2, Mode::Fast), &d);
    assert!(resp.results.len() <= 2, "results must never exceed req.limit");
}

#[test]
fn original_query_is_the_one_searched() {
    let searcher = Recorder::new(corpus());
    let lanes: Vec<&dyn LaneSearcher> = vec![&searcher];
    let selector = RuleSelector::new();
    let aliases = AliasPriors::empty();
    let d = deps(&lanes, &selector, None, &aliases);

    let _ = run_search(request("rust async", 5, Mode::Fast), &d);

    let seen = searcher.queries();
    assert!(!seen.is_empty());
    assert!(
        seen.iter().all(|q| q == "rust async"),
        "the exact original query string must be searched, saw {seen:?}"
    );
}

#[test]
fn fast_mode_is_one_round_and_deep_stops_early_on_a_rule() {
    // Fast: exactly one retrieval round.
    let fast = Recorder::new(corpus());
    let lanes: Vec<&dyn LaneSearcher> = vec![&fast];
    let selector = RuleSelector::new();
    let aliases = AliasPriors::empty();
    let d = deps(&lanes, &selector, None, &aliases);
    let resp_fast = run_search(request("rust async", 5, Mode::Fast), &d);
    assert_eq!(fast.calls(), 1, "Fast performs exactly one retrieval round");
    assert!(
        resp_fast.trace.iter().any(|l| l.contains("round 1")),
        "trace must log the round"
    );

    // Deep: up to 5 rounds, but a rule (coverage plateau) fires first.
    let deep = Recorder::new(corpus());
    let lanes2: Vec<&dyn LaneSearcher> = vec![&deep];
    let d2 = deps(&lanes2, &selector, None, &aliases);
    let resp_deep = run_search(request("rust async", 5, Mode::Deep), &d2);
    assert!(deep.calls() >= 1 && deep.calls() <= 5, "Deep rounds are bounded at 5");
    assert!(
        resp_deep.trace.iter().any(|l| l.contains("Plateau")),
        "a stopping rule must be logged, trace: {:?}",
        resp_deep.trace
    );
    assert!(
        deep.calls() < 5,
        "Deep must stop early once the rule fires, saw {} rounds",
        deep.calls()
    );
}

#[test]
fn lane_error_does_not_abort_the_search() {
    let failing = Failing;
    let good = Recorder::new(corpus());
    let lanes: Vec<&dyn LaneSearcher> = vec![&failing, &good];
    let selector = RuleSelector::new();
    let aliases = AliasPriors::empty();
    let d = deps(&lanes, &selector, None, &aliases);

    let resp = run_search(request("rust async", 5, Mode::Fast), &d);

    assert!(!resp.results.is_empty(), "the healthy lexical lane still returns results");
    assert!(
        resp.trace.iter().any(|l| l.contains("boom")),
        "the lane failure must appear in the trace, trace: {:?}",
        resp.trace
    );
}

#[test]
fn selector_none_yields_no_admissible_action() {
    let searcher = Recorder::new(corpus());
    let lanes: Vec<&dyn LaneSearcher> = vec![&searcher];
    let selector = NeverSelector;
    let aliases = AliasPriors::empty();
    let d = deps(&lanes, &selector, None, &aliases);

    let resp = run_search(request("rust async", 5, Mode::Auto), &d);

    assert_eq!(resp.stop_reason, StopReason::NoAdmissibleAction);
    assert_eq!(searcher.calls(), 1, "no follow-up lane call after the selector declines");
}

#[test]
fn action_with_exhausted_budget_is_not_executed() {
    // Fast has exactly one round. The selector proposes a follow-up, but there are no rounds left,
    // so the action must be rejected at revalidation and never reach a lane.
    let searcher = Recorder::new(corpus());
    let lanes: Vec<&dyn LaneSearcher> = vec![&searcher];
    let selector = FirstSelector;
    let aliases = AliasPriors::empty();
    let d = deps(&lanes, &selector, None, &aliases);

    let resp = run_search(request("rust async", 5, Mode::Fast), &d);

    assert_eq!(searcher.calls(), 1, "no extra lane call may happen once the budget is exhausted");
    assert_eq!(resp.stop_reason, StopReason::BudgetExhausted);
    assert!(
        resp.trace.iter().any(|l| l.contains("not executed")),
        "the rejected action must be logged, trace: {:?}",
        resp.trace
    );
}

#[test]
fn rule_selector_expands_highest_weight_uncovered_unit() {
    let actions = vec![
        Action::Widen { lane: LaneId::Bm25 },
        Action::Expand { unit_id: "U1".to_string() },
        Action::Expand { unit_id: "U2".to_string() },
        Action::Abstain,
    ];
    let mut state = EngineState::default();
    state.uncovered_mandatory = vec![("U1".to_string(), 1.0), ("U2".to_string(), 2.0)];

    let idx = RuleSelector::new().choose(&actions, &state).unwrap_or(usize::MAX);
    assert_eq!(
        actions.get(idx),
        Some(&Action::Expand { unit_id: "U2".to_string() }),
        "Expand must target the highest-weight uncovered mandatory unit"
    );
}

#[test]
fn rule_selector_widens_global_lexical_then_abstains() {
    let actions = vec![
        Action::Widen { lane: LaneId::Bm25 },
        Action::Expand { unit_id: "U1".to_string() },
        Action::Abstain,
    ];
    // Nothing uncovered -> widen the global lexical lane.
    let state = EngineState::default();
    let idx = RuleSelector::new().choose(&actions, &state).unwrap_or(usize::MAX);
    assert_eq!(actions.get(idx), Some(&Action::Widen { lane: LaneId::Bm25 }));

    // Nothing to do -> abstain.
    let only_abstain = vec![Action::Abstain];
    let idx2 = RuleSelector::new().choose(&only_abstain, &state).unwrap_or(usize::MAX);
    assert_eq!(only_abstain.get(idx2), Some(&Action::Abstain));
}

#[test]
fn answerer_runs_after_selection_and_does_not_change_results() {
    let searcher_a = Recorder::new(corpus());
    let lanes_a: Vec<&dyn LaneSearcher> = vec![&searcher_a];
    let selector = RuleSelector::new();
    let aliases = AliasPriors::empty();
    let deps_a = deps(&lanes_a, &selector, None, &aliases);
    let without = run_search(request("rust async", 5, Mode::Fast), &deps_a);

    let searcher_b = Recorder::new(corpus());
    let lanes_b: Vec<&dyn LaneSearcher> = vec![&searcher_b];
    let answerer = RecordingAnswerer::new();
    let deps_b = deps(&lanes_b, &selector, Some(&answerer), &aliases);
    let with = run_search(request("rust async", 5, Mode::Fast), &deps_b);

    assert_eq!(without.results, with.results, "the answerer must not alter results");
    assert_eq!(with.answer.as_deref(), Some("ANSWER"));
    assert_eq!(answerer.calls(), 1, "the answerer is invoked exactly once, after selection");
}

#[test]
fn zero_candidates_still_considers_widening_or_expanding() {
    // The first call returns nothing; the coordinator must keep trying (via the selector) rather
    // than declaring no matches immediately, and must eventually surface the later hits.
    let grower = Grower::new(corpus());
    let lanes: Vec<&dyn LaneSearcher> = vec![&grower];
    let selector = RuleSelector::new();
    let aliases = AliasPriors::empty();
    let d = deps(&lanes, &selector, None, &aliases);

    let resp = run_search(request("rust async", 5, Mode::Auto), &d);

    assert!(!resp.results.is_empty(), "later retrieval must surface results");
    assert!(grower.calls() >= 2, "the coordinator must take a follow-up action");
}

// ---------------------------------------------------------------------------------------------
// §16 evidence verification: token-based title matching and the injected `EvidenceVerifier`.
// ---------------------------------------------------------------------------------------------

/// A §16 verifier that reports content support only for one named document.
struct SupportsDoc {
    doc_id: String,
}

impl EvidenceVerifier for SupportsDoc {
    fn supports(&self, unit_text: &str, doc_id: &str) -> bool {
        !unit_text.trim().is_empty() && doc_id == self.doc_id
    }
}

/// A §16 verifier that reports no support at all (forces the title fallback).
struct NeverSupports;

impl EvidenceVerifier for NeverSupports {
    fn supports(&self, _unit_text: &str, _doc_id: &str) -> bool {
        false
    }
}

#[test]
fn hyphenated_title_supports_multi_word_unit() {
    // The unit is the whole phrase "retrieval augmented generation"; the document renders it
    // "Retrieval-Augmented Generation ...". The old substring test failed on the hyphen; the
    // normalised token matcher must succeed, so coverage reaches 1.0 with no verifier injected.
    let mem = MemoryLaneSearcher::with_docs(vec![
        MemoryDoc::new("d1", "Retrieval-Augmented Generation: A Survey")
            .with_body("retrieval augmented generation explained"),
        MemoryDoc::new("d2", "Unrelated topic").with_body("nothing here"),
    ]);
    let searcher = Recorder::new(mem);
    let lanes: Vec<&dyn LaneSearcher> = vec![&searcher];
    let selector = RuleSelector::new();
    let aliases = AliasPriors::empty();
    let d = deps(&lanes, &selector, None, &aliases);

    let resp = run_search(request("retrieval augmented generation", 5, Mode::Fast), &d);

    assert!(
        resp.coverage.mandatory_ok,
        "the hyphenated title must support U1: {:?}",
        resp.coverage
    );
    assert!(
        resp.coverage.coverage >= 0.99,
        "coverage {} should be ~1.0",
        resp.coverage.coverage
    );
    assert!(
        resp.coverage.unsupported.is_empty(),
        "unsupported: {:?}",
        resp.coverage.unsupported
    );
}

#[test]
fn injected_verifier_supports_when_titles_alone_would_not() {
    // The title shares no token with the unit, so title matching alone cannot support U1.
    let corpus = || {
        MemoryLaneSearcher::with_docs(vec![MemoryDoc::new("d1", "Cooking pasta tonight")
            .with_body("retrieval augmented generation explained")])
    };
    let selector = RuleSelector::new();
    let aliases = AliasPriors::empty();

    // No verifier -> title fallback fails -> U1 unsupported.
    let s1 = Recorder::new(corpus());
    let lanes1: Vec<&dyn LaneSearcher> = vec![&s1];
    let d1 = deps(&lanes1, &selector, None, &aliases);
    let without = run_search(request("retrieval augmented generation", 5, Mode::Fast), &d1);
    assert!(!without.coverage.mandatory_ok, "title alone must not support U1");
    assert!(
        without.coverage.unsupported.contains(&"U1".to_string()),
        "unsupported: {:?}",
        without.coverage.unsupported
    );

    // With a verifier that reports content support for d1 -> U1 supported despite the title.
    let s2 = Recorder::new(corpus());
    let lanes2: Vec<&dyn LaneSearcher> = vec![&s2];
    let verifier = SupportsDoc { doc_id: "d1".to_string() };
    let d2 = deps_with_verifier(&lanes2, &selector, &aliases, &verifier);
    let with = run_search(request("retrieval augmented generation", 5, Mode::Fast), &d2);
    assert!(
        with.coverage.mandatory_ok,
        "the verifier must support U1: {:?}",
        with.coverage
    );
    assert!(with.coverage.coverage >= 0.99);
}

#[test]
fn unit_with_no_matching_title_stays_unsupported() {
    // d1 is retrieved (its body contains the query terms) but its TITLE contains none of the
    // unit's tokens and no verifier is injected, so U1 must stay unsupported/Unknown.
    let mem = MemoryLaneSearcher::with_docs(vec![MemoryDoc::new("d1", "Cooking pasta tonight")
        .with_body("quantum entanglement explained")]);
    let searcher = Recorder::new(mem);
    let lanes: Vec<&dyn LaneSearcher> = vec![&searcher];
    let selector = RuleSelector::new();
    let aliases = AliasPriors::empty();
    let d = deps(&lanes, &selector, None, &aliases);

    let resp = run_search(request("quantum entanglement", 5, Mode::Auto), &d);

    assert_eq!(resp.coverage.coverage, 0.0);
    assert!(!resp.coverage.mandatory_ok);
    assert_eq!(resp.coverage.unsupported, vec!["U1".to_string()]);
    assert!(resp.coverage.contradicted_units.is_empty());
    assert!(!resp.results.is_empty(), "the document is still retrieved");
    assert!(
        resp.results.iter().all(|r| r.unit_support.is_empty()),
        "no result may claim support for an unverified unit"
    );
}

#[test]
fn unit_is_never_marked_supported_on_speculation() {
    // Even with a verifier present, one that reports no support must not cause a speculative
    // Supported record; the title (also non-matching) leaves U1 unsupported.
    let mem = MemoryLaneSearcher::with_docs(vec![MemoryDoc::new("d1", "Cooking pasta tonight")
        .with_body("quantum entanglement explained")]);
    let searcher = Recorder::new(mem);
    let lanes: Vec<&dyn LaneSearcher> = vec![&searcher];
    let selector = RuleSelector::new();
    let aliases = AliasPriors::empty();
    let verifier = NeverSupports;
    let d = deps_with_verifier(&lanes, &selector, &aliases, &verifier);

    let resp = run_search(request("quantum entanglement", 5, Mode::Fast), &d);

    assert_eq!(resp.coverage.coverage, 0.0);
    assert!(!resp.coverage.mandatory_ok);
    assert_eq!(resp.coverage.unsupported, vec!["U1".to_string()]);
    assert!(resp.results.iter().all(|r| r.unit_support.is_empty()));
}

/// The trace must name the document that satisfied each unit, not merely that coverage is
/// positive: a caller that disagrees with a coverage verdict (a page that happens to contain
/// the unit's tokens, say) needs to know which document to look at.
#[test]
fn trace_names_the_document_that_satisfied_a_unit() {
    let mem = MemoryLaneSearcher::with_docs(vec![MemoryDoc::new("d1", "Cooking pasta tonight")
        .with_body("quantum entanglement explained")]);
    let searcher = Recorder::new(mem);
    let lanes: Vec<&dyn LaneSearcher> = vec![&searcher];
    let selector = RuleSelector::new();
    let aliases = AliasPriors::empty();
    let verifier = SupportsDoc { doc_id: "d1".to_string() };
    let d = deps_with_verifier(&lanes, &selector, &aliases, &verifier);

    let resp = run_search(request("quantum entanglement", 5, Mode::Fast), &d);

    assert!(resp.coverage.mandatory_ok, "{:?}", resp.coverage);
    let line = resp
        .trace
        .iter()
        .find(|l| l.starts_with("evidence:"))
        .unwrap_or_else(|| panic!("the trace must record the evidence: {:?}", resp.trace));
    assert!(
        line.contains("U1=d1"),
        "the supporting document must be named, got: {line}"
    );

    // And with no support at all, the line says so instead of staying silent.
    let verifier = NeverSupports;
    let d = deps_with_verifier(&lanes, &selector, &aliases, &verifier);
    let resp = run_search(request("quantum entanglement", 5, Mode::Fast), &d);
    assert!(
        resp.trace.iter().any(|l| l == "evidence: no unit supported"),
        "an unsupported unit must be reported as such: {:?}",
        resp.trace
    );
}

// ---------------------------------------------------------------------------------------------
// §17 stop-reason reporting: deadline > limit-reached > sufficient-coverage > budget.
// ---------------------------------------------------------------------------------------------

#[test]
fn stop_reason_is_limit_reached_for_every_mode_when_limit_is_met() {
    // Two documents match "rust async"; a limit of 2 is satisfied by every mode, so the reported
    // reason must be LimitReached rather than the round-budget/plateau verdict the loop hit.
    for mode in [Mode::Fast, Mode::Auto, Mode::Deep] {
        let searcher = Recorder::new(corpus());
        let lanes: Vec<&dyn LaneSearcher> = vec![&searcher];
        let selector = RuleSelector::new();
        let aliases = AliasPriors::empty();
        let d = deps(&lanes, &selector, None, &aliases);

        let resp = run_search(request("rust async", 2, mode), &d);

        assert_eq!(resp.results.len(), 2, "fixture must fill the limit ({mode:?})");
        assert_eq!(
            resp.stop_reason,
            StopReason::LimitReached,
            "mode {mode:?} must report LimitReached, trace: {:?}",
            resp.trace
        );
        assert!(
            resp.trace.iter().any(|l| l.contains("LimitReached")),
            "the reason must appear in the trace ({mode:?})"
        );
    }
}

#[test]
fn stop_reason_is_deadline_when_deadline_already_elapsed_for_every_mode() {
    // The deadline is already past at entry. Even though the fixture fills the limit, the
    // deadline outranks the limit in the §17 priority, so Deadline must be reported.
    for mode in [Mode::Fast, Mode::Auto, Mode::Deep] {
        let searcher = Recorder::new(corpus());
        let lanes: Vec<&dyn LaneSearcher> = vec![&searcher];
        let selector = RuleSelector::new();
        let aliases = AliasPriors::empty();
        let d = deps(&lanes, &selector, None, &aliases);

        let req = SearchRequest {
            query: "rust async".to_string(),
            limit: 2,
            mode,
            deadline_ms: 0,
            tz_offset_min: 0,
        };
        let resp = run_search(req, &d);

        assert_eq!(resp.results.len(), 2, "fixture must fill the limit ({mode:?})");
        assert_eq!(
            resp.stop_reason,
            StopReason::Deadline,
            "mode {mode:?} must report Deadline, trace: {:?}",
            resp.trace
        );
    }
}
