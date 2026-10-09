//! Hermetic integration tests for `search-answer`.
//!
//! A [`FakeTransport`] returns canned response **bodies** and records how many times it was
//! called, so nothing here touches the network, reads a credential, or sleeps. The real HTTP
//! transport is never exercised; the parsing/`finish_reason` logic is fully reached because the
//! client parses the body the fake returns.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use search_answer::{
    AnswerConfig, AnswerError, LlmAnswerer, Transport, MAX_CHARS_PER_PASSAGE, MAX_PROMPT_CHARS,
};
use search_engine::evidence::{EvidenceRecord, Support};
use search_engine::Answerer;

/// Canned-body transport with observability: total calls and the last prompt it saw.
#[derive(Clone)]
struct FakeTransport {
    calls: Arc<AtomicU64>,
    last_prompt: Arc<Mutex<Option<String>>>,
    body: FakeBody,
}

/// Cloneable canned outcome (the real `AnswerError` is not `Clone`).
#[derive(Clone)]
enum FakeBody {
    Ok(String),
    Timeout(String),
}

impl FakeTransport {
    fn ok(body: impl Into<String>) -> Self {
        Self {
            calls: Arc::new(AtomicU64::new(0)),
            last_prompt: Arc::new(Mutex::new(None)),
            body: FakeBody::Ok(body.into()),
        }
    }

    fn timeout(msg: impl Into<String>) -> Self {
        Self {
            calls: Arc::new(AtomicU64::new(0)),
            last_prompt: Arc::new(Mutex::new(None)),
            body: FakeBody::Timeout(msg.into()),
        }
    }

    fn calls(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }

    fn last_prompt(&self) -> String {
        self.last_prompt.lock().unwrap().clone().unwrap_or_default()
    }
}

#[async_trait]
impl Transport for FakeTransport {
    async fn chat(&self, _model: &str, prompt: &str) -> Result<String, AnswerError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last_prompt.lock().unwrap() = Some(prompt.to_string());
        match &self.body {
            FakeBody::Ok(b) => Ok(b.clone()),
            FakeBody::Timeout(m) => Err(AnswerError::Timeout(m.clone())),
        }
    }
}

fn content_body(text: &str) -> String {
    serde_json::json!({
        "choices": [{ "finish_reason": "stop", "message": { "content": text } }]
    })
    .to_string()
}

fn config(dir: &std::path::Path) -> AnswerConfig {
    AnswerConfig {
        base_url: "http://test.local/v1".to_string(),
        model: "deepseek/deepseek-v4.1-flash".to_string(),
        timeout_ms: 20_000,
        max_tokens: 1024,
        concurrency: 4,
        cache_dir: dir.to_path_buf(),
    }
}

fn client(t: FakeTransport, dir: &std::path::Path) -> LlmAnswerer {
    LlmAnswerer::with_transport(config(dir), Arc::new(t)).unwrap()
}

fn evidence() -> Vec<EvidenceRecord> {
    vec![EvidenceRecord {
        unit_id: "u1".to_string(),
        doc_id: "doc-1".to_string(),
        status: Support::Supported,
        span: Some((0, 10)),
        doc_version: "v1".to_string(),
    }]
}

fn passages() -> Vec<String> {
    vec!["the rust async runtime".to_string(), "tokio is a runtime".to_string()]
}

// ---------------------------------------------------------------------------
// 1. Prompt-shape stability: a golden substring of the assembled prompt.
// ---------------------------------------------------------------------------
#[test]
fn prompt_shape_is_stable() {
    let dir = tempfile::tempdir().unwrap();
    let t = FakeTransport::ok(content_body("answer text"));
    let c = client(t.clone(), dir.path());

    let out = c.answer("what is rust async?", &evidence(), &passages()).unwrap();
    assert_eq!(out, "answer text");

    let prompt = t.last_prompt();
    assert!(prompt.contains("using ONLY the evidence and passages listed below"));
    assert!(prompt.contains("If the evidence is insufficient to answer, say so plainly"));
    assert!(prompt.contains("Cite the passage indices you used"));
    assert!(prompt.contains("Query:\nwhat is rust async?"));
    assert!(prompt.contains("[0] the rust async runtime"));
    assert_eq!(t.calls(), 1);
}

// ---------------------------------------------------------------------------
// 2. No key -> from_env returns None and the transport is never called.
// ---------------------------------------------------------------------------
#[test]
fn no_key_disables_and_never_calls_transport() {
    let _guard = ENV_LOCK.lock().unwrap();
    let saved_enabled = std::env::var("SEARCH_ANSWER_ENABLED").ok();
    let saved_key = std::env::var("KITANI_API_KEY").ok();
    std::env::set_var("SEARCH_ANSWER_ENABLED", "1");
    std::env::remove_var("KITANI_API_KEY");

    let t = FakeTransport::ok(content_body("never"));
    let built = search_answer::from_env_with_transport(Arc::new(t.clone()));
    assert!(built.is_none(), "without a key the answerer must be None");
    assert_eq!(t.calls(), 0, "the transport must never be called");
    assert_eq!(
        search_answer::disabled_reason(),
        search_answer::Disabled::MissingKey
    );

    // Feature disabled entirely (no key either way).
    std::env::remove_var("SEARCH_ANSWER_ENABLED");
    assert!(search_answer::from_env().is_none());
    assert_eq!(
        search_answer::disabled_reason(),
        search_answer::Disabled::FeatureDisabled
    );

    restore("SEARCH_ANSWER_ENABLED", saved_enabled);
    restore("KITANI_API_KEY", saved_key);
}

// ---------------------------------------------------------------------------
// 3. A cache hit performs no transport call (exactly one request across two calls).
// ---------------------------------------------------------------------------
#[test]
fn cache_hit_performs_no_transport_call() {
    let dir = tempfile::tempdir().unwrap();
    let t = FakeTransport::ok(content_body("cached answer"));
    let c = client(t.clone(), dir.path());

    let a = c.answer("q", &evidence(), &passages()).unwrap();
    let b = c.answer("q", &evidence(), &passages()).unwrap();

    assert_eq!(a, "cached answer");
    assert_eq!(b, a);
    assert_eq!(t.calls(), 1, "second call must be a cache hit: no transport call");

    let stats = c.cache_stats();
    assert_eq!(stats.hits, 1);
    assert_eq!(stats.misses, 1);
    assert_eq!(stats.entries, 1);
}

// ---------------------------------------------------------------------------
// 4. An empty/whitespace content response becomes Err.
// ---------------------------------------------------------------------------
#[test]
fn whitespace_content_becomes_err() {
    let dir = tempfile::tempdir().unwrap();
    let body = r#"{"choices":[{"finish_reason":"stop","message":{"content":"   "}}]}"#;
    let t = FakeTransport::ok(body);
    let c = client(t, dir.path());

    let err = c.answer("q", &evidence(), &passages()).unwrap_err();
    assert!(!err.is_empty());
    assert!(err.contains("no content"), "got: {err}");
}

// ---------------------------------------------------------------------------
// 5. finish_reason=length + null content becomes Err mentioning the token budget.
// ---------------------------------------------------------------------------
#[test]
fn length_finish_reason_mentions_budget() {
    let dir = tempfile::tempdir().unwrap();
    let body = r#"{"choices":[{"finish_reason":"length","message":{"content":null}}]}"#;
    let t = FakeTransport::ok(body);
    let c = client(t, dir.path());

    let err = c.answer("q", &evidence(), &passages()).unwrap_err();
    assert!(
        err.contains("SEARCH_ANSWER_MAX_TOKENS"),
        "error must mention the token budget, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// 6. The character cap truncates a huge passage set.
// ---------------------------------------------------------------------------
#[test]
fn huge_passage_set_is_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let t = FakeTransport::ok(content_body("ok"));
    let c = client(t.clone(), dir.path());

    let huge: Vec<String> = (0..50)
        .map(|i| format!("passage-{i} ").repeat(500))
        .collect();
    // Exactly one giant passage is itself capped per-passage; many passages are capped at the
    // total prompt budget.
    c.answer("q", &[], &huge).unwrap();

    let prompt = t.last_prompt();
    assert!(
        prompt.chars().count() <= MAX_PROMPT_CHARS,
        "prompt {} chars exceeds cap {MAX_PROMPT_CHARS}",
        prompt.chars().count()
    );
    assert!(prompt.contains("[0]"));
    // At most MAX_PASSAGES (8) passages are embedded, so index [8] never appears.
    assert!(!prompt.contains("[8]"), "passage count must be capped");

    // A single oversized passage is truncated to the per-passage cap.
    let dir2 = tempfile::tempdir().unwrap();
    let t2 = FakeTransport::ok(content_body("ok"));
    let c2 = client(t2.clone(), dir2.path());
    let one = vec!["x".repeat(MAX_CHARS_PER_PASSAGE * 3)];
    c2.answer("q", &[], &one).unwrap();
    let body = t2.last_prompt();
    let x_run = body.chars().filter(|c| *c == 'x').count();
    assert_eq!(x_run, MAX_CHARS_PER_PASSAGE, "per-passage cap must apply");
}

// ---------------------------------------------------------------------------
// 7. A transport timeout maps to Err without panicking.
// ---------------------------------------------------------------------------
#[test]
fn transport_timeout_maps_to_err() {
    let dir = tempfile::tempdir().unwrap();
    let t = FakeTransport::timeout("request timed out after 20000ms");
    let c = client(t, dir.path());

    let err = c.answer("q", &evidence(), &passages()).unwrap_err();
    assert!(err.contains("timed out"), "got: {err}");
}

// ---------------------------------------------------------------------------
// 8. The cache file is written atomically: a partial (temp-only) write is never visible.
// ---------------------------------------------------------------------------
#[test]
fn cache_write_is_atomic_and_partial_never_poisons() {
    let dir = tempfile::tempdir().unwrap();
    let t = FakeTransport::ok(content_body("final answer"));
    let c = client(t.clone(), dir.path());

    // First call populates the cache normally.
    c.answer("q", &evidence(), &passages()).unwrap();

    let model = "deepseek/deepseek-v4.1-flash";
    let prompt = search_answer::build_prompt("q", &evidence(), &passages());
    // Recompute the key the client used (model \0 prompt-version \0 prompt).
    let hash = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(model.as_bytes());
        h.update([0u8]);
        h.update(search_answer::PROMPT_VERSION.as_bytes());
        h.update([0u8]);
        h.update(prompt.as_bytes());
        hex::encode(h.finalize())
    };

    // Simulate a crash between writing the temp file and the atomic rename: write a *partial*
    // temp file that would be visible only if the rename had happened.
    let partial = search_answer::write_cache_temp_only(dir.path(), &hash, b"{ partial").unwrap();
    // The entry path still holds the good answer; the partial temp is not read as a cache entry.
    assert!(partial.exists(), "temp file was written");
    assert!(
        search_answer::read_cache(dir.path(), &hash).is_some(),
        "the good entry must remain readable"
    );

    // No stray *.json was created by the partial write, and re-answering hits the cache
    // (transport call count stays at 1).
    assert_eq!(t.calls(), 1);
    let stats = c.cache_stats();
    assert_eq!(stats.entries, 1, "exactly one cache entry on disk");

    // A corrupt *target* entry degrades to a miss (fresh transport call), never a hard error.
    let entry = search_answer::entry_path(dir.path(), &hash);
    std::fs::write(&entry, b"{ not json").unwrap();
    let t2 = FakeTransport::ok(content_body("recomputed"));
    let c2 = client(t2.clone(), dir.path());
    let got = c2.answer("q", &evidence(), &passages()).unwrap();
    assert_eq!(got, "recomputed");
    assert_eq!(t2.calls(), 1, "a corrupt entry degrades to a fresh call");
}

// ---------------------------------------------------------------------------
// A tiny env lock so the no-key test does not race the rest of the suite.
// ---------------------------------------------------------------------------
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn restore(name: &str, value: Option<String>) {
    match value {
        Some(v) => std::env::set_var(name, v),
        None => std::env::remove_var(name),
    }
}
