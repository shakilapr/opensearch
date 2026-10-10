//! Hermetic tests for the LLM query planner.
//!
//! No network, no credential, no sleeps: the LLM transport is a fake, so the cache, the parse and
//! the `QueryPlanner` implementation are all exercised for real.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use search_engine::analyze::analyze_query;
use search_engine::QueryPlanner;
use search_organize::{LlmClient, LlmConfig, OrganizeError, Transport};
use search_planner::{parse_plan, with_client};

fn cfg(dir: &Path) -> LlmConfig {
    LlmConfig {
        base_url: "http://planner.test/v1".to_string(),
        model: "test-model".to_string(),
        escalate_model: "test-model".to_string(),
        api_key: None,
        timeout_s: 5,
    }
}

/// A transport that returns a fixed assistant content and counts how often it was asked.
struct FakeTransport {
    body: String,
    calls: AtomicUsize,
}

impl FakeTransport {
    fn new(body: &str) -> Arc<Self> {
        Arc::new(Self { body: body.to_string(), calls: AtomicUsize::new(0) })
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Transport for FakeTransport {
    async fn chat(&self, _model: &str, _prompt: &str) -> Result<String, OrganizeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.body.clone())
    }
}

/// A transport that always fails.
struct FailingTransport;

#[async_trait]
impl Transport for FailingTransport {
    async fn chat(&self, _model: &str, _prompt: &str) -> Result<String, OrganizeError> {
        Err(OrganizeError::Http("boom".to_string()))
    }
}

fn client_with(dir: &Path, transport: Arc<dyn Transport>) -> Arc<LlmClient> {
    Arc::new(
        LlmClient::with_transport(cfg(dir), dir.join("cache"), 2, transport)
            .expect("client builds"),
    )
}

#[test]
fn plan_returns_expansions_and_the_second_identical_query_is_cached() {
    let dir = tempfile::TempDir::new().unwrap();
    let transport = FakeTransport::new(
        r#"{"expansions": ["paged attention", "\"memory complexity\"~3"], "notes": "entities + phrase"}"#,
    );
    let client = client_with(dir.path(), transport.clone());
    let planner = with_client(client, 3).expect("planner builds");

    let q = "how does flash attention reduce memory";
    let analysis = analyze_query(q, 0, 0);

    let first = planner.plan(q, &analysis);
    assert_eq!(
        first.expansions,
        vec!["paged attention".to_string(), "\"memory complexity\"~3".to_string()]
    );
    assert_eq!(first.notes, vec!["entities + phrase".to_string()]);

    // The same query must not pay the model twice.
    let second = planner.plan(q, &analysis);
    assert_eq!(second, first);
    assert_eq!(transport.calls(), 1, "the second identical plan must be served from the cache");
}

#[test]
fn a_transport_failure_becomes_an_empty_plan_with_a_note_not_a_panic() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = client_with(dir.path(), Arc::new(FailingTransport));
    let planner = with_client(client, 3).expect("planner builds");

    let q = "anything";
    let planned = planner.plan(q, &analyze_query(q, 0, 0));

    assert!(planned.expansions.is_empty(), "no expansions on failure");
    assert!(
        planned.notes.iter().any(|n| n.contains("boom")),
        "the failure must be visible in the notes: {:?}",
        planned.notes
    );
}

#[test]
fn an_unparseable_response_becomes_an_empty_plan_with_a_note() {
    let dir = tempfile::TempDir::new().unwrap();
    let transport = FakeTransport::new("I am sorry, I cannot do that.");
    let client = client_with(dir.path(), transport);
    let planner = with_client(client, 3).expect("planner builds");

    let q = "anything";
    let planned = planner.plan(q, &analyze_query(q, 0, 0));

    assert!(planned.expansions.is_empty());
    assert!(
        planned.notes.iter().any(|n| n.contains("unparseable")),
        "the parse failure must be reported: {:?}",
        planned.notes
    );
}

#[test]
fn the_planner_never_returns_more_than_its_cap() {
    // Even if the model over-returns, the cap holds. The engine caps again on top.
    let body = r#"{"expansions": ["a", "b", "c", "d", "e"]}"#;
    let p = parse_plan(body, 2, "q").unwrap();
    assert_eq!(p.expansions, vec!["a".to_string(), "b".to_string()]);
}
