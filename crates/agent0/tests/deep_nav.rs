//! Stage-4 deep-navigation tests for `crates/agent0`.
//!
//! Hermetic: a tempdir Tantivy index (no network) drives the navigation, and a
//! loopback fixture server is used only for the `read_doc` round trip.
//!
//! Covers the Stage-4 exit gate: a simple query stays in the fast path with
//! **0** model calls, and a multi-hop query improves over one-shot retrieval.

use std::sync::{Arc, Mutex};

use axum::{response::Html, routing::get, Router};

use search_agent0::{run_deep_search, NoopEmitter, Phase, PhaseEmitter, PhaseEvent, StopReason};
use search_api::{IndexerSearcher, Searcher};
use search_core::config::Config;
use search_core::AppError;
use search_indexer::{IndexDoc, Indexer, IndexerConfig};

// ---------------------------------------------------------------------------
// Fixture index
// ---------------------------------------------------------------------------

/// A 4-document corpus where a multi-hop query's terms spread across documents,
/// so navigation (widen + follow) gathers more than a one-shot window.
fn fixture_index() -> (tempfile::TempDir, Arc<Indexer>) {
    let dir = tempfile::TempDir::new().unwrap();
    let indexer = Indexer::open(IndexerConfig::new(dir.path().join("tantivy"))).unwrap();
    indexer
        .index_batch(vec![
            IndexDoc::new(
                "d1",
                "res_d1",
                "https://example.test/ownership",
                "Ownership rules",
                "rust ownership rules and the borrow checker",
            )
            .with_source("docs"),
            IndexDoc::new(
                "d2",
                "res_d2",
                "https://example.test/borrowing",
                "Borrowing in rust",
                "rust borrowing annotations",
            )
            .with_source("docs"),
            IndexDoc::new(
                "d3",
                "res_d3",
                "https://example.test/lifetimes",
                "Lifetimes guide",
                "rust lifetimes and borrowing elision",
            )
            .with_source("docs"),
            IndexDoc::new(
                "d4",
                "res_d4",
                "https://example.test/move",
                "Move semantics",
                "rust ownership move semantics deep dive",
            )
            .with_source("blog"),
        ])
        .unwrap();
    indexer.publish_manifest().unwrap();
    (dir, Arc::new(indexer))
}

fn fixture_searcher() -> (tempfile::TempDir, Arc<dyn Searcher + Send + Sync>) {
    let (dir, indexer) = fixture_index();
    let s: Arc<dyn Searcher + Send + Sync> = Arc::new(IndexerSearcher::new(indexer));
    (dir, s)
}

fn now() -> i64 {
    search_core::time::now_unix_s()
}

// ---------------------------------------------------------------------------
// Recording trace sink
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<PhaseEvent>>,
}

impl PhaseEmitter for Recorder {
    fn emit(&self, ev: &PhaseEvent) {
        self.events.lock().unwrap().push(ev.clone());
    }
}

// ---------------------------------------------------------------------------
// (1) simple query -> fast path, 0 model calls
// ---------------------------------------------------------------------------

#[test]
fn simple_query_uses_the_fast_path_with_zero_model_calls() {
    let (_dir, searcher) = fixture_searcher();

    let report = run_deep_search(searcher, "rust", 10, now(), &NoopEmitter).unwrap();

    assert_eq!(report.stop, StopReason::Solved, "a simple query is answered");
    assert_eq!(report.steps, 1, "exactly one retrieve: the fast path");
    assert_eq!(report.model_calls, 0, "no model call for a simple query");
    assert!(report.is_fast_path());
    assert_eq!(report.used_actions, 1);
    assert!(!report.result.hits.is_empty());
    // Every hit carries the STAGE7 shape.
    for h in &report.result.hits {
        assert!(!h.doc_id.is_empty());
        assert!(h.url.starts_with("https://"));
        assert!(!h.provenance.is_empty());
    }
}

// ---------------------------------------------------------------------------
// (2) multi-hop query -> improves over one-shot
// ---------------------------------------------------------------------------

#[test]
fn multihop_query_improves_over_one_shot() {
    let (_dir, searcher) = fixture_searcher();
    let query = "ownership and borrowing then lifetimes";

    // One-shot: a single retrieve with the same limit.
    let one_shot = searcher.search(query, 1, None).unwrap();

    let report = run_deep_search(searcher, query, 1, now(), &NoopEmitter).unwrap();

    assert_eq!(report.model_calls, 0, "rules/policy suffice: no model call");
    assert!(
        report.result.hits.len() > one_shot.len(),
        "multi-hop navigation gathers more than one-shot: deep={} one_shot={}",
        report.result.hits.len(),
        one_shot.len()
    );
    // The navigation actually did more than a single retrieve.
    assert!(report.steps > 1, "multi-hop loops more than once");
    assert!(
        report.result.trace.iter().any(|t| t.contains("widen")),
        "widen broadened recall: {:?}",
        report.result.trace
    );
    // The trace strings are non-empty and one-per-step.
    assert_eq!(report.result.trace.len(), report.steps);
    assert!(report.result.trace.iter().all(|t| !t.is_empty()));
}

// ---------------------------------------------------------------------------
// (3) per-phase trace emission (PolicyTrace interface)
// ---------------------------------------------------------------------------

#[test]
fn deep_search_emits_phase_events_with_policy_tiers() {
    let (_dir, searcher) = fixture_searcher();
    let rec = Recorder::default();

    let report = run_deep_search(searcher, "rust", 5, now(), &rec).unwrap();
    assert!(report.is_fast_path());

    let events = rec.events.lock().unwrap().clone();
    let phases: Vec<Phase> = events.iter().map(|e| e.phase).collect();
    assert!(phases.contains(&Phase::Query), "a Query event is emitted");
    assert!(phases.contains(&Phase::Analyze), "an Analyze event is emitted");
    assert!(phases.contains(&Phase::Retrieve), "a Retrieve event is emitted");
    assert!(phases.contains(&Phase::Answer), "an Answer event is emitted");

    // A simple query is answered entirely by the rules tier.
    let retrieve = events
        .iter()
        .find(|e| e.phase == Phase::Retrieve)
        .expect("retrieve event");
    assert_eq!(retrieve.policy_tier.as_deref(), Some("rules"));
    // The final Answer event records the model-call count (0).
    let answer = events
        .iter()
        .find(|e| e.phase == Phase::Answer)
        .expect("answer event");
    assert_eq!(answer.detail.get("model_calls").map(String::as_str), Some("0"));
    assert!(events.iter().all(|e| !e.query_id.is_empty()));
}

// ---------------------------------------------------------------------------
// (4) read_doc round trip (loopback crawl fixture)
// ---------------------------------------------------------------------------

const PAGE_HTML: &str = "<!doctype html><html><head><title>Deep Fixture</title></head>\
<body><p>A zephyrquokka observation for the read path.</p></body></html>";

async fn page() -> Html<&'static str> {
    Html(PAGE_HTML)
}

async fn spawn_fixture() -> String {
    let app = Router::new().route("/page", get(page));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn test_config(dir: &std::path::Path) -> Config {
    let mut cfg = Config::default();
    cfg.data_dir = dir.to_path_buf();
    cfg.bind = "127.0.0.1:0".to_string();
    cfg
}

#[tokio::test]
async fn read_doc_returns_normalized_text_and_missing_is_not_found() {
    let base = spawn_fixture().await;
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());
    let p = search_agent0::Pipeline::open_with(&cfg, true).unwrap();

    let url = format!("{base}/page");
    let rid = p.resource_id_for(&url).unwrap();
    assert!(p.crawl_one(&url).await.unwrap().is_ingested());

    let doc = p.read_doc(&rid).unwrap();
    assert_eq!(doc.doc_id, rid);
    assert!(
        doc.text.to_lowercase().contains("zephyrquokka"),
        "normalized text is returned: {}",
        doc.text
    );

    // An unknown document id is an honest NotFound (-> HTTP 404 upstream).
    let err = p.read_doc("res_does_not_exist").unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
}
