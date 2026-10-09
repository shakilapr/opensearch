//! Loads a corpus-derived query set produced by `agent0 eval build-set`.
//!
//! Ignored by default (it needs a generated file). Run:
//!
//! ```sh
//! SEARCH_EVAL_SET=benchmarks/queries.corpus.jsonl \
//!   cargo test -p search-eval --test corpus_set -- --ignored --nocapture
//! ```
//!
//! Its job is to prove the set the engine will be measured against is loadable and *sane*: the
//! generator's rules are tested where they live, but a set that parsed into thousands of cases with
//! empty labels would silently make every metric meaningless.

use std::collections::HashSet;

use search_eval::{aggregate, load_cases, ndcg_at_k, RunResult};

#[test]
#[ignore = "needs a generated query set"]
fn the_corpus_derived_set_is_loadable_and_labelled() {
    let raw = std::env::var("SEARCH_EVAL_SET")
        .expect("set SEARCH_EVAL_SET to a file from `agent0 eval build-set`");
    // A cargo test's working directory is the crate, not the workspace root, so a relative path
    // like `benchmarks/queries.corpus.jsonl` must be resolved against the workspace.
    let path = std::path::PathBuf::from(&raw);
    let path = if path.is_absolute() {
        path
    } else {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(path)
    };
    let cases = load_cases(&path).expect("the set must load");
    println!("cases: {}", cases.len());
    assert!(cases.len() >= 100, "a benchmark set this small cannot support a claim: {}", cases.len());

    let mut queries = HashSet::new();
    let mut docs = HashSet::new();
    for case in &cases {
        assert!(!case.query.trim().is_empty(), "case {} has an empty query", case.id);
        assert!(!case.relevant.is_empty(), "case {} has no label", case.id);
        assert!(!case.graded.is_empty(), "case {} has no graded label", case.id);
        assert!(
            case.graded.iter().all(|(_, gain)| *gain <= 3),
            "case {} has a gain outside 0..3",
            case.id
        );
        queries.insert(case.query.clone());
        docs.extend(case.relevant.iter().cloned());
    }
    // A duplicated query would double-count the same measurement.
    assert_eq!(queries.len(), cases.len(), "queries must be distinct");
    println!("distinct queries: {} | distinct labelled documents: {}", queries.len(), docs.len());

    // The metric machinery must actually work over this set: a ranking that returns exactly the
    // labelled document must score ~1.0, and an empty ranking must score ~0.
    let perfect: Vec<f64> = cases.iter().map(|c| ndcg_at_k(&c.relevant, &c.graded, 10)).collect();
    let mean = perfect.iter().sum::<f64>() / perfect.len() as f64;
    assert!(
        mean > 0.99,
        "a ranking returning exactly the labelled document must score ~1.0, got {mean}"
    );

    let results: Vec<RunResult> = cases
        .iter()
        .map(|c| RunResult { case_id: c.id.clone(), ranked: Vec::new(), latency_ms: 0 })
        .collect();
    let empty = aggregate(&results, &cases);
    assert!(empty.ndcg_at_10 < 0.01, "an empty ranking must score ~0, got {}", empty.ndcg_at_10);
}
