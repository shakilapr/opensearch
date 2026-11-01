//! §7 routing gate (`crates/engine/src/route.rs`) through the real coordinator.
//!
//! Two properties, and both are about what reaches the *searcher*, not about what the router
//! returns on its own:
//!
//! 1. With `SEARCH_ROUTE=1`, an exact-surface query does not call the semantic lanes at all —
//!    the latency claim in v2 §7 is a claim about calls made, so it is measured on calls made.
//! 2. The lexical lane still runs and still returns the answer. A router that saves time by
//!    losing the document containing the quoted string is a regression, not a trade.
//!
//! `SEARCH_ROUTE` is process-global, so the whole A/B lives in one test function: two tests
//! racing on the same variable would be a flake generator.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use search_engine::analyze::AliasPriors;
use search_engine::coord::{run_search, Deps, RuleSelector, SearchRequest};
use search_engine::plan::{IndexStats, Mode};
use search_retrieval::{
    Candidate, Lane, LaneSearcher, MemoryDoc, MemoryLaneSearcher, RetrievalError,
};

/// A lane that declares which fusion lane it feeds, so the router can decide about it.
struct Tagged {
    inner: MemoryLaneSearcher,
    lane: Lane,
    calls: AtomicUsize,
    queries: Mutex<Vec<String>>,
}

impl Tagged {
    fn new(lane: Lane, docs: Vec<MemoryDoc>) -> Self {
        Self {
            inner: MemoryLaneSearcher::with_docs(docs),
            lane,
            calls: AtomicUsize::new(0),
            queries: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn reset(&self) {
        self.calls.store(0, Ordering::SeqCst);
        if let Ok(mut g) = self.queries.lock() {
            g.clear();
        }
    }
}

impl LaneSearcher for Tagged {
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

    fn lane_hint(&self) -> Option<Lane> {
        Some(self.lane)
    }
}

fn stats() -> IndexStats {
    IndexStats {
        docs: 4,
        avg_doc_len: 4.0,
        updated_at: 0,
    }
}

fn request(query: &str) -> SearchRequest {
    SearchRequest {
        query: query.to_string(),
        limit: 5,
        mode: Mode::Auto,
        deadline_ms: 500,
        tz_offset_min: 0,
    }
}

#[test]
fn routing_skips_semantic_lanes_but_never_the_lexical_one() {
    let lexical = Tagged::new(
        Lane::Bm25,
        vec![
            MemoryDoc::new("d1", "connection reset").with_body("connection reset by peer"),
            MemoryDoc::new("d2", "release notes").with_body("unrelated material"),
        ],
    );
    let dense = Tagged::new(
        Lane::Dense,
        vec![
            // Deliberately the same two documents the lexical lane can reach: the assertion
            // below is about a skipped lane not changing the answer, so the fixture must not
            // give the skipped lane a document of its own to contribute.
            MemoryDoc::new("d1", "connection reset").with_body("connection reset by peer"),
            MemoryDoc::new("d2", "release notes").with_body("unrelated material"),
        ],
    );
    let lanes: &[&dyn LaneSearcher] = &[&lexical, &dense];
    let selector = RuleSelector::new();
    let aliases = AliasPriors::default();
    let deps = Deps {
        lanes,
        selector: &selector,
        answerer: None,
        aliases: &aliases,
        stats: stats(),
        now: 0,
        verifier: None,
        planner: None,
    };

    // A quoted phrase: a literal surface, so §7's exact-surface branch applies.
    let query = "\"connection reset by peer\"";
    let analysis = search_engine::analyze::analyze_query(query, 0, 0);
    assert_eq!(
        search_engine::route::classify(&analysis),
        search_engine::route::QueryClass::ExactSurface,
        "the fixture must exercise the exact-surface branch"
    );

    // --- routing off (the current default) : every lane runs ---
    std::env::remove_var("SEARCH_ROUTE");
    let resp_off = run_search(request(query), &deps);
    let dense_off = dense.calls();
    assert!(
        dense_off > 0,
        "with routing disabled every searcher must run, got {dense_off}"
    );
    assert!(
        !resp_off.results.is_empty(),
        "fixture must return something to lose"
    );

    // --- routing on : the dense lane is never called, the answer survives ---
    lexical.reset();
    dense.reset();
    std::env::set_var("SEARCH_ROUTE", "1");
    let resp_on = run_search(request(query), &deps);
    std::env::remove_var("SEARCH_ROUTE");

    assert_eq!(
        dense.calls(),
        0,
        "an exact-surface query must not pay for the dense lane; queries it saw: {:?}",
        dense.queries.lock().map(|g| g.clone()).unwrap_or_default()
    );
    assert!(
        lexical.calls() > 0,
        "the lexical expert is never routed away"
    );
    assert!(
        resp_on.trace.iter().any(|t| t.starts_with("route: ")),
        "the trace must say which route was taken: {:?}",
        resp_on.trace
    );
    assert_eq!(
        resp_on
            .results
            .iter()
            .map(|r| r.doc_id.clone())
            .collect::<Vec<_>>(),
        resp_off
            .results
            .iter()
            .map(|r| r.doc_id.clone())
            .collect::<Vec<_>>(),
        "skipping a lane that contributed nothing must not change the result set"
    );
}

#[test]
fn a_lane_without_a_hint_is_never_dropped() {
    // `lane_hint() == None` means "unknown", and the router must treat unknown as "run it" —
    // otherwise adding a lane to the engine would silently stop it from being called.
    struct Anonymous {
        inner: MemoryLaneSearcher,
        calls: AtomicUsize,
    }
    impl LaneSearcher for Anonymous {
        fn search(
            &self,
            query: &str,
            limit: usize,
            source_id: Option<&str>,
        ) -> Result<Vec<Candidate>, RetrievalError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner.search(query, limit, source_id)
        }
    }

    let anonymous = Anonymous {
        inner: MemoryLaneSearcher::with_docs(vec![MemoryDoc::new("d1", "x").with_body("y")]),
        calls: AtomicUsize::new(0),
    };
    let lexical = Tagged::new(Lane::Bm25, vec![MemoryDoc::new("d2", "x").with_body("y")]);
    let lanes: &[&dyn LaneSearcher] = &[&lexical, &anonymous];
    let selector = RuleSelector::new();
    let aliases = AliasPriors::default();
    let deps = Deps {
        lanes,
        selector: &selector,
        answerer: None,
        aliases: &aliases,
        stats: stats(),
        now: 0,
        verifier: None,
        planner: None,
    };

    std::env::set_var("SEARCH_ROUTE", "1");
    let _ = run_search(request("\"exact phrase\""), &deps);
    std::env::remove_var("SEARCH_ROUTE");

    assert!(
        anonymous.calls.load(Ordering::SeqCst) > 0,
        "a searcher with no lane hint must always run"
    );
}
