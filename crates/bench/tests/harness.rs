//! Integration test for the ablation harness: drives the same library function the `bench`
//! binary uses, over the committed corpus + query set, and asserts the report is honest and
//! well-formed. Writes to a temp file so the committed `benchmarks/results.json` is untouched.

use std::path::Path;

use search_bench::{load_corpus, run, Config, DEFAULT_CORPUS_DIR, DEFAULT_QUERIES};

fn finite_unit(x: f64) -> bool {
    x.is_finite() && (0.0..=1.0).contains(&x)
}

#[test]
fn corpus_loads_and_is_committed() {
    let docs = load_corpus(Path::new(DEFAULT_CORPUS_DIR)).expect("corpus loads");
    assert!(
        (20..=40).contains(&docs.len()),
        "fixture corpus should be 20-40 docs, got {}",
        docs.len()
    );
    let total: usize = docs.iter().map(|d| d.body.len() + d.title.len()).sum();
    assert!(total < 200_000, "corpus should stay small (<200 KB of text)");
    assert!(Path::new(DEFAULT_QUERIES).exists(), "query set is committed");
}

#[test]
fn all_ablations_produce_finite_metrics_and_a_results_file() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("results.json");
    let cfg = Config {
        out_path: out.clone(),
        ..Config::default()
    };

    let report = run(&cfg).expect("harness runs");

    // At least the two ablation rows the task calls out, plus the full set.
    assert!(report.ablations.len() >= 2, "need >=2 ablations");
    let by: std::collections::HashMap<&str, &search_bench::AblationRow> =
        report.ablations.iter().map(|a| (a.name.as_str(), a)).collect();
    assert!(by.contains_key("bm25"), "bm25 ablation present");
    assert!(
        by.contains_key("bm25+dense"),
        "bm25+dense ablation present"
    );

    // Every metric in [0,1] and finite; n matches the query count.
    for a in &report.ablations {
        assert_eq!(a.metrics.n, report.budget.queries, "n == query count");
        assert!(finite_unit(a.metrics.ndcg_at_10), "{} ndcg", a.name);
        assert!(finite_unit(a.metrics.mrr), "{} mrr", a.name);
        assert!(finite_unit(a.metrics.recall_at_10), "{} recall", a.name);
        assert!(a.metrics.p95_ms >= a.metrics.p50_ms, "{} latency order", a.name);
    }

    // The machine-readable results file is produced and round-trips.
    assert!(out.exists(), "results file written");
    let text = std::fs::read_to_string(&out).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    assert!(parsed["ablations"].as_array().unwrap().len() >= 2);
    assert!(parsed["budget"]["docs"].as_u64().unwrap() >= 20);
    // The honesty notes must include the unavailable-lane disclosures.
    let notes = parsed["notes"].as_array().unwrap();
    assert!(
        notes.iter().any(|n| n.as_str().unwrap().contains("UNAVAILABLE")),
        "report must disclose unavailable lanes"
    );
}

#[test]
fn hybrid_is_measured_not_guaranteed() {
    // The exit gate wants hybrid to beat BM25, but the harness must report the ACTUAL number
    // either way. This test only asserts both were measured on the identical case set.
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        out_path: dir.path().join("results.json"),
        ..Config::default()
    };
    let report = run(&cfg).expect("harness runs");
    let bm25 = report.ablations.iter().find(|a| a.name == "bm25").unwrap();
    let hybrid = report
        .ablations
        .iter()
        .find(|a| a.name == "bm25+dense+sparse")
        .unwrap();
    assert_eq!(bm25.metrics.n, hybrid.metrics.n);
    // Both are valid measurements regardless of which wins.
    assert!(finite_unit(bm25.metrics.ndcg_at_10));
    assert!(finite_unit(hybrid.metrics.ndcg_at_10));
}
