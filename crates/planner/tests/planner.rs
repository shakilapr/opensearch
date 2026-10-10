//! Hermetic tests for the LLM query planner.
//!
//! No network, no credential, no sleeps: the LLM transport is a fake, so the cache, the mapping
//! and the `QueryPlanner` implementation are all exercised for real. The prompt and the response
//! parser belong to `search-organize` and are tested there (including against the live model).

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use search_engine::analyze::analyze_query;
use search_engine::QueryPlanner;
use search_organize::{LlmClient, LlmConfig, OrganizeError, Transport};
use search_planner::{plan_to_probes, with_client};

fn cfg(_dir: &Path) -> LlmConfig {
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
fn plan_returns_the_rewritten_query_as_a_probe_with_the_entities_in_the_notes() {
    let dir = tempfile::TempDir::new().unwrap();
    let transport = FakeTransport::new(
        r#"{"query": "+flash +attention +memory -economic", "entities": ["FlashAttention"], "phrases": ["flash attention"], "negatives": ["economic"]}"#,
    );
    let client = client_with(dir.path(), transport.clone());
    let planner = with_client(client, 3).expect("planner builds");

    let q = "how does flash attention reduce memory";
    let planned = planner.plan(q, &analyze_query(q, 0, 0));

    // `search-organize` folds an identified phrase into the query as `"a b"~3` (a live run showed
    // the model reporting phrases separately from the query, so a phrase would otherwise be lost
    // between the two). The probe is therefore the required terms *plus* the folded phrase.
    assert_eq!(planned.expansions.len(), 1);
    let probe = &planned.expansions[0];
    assert!(probe.starts_with("+flash +attention +memory -economic"), "{probe}");
    assert!(probe.contains("\"flash attention\"~3"), "the phrase must be folded in: {probe}");
    assert!(
        planned.notes.iter().any(|n| n.contains("entities: FlashAttention")),
        "the canonical entity must be visible in the trace: {:?}",
        planned.notes
    );
    assert!(planned.notes.iter().any(|n| n.contains("negatives: economic")), "{:?}", planned.notes);
}

#[test]
fn the_second_identical_query_is_served_from_the_cache() {
    let dir = tempfile::TempDir::new().unwrap();
    let transport = FakeTransport::new(r#"{"query": "+zeta"}"#);
    let client = client_with(dir.path(), transport.clone());
    let planner = with_client(client, 3).expect("planner builds");

    let q = "zeta";
    let analysis = analyze_query(q, 0, 0);
    let first = planner.plan(q, &analysis);
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
fn an_unparseable_response_becomes_an_empty_plan() {
    let dir = tempfile::TempDir::new().unwrap();
    let transport = FakeTransport::new("I am sorry, I cannot do that.");
    let client = client_with(dir.path(), transport);
    let planner = with_client(client, 3).expect("planner builds");

    let q = "anything";
    let planned = planner.plan(q, &analyze_query(q, 0, 0));

    assert!(planned.expansions.is_empty());
    assert!(planned.is_empty());
}

#[test]
fn an_unchanged_query_yields_no_probe_and_an_entity_only_plan_still_reports_it() {
    // A bare keyword search is returned unchanged by the planner: probing it again would search
    // the original twice. An entity-only plan (e.g. the question is just "vLLM?") carries no
    // probe but must still surface the entity, which is what the graph walk consumes.
    use search_organize::QueryPlan;

    let unchanged = plan_to_probes(
        Some(QueryPlan {
            query: "rust async".to_string(),
            entities: vec![],
            phrases: vec![],
            negatives: vec![],
        }),
        "rust async",
        3,
    );
    assert!(unchanged.expansions.is_empty(), "an unchanged query is not a probe");

    let entity_only = plan_to_probes(
        Some(QueryPlan {
            query: String::new(),
            entities: vec!["vLLM".to_string()],
            phrases: vec![],
            negatives: vec![],
        }),
        "vLLM?",
        3,
    );
    assert!(entity_only.expansions.is_empty());
    assert!(entity_only.notes.iter().any(|n| n.contains("vLLM")), "{:?}", entity_only.notes);
}

#[test]
fn a_missing_plan_is_the_identity() {
    assert!(plan_to_probes(None, "anything", 3).is_empty());
}
