//! Stage-3 feature rerank gate (`SEARCH_RERANK`) through the real coordinator.
//!
//! The fixture is deliberately fused in one order and reranked in another: the top fused
//! document has no title hit, while the runner-up's title contains the quoted query phrase, so
//! the feature ranker has a real reason to move it. `SEARCH_RERANK` is process-global, so the
//! whole off/on A/B lives in one test function — separate tests racing on the same variable
//! would be a flake generator.

use search_engine::analyze::AliasPriors;
use search_engine::coord::{run_search, Deps, RuleSelector, SearchRequest, SearchResponse};
use search_engine::plan::{IndexStats, Mode};
use search_retrieval::{Candidate, LaneSearcher, RetrievalError};

/// Two documents, returned in this order regardless of the query, so fusion order is known.
struct Fixed;

impl LaneSearcher for Fixed {
    fn search(
        &self,
        _query: &str,
        _limit: usize,
        _source_id: Option<&str>,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        Ok(vec![
            Candidate::new("d1", 1.0).with_meta("systems runtime", "https://example.test/d1"),
            Candidate::new("d2", 0.9).with_meta("Rust async guide", "https://example.test/d2"),
        ])
    }
}

fn run() -> SearchResponse {
    let lane = Fixed;
    let lanes: &[&dyn LaneSearcher] = &[&lane];
    let selector = RuleSelector::new();
    let aliases = AliasPriors::empty();
    let deps = Deps {
        lanes,
        selector: &selector,
        answerer: None,
        aliases: &aliases,
        stats: IndexStats {
            docs: 2,
            avg_doc_len: 4.0,
            updated_at: 0,
        },
        now: 0,
        verifier: None,
        planner: None,
    };
    run_search(
        SearchRequest {
            query: "\"rust async\"".to_string(),
            limit: 5,
            mode: Mode::Fast,
            deadline_ms: 200,
            tz_offset_min: 0,
        },
        &deps,
    )
}

fn ids(resp: &SearchResponse) -> Vec<&str> {
    resp.results.iter().map(|r| r.doc_id.as_str()).collect()
}

#[test]
fn rerank_is_off_by_default_and_reorders_only_when_enabled() {
    let previous = std::env::var("SEARCH_RERANK").ok();
    std::env::remove_var("SEARCH_RERANK");

    // Unset: the fused order, and no rerank trace line.
    let off = run();
    let off_again = run();
    assert_eq!(
        off.results, off_again.results,
        "the off path must be deterministic"
    );
    assert_eq!(
        ids(&off),
        vec!["d1", "d2"],
        "unset must keep the fused order"
    );
    assert!(
        !off.trace.iter().any(|line| line.starts_with("rerank:")),
        "unset must not append a rerank trace line: {:?}",
        off.trace
    );

    // `0` is not a recognized enable value either.
    std::env::set_var("SEARCH_RERANK", "0");
    let zero = run();
    std::env::remove_var("SEARCH_RERANK");
    assert_eq!(
        zero.results, off.results,
        "anything other than 1/true must be off"
    );

    // Enabled: the title hit moves the runner-up to the front.
    std::env::set_var("SEARCH_RERANK", "1");
    let on = run();
    std::env::remove_var("SEARCH_RERANK");

    assert_eq!(
        ids(&on),
        vec!["d2", "d1"],
        "the title-matching document must move first"
    );
    assert_ne!(on.results, off.results, "an ordering must actually change");
    let line = on.trace.iter().find(|line| line.starts_with("rerank:"));
    assert_eq!(
        line.map(String::as_str),
        Some("rerank: 2 candidates, 2 reordered"),
        "the trace must report the window and how many positions changed: {:?}",
        on.trace
    );

    // `true` is the other documented enable value.
    std::env::set_var("SEARCH_RERANK", "true");
    let on_true = run();
    std::env::remove_var("SEARCH_RERANK");
    assert_eq!(
        on_true.results, on.results,
        "`true` and `1` must enable the same path"
    );

    match previous {
        Some(value) => std::env::set_var("SEARCH_RERANK", value),
        None => std::env::remove_var("SEARCH_RERANK"),
    }
}
