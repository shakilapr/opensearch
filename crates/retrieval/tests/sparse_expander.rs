//! Stage-9 exit-gate tests for `search-retrieval`'s `SparseExpander` (spec
//! `docs/specs/STAGE9_SPEC.md`).
//!
//! The expander is built from the committed co-occurrence fixture and must (a) be deterministic
//! and (b) improve recall on a fixture query versus the unexpanded query.

use std::collections::BTreeSet;
use std::path::PathBuf;

use search_retrieval::{LaneSearcher, MemoryDoc, MemoryLaneSearcher, SparseExpander};

fn fixture_pairs() -> Vec<(String, String)> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../datasets/fixtures/ann/cooccurrence.jsonl");
    let text = std::fs::read_to_string(&path).expect("committed co-occurrence fixture");
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).expect("valid fixture JSON");
            (
                v["a"].as_str().unwrap().to_string(),
                v["b"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn corpus() -> MemoryLaneSearcher {
    MemoryLaneSearcher::with_docs(vec![
        MemoryDoc::new("d1", "llm inference").with_body("llm inference serving"),
        MemoryDoc::new("d2", "transformer architecture").with_body("transformer architecture deep"),
        MemoryDoc::new("d3", "rust async").with_body("rust async runtime tokio"),
    ])
}

fn recall(searcher: &MemoryLaneSearcher, query: &str, relevant: &[&str], k: usize) -> f32 {
    let got: BTreeSet<String> = searcher
        .search(query, k, None)
        .unwrap()
        .into_iter()
        .map(|c| c.doc_id)
        .collect();
    let hit = relevant.iter().filter(|id| got.contains(**id)).count();
    hit as f32 / relevant.len() as f32
}

#[test]
fn expander_is_deterministic() {
    let pairs = fixture_pairs();
    let e = SparseExpander::from_pairs(&pairs, 4);
    let q = vec!["llm".to_string(), "rust".to_string()];
    let a = e.expand(&q);
    let b = e.expand(&q);
    assert_eq!(a, b, "same input must give the same expansion");
    assert!(!a.is_empty());
    // Input terms are never returned.
    for (t, _) in &a {
        assert_ne!(t, "llm");
        assert_ne!(t, "rust");
    }
}

#[test]
fn expansion_improves_recall_on_fixture_query() {
    let pairs = fixture_pairs();
    let expander = SparseExpander::from_pairs(&pairs, 4);
    let searcher = corpus();
    let relevant = ["d1", "d2"]; // "llm inference" and "transformer architecture"

    // Unexpanded: the query only matches d1, d2 is unreachable.
    let unexpanded = recall(&searcher, "llm", &relevant, 2);

    // Expanded: "llm" pulls in "transformer"/"inference", reaching d2.
    let expanded_terms = expander.expand(&["llm".to_string()]);
    assert!(
        expanded_terms.iter().any(|(t, _)| t == "transformer"),
        "fixture must relate llm -> transformer"
    );
    let mut query_terms = vec!["llm".to_string()];
    query_terms.extend(expanded_terms.into_iter().map(|(t, _)| t));
    let expanded_query = query_terms.join(" ");
    let expanded = recall(&searcher, &expanded_query, &relevant, 2);

    println!(
        "recall@2 unexpanded={unexpanded:.3} expanded={expanded:.3} (query={expanded_query:?})"
    );
    assert!(
        expanded > unexpanded,
        "expansion must improve recall ({expanded} vs {unexpanded})"
    );
    assert_eq!(expanded, 1.0, "expanded query should reach both relevant docs");
}
