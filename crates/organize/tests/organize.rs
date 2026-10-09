//! Hermetic integration tests for `search-organize`.
//!
//! A [`FakeTransport`] returns canned JSON and records invocation counts, so nothing here
//! touches the network, reads a credential, or sleeps.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use search_organize::{
    cache_entry_path, organize_all, organize_page, LlmClient, LlmConfig, OrganizeError, RawPage,
    Transport,
};

const PRIMARY: &str = "deepseek/deepseek-v4.1-flash";
const ESCALATE: &str = "deepseek/deepseek-v4.1-flash-thinking";

/// A canned-response transport with observability: total calls, per-model calls, and the maximum
/// number of simultaneous in-flight calls (to prove bounded parallelism).
#[derive(Clone)]
struct FakeTransport {
    calls: Arc<AtomicU64>,
    in_flight: Arc<AtomicU64>,
    max_in_flight: Arc<AtomicU64>,
    per_model: Arc<Mutex<HashMap<String, u64>>>,
    responses: Arc<HashMap<String, String>>,
    default_response: String,
    yields: u32,
}

impl FakeTransport {
    fn new(default_response: String, responses: HashMap<String, String>, yields: u32) -> Self {
        Self {
            calls: Arc::new(AtomicU64::new(0)),
            in_flight: Arc::new(AtomicU64::new(0)),
            max_in_flight: Arc::new(AtomicU64::new(0)),
            per_model: Arc::new(Mutex::new(HashMap::new())),
            responses: Arc::new(responses),
            default_response,
            yields,
        }
    }

    fn calls(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }

    fn per_model(&self, model: &str) -> u64 {
        self.per_model
            .lock()
            .unwrap()
            .get(model)
            .copied()
            .unwrap_or(0)
    }

    fn max_in_flight(&self) -> u64 {
        self.max_in_flight.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Transport for FakeTransport {
    async fn chat(&self, model: &str, _prompt: &str) -> Result<String, OrganizeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        {
            let mut m = self.per_model.lock().unwrap();
            *m.entry(model.to_string()).or_insert(0) += 1;
        }
        let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(current, Ordering::SeqCst);

        // Yield (never sleep) so concurrent workers actually interleave, letting the max
        // in-flight counter observe the effect of the concurrency semaphore.
        for _ in 0..self.yields {
            tokio::task::yield_now().await;
        }

        self.in_flight.fetch_sub(1, Ordering::SeqCst);

        Ok(self
            .responses
            .get(model)
            .cloned()
            .unwrap_or_else(|| self.default_response.clone()))
    }
}

fn good_json() -> String {
    r#"{"title":"Organized Title","summary":"A concise summary.","topics":["rust","async"],"doc_type":"article","language":"en","key_entities":["Rust","Tokio"],"id_hint":"organized-title"}"#.to_string()
}

fn config() -> LlmConfig {
    LlmConfig {
        base_url: "http://test.local/v1".to_string(),
        model: PRIMARY.to_string(),
        escalate_model: ESCALATE.to_string(),
        api_key: Some("test-key".to_string()),
        timeout_s: 5,
    }
}

fn page(url: &str, text: &str) -> RawPage {
    RawPage {
        url: url.to_string(),
        source: "crawler".to_string(),
        title: None,
        text: text.to_string(),
        published_at: Some(1_700_000_000),
    }
}

fn client(transport: FakeTransport, dir: &std::path::Path, concurrency: usize) -> LlmClient {
    LlmClient::with_transport(config(), dir.to_path_buf(), concurrency, Arc::new(transport)).unwrap()
}

// ---------------------------------------------------------------------------
// 1. A valid canned response produces the expected record and posts exactly once.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn valid_response_produces_record_and_posts_once() {
    let dir = tempfile::tempdir().unwrap();
    let t = FakeTransport::new(good_json(), HashMap::new(), 0);
    let c = client(t.clone(), dir.path(), 4);

    let p = page("https://example.com/a", "raw page text about rust async");
    let records = organize_page(&c, &p).await.unwrap();

    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert_eq!(r.url.as_deref(), Some("https://example.com/a"));
    assert_eq!(r.source, "crawler");
    assert_eq!(r.title.as_deref(), Some("Organized Title"));
    assert_eq!(r.text, "raw page text about rust async");
    assert_eq!(r.published_at, Some(1_700_000_000));
    assert_eq!(r.extras.get("summary").unwrap(), "A concise summary.");
    assert_eq!(r.extras.get("topics").unwrap(), "rust,async");
    assert_eq!(r.extras.get("doc_type").unwrap(), "article");
    assert_eq!(r.extras.get("language").unwrap(), "en");
    assert_eq!(r.extras.get("key_entities").unwrap(), "Rust,Tokio");
    assert_eq!(r.extras.get("organizer_model").unwrap(), PRIMARY);

    assert_eq!(t.calls(), 1, "exactly one transport call for one page");
    assert_eq!(t.per_model(PRIMARY), 1);
    assert_eq!(t.per_model(ESCALATE), 0);
}

// ---------------------------------------------------------------------------
// 2. The same prompt twice: second is served from cache, transport count unchanged.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn same_prompt_is_served_from_cache() {
    let dir = tempfile::tempdir().unwrap();
    let t = FakeTransport::new(good_json(), HashMap::new(), 0);
    let c = client(t.clone(), dir.path(), 4);

    let first = c.complete("shared prompt").await.unwrap();
    let second = c.complete("shared prompt").await.unwrap();

    assert_eq!(first, second);
    assert_eq!(t.calls(), 1, "cache hit must make NO transport call");
    assert_eq!(t.per_model(PRIMARY), 1);

    let stats = c.cache_stats();
    assert_eq!(stats.hits, 1, "second call must register a cache hit");
    assert_eq!(stats.misses, 1);
    assert_eq!(stats.entries, 1);

    // File layout is exactly <dir>/<first two hex>/<full hash>.json.
    let path = cache_entry_path(dir.path(), "http://test.local/v1", PRIMARY, "shared prompt");
    assert!(path.exists(), "cached entry file must exist at {path:?}");
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    assert!(name.ends_with(".json"));
    assert_eq!(name.len(), 64 + ".json".len());
    let sub = path.parent().unwrap().file_name().unwrap().to_string_lossy().to_string();
    assert_eq!(sub.len(), 2);
    assert_eq!(name[..2], sub[..]);
}

// ---------------------------------------------------------------------------
// 3. Invalid primary JSON escalates exactly once; the good thinking answer wins.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn invalid_primary_json_escalates_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut responses = HashMap::new();
    responses.insert(PRIMARY.to_string(), "this is definitely not json".to_string());
    responses.insert(ESCALATE.to_string(), good_json());
    let t = FakeTransport::new(String::new(), responses, 0);
    let c = client(t.clone(), dir.path(), 4);

    let records = organize_page(&c, &page("https://example.com/b", "some body")).await.unwrap();

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].extras.get("organizer_model").unwrap(), ESCALATE);
    assert_eq!(t.per_model(PRIMARY), 1, "primary asked exactly once");
    assert_eq!(t.per_model(ESCALATE), 1, "escalated exactly once");
    assert_eq!(t.calls(), 2);
}

// ---------------------------------------------------------------------------
// 4. organize_all: stable order, N records, and never exceeds the configured concurrency.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn organize_all_is_stable_and_bounds_concurrency() {
    // Part A: concurrency = 1 must serialize transport calls (max in-flight exactly 1).
    let dir_a = tempfile::tempdir().unwrap();
    let ta = FakeTransport::new(good_json(), HashMap::new(), 3);
    let ca = client(ta.clone(), dir_a.path(), 1);
    let pages_a: Vec<RawPage> = (0..8)
        .map(|i| page(&format!("https://example.com/{i}"), &format!("page body {i}")))
        .collect();

    let records_a = organize_all(&ca, pages_a).await.unwrap();
    assert_eq!(records_a.len(), 8);
    assert_eq!(ta.calls(), 8);
    assert_eq!(
        ta.max_in_flight(),
        1,
        "concurrency=1 must never overlap transport calls"
    );
    for (i, r) in records_a.iter().enumerate() {
        assert_eq!(r.url.as_deref(), Some(format!("https://example.com/{i}").as_str()));
    }

    // Part B: concurrency = 4 over 12 pages stays within the bound and returns all records.
    let dir_b = tempfile::tempdir().unwrap();
    let tb = FakeTransport::new(good_json(), HashMap::new(), 3);
    let cb = client(tb.clone(), dir_b.path(), 4);
    let pages_b: Vec<RawPage> = (0..12)
        .map(|i| page(&format!("https://example.org/{i}"), &format!("body {i}")))
        .collect();

    let records_b = organize_all(&cb, pages_b).await.unwrap();
    assert_eq!(records_b.len(), 12);
    assert_eq!(tb.calls(), 12);
    assert!(
        tb.max_in_flight() <= 4,
        "max in-flight {} exceeded concurrency 4",
        tb.max_in_flight()
    );
    for (i, r) in records_b.iter().enumerate() {
        assert_eq!(r.url.as_deref(), Some(format!("https://example.org/{i}").as_str()));
    }
}

// ---------------------------------------------------------------------------
// 5. from_env() without KITANI_API_KEY errors mentioning that variable.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn from_env_errors_without_api_key() {
    let saved = std::env::var("KITANI_API_KEY").ok();
    std::env::remove_var("KITANI_API_KEY");

    let err = LlmConfig::from_env().expect_err("must fail without KITANI_API_KEY");
    let msg = err.to_string();
    assert!(
        msg.contains("KITANI_API_KEY"),
        "error must name the variable, got: {msg}"
    );
    assert!(matches!(err, OrganizeError::MissingEnv(ref v) if v == "KITANI_API_KEY"));

    if let Some(v) = saved {
        std::env::set_var("KITANI_API_KEY", v);
    }
}
