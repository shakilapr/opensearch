//! Integration tests for the engine route: `POST /search` with `engine: true`.
//!
//! Boots the real router on an ephemeral loopback port over a stub [`Searcher`]
//! and exercises it over TCP (hermetic: no network, no clock dependency).

use search_api::{router, AppState, IndexHit, Searcher};
use search_core::contract::Signal;
use search_core::{AppError, AppResult, IndexManifest, Provenance, SearchFilters};
use search_engine::Answerer;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// --- stubs ---------------------------------------------------------------

/// Deterministic stub corpus: `count` hits titled "rust doc {i}".
struct StubSearcher {
    hits: Vec<IndexHit>,
}

impl StubSearcher {
    fn new(count: usize) -> Self {
        let hits = (0..count)
            .map(|i| IndexHit {
                doc_id: format!("doc-{i:03}"),
                version: 1,
                resource_id: format!("res-{i:03}"),
                work_id: None,
                content_id: None,
                url: format!("https://example.test/rust/{i}"),
                title: format!("rust doc {i}"),
                snippet: format!("snippet about rust number {i}"),
                source_id: Some("stub".into()),
                doc_type: Some("html".into()),
                language: Some("en".into()),
                published_at: Some(1_700_000_000 + i as i64),
                modified_at: None,
                fetched_at: 1_700_000_500,
                lane: "lexical_base".into(),
                provenance: Provenance::Snapshot,
                score: (count - i) as f32,
                signals: vec![Signal {
                    name: "bm25".into(),
                    value: (count - i) as f32,
                    note: "stub".into(),
                }],
            })
            .collect();
        Self { hits }
    }
}

impl Searcher for StubSearcher {
    fn search(
        &self,
        _q: &str,
        limit: usize,
        filters: Option<&SearchFilters>,
    ) -> AppResult<Vec<IndexHit>> {
        let mut v: Vec<IndexHit> = self
            .hits
            .iter()
            .filter(|h| match filters.and_then(|f| f.source_id.as_deref()) {
                Some(src) => h.source_id.as_deref() == Some(src),
                None => true,
            })
            .cloned()
            .collect();
        v.truncate(limit);
        Ok(v)
    }

    fn manifest(&self) -> AppResult<IndexManifest> {
        Ok(IndexManifest {
            manifest_id: "manifest-7".into(),
            generation: 7,
            doc_count: self.hits.len() as u64,
            base_segments: 1,
            watermark: 42,
            embedding_model_ids: vec![],
            metadata_version: 1,
            created_at: 1_700_000_000,
            published_at: Some(1_700_000_001),
        })
    }
}

/// A searcher whose `search` always fails (but whose `manifest` is fine).
struct FailingSearcher;
impl Searcher for FailingSearcher {
    fn search(
        &self,
        _q: &str,
        _limit: usize,
        _filters: Option<&SearchFilters>,
    ) -> AppResult<Vec<IndexHit>> {
        Err(AppError::Internal("boom".into()))
    }
}

/// Answerer stub: echoes a fixed marker so the test can see it in `answer`.
struct StubAnswerer;
impl Answerer for StubAnswerer {
    fn answer(
        &self,
        query: &str,
        _evidence: &[search_engine::EvidenceRecord],
        _passages: &[String],
    ) -> Result<String, String> {
        Ok(format!("ANSWER for {query}"))
    }
}

/// Answerer stub that hands back the first passage verbatim, so a test can
/// assert what text the API actually resolved for the engine's doc ids.
struct FirstPassageAnswerer;
impl Answerer for FirstPassageAnswerer {
    fn answer(
        &self,
        _query: &str,
        _evidence: &[search_engine::EvidenceRecord],
        passages: &[String],
    ) -> Result<String, String> {
        Ok(passages.first().cloned().unwrap_or_default())
    }
}

// --- harness -------------------------------------------------------------

async fn boot(state: AppState) -> std::net::SocketAddr {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

async fn call(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> (u16, serde_json::Value) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let body = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf);
    let status: u16 = text.split_whitespace().nth(1).expect("status line").parse().unwrap();
    let raw_body = text.split("\r\n\r\n").nth(1).unwrap_or("");
    let json = serde_json::from_str(raw_body).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn simple_boot(count: usize) -> std::net::SocketAddr {
    boot(AppState::new(Arc::new(StubSearcher::new(count)))).await
}

// --- tests ---------------------------------------------------------------

#[tokio::test]
async fn engine_absent_preserves_legacy_response_shape() {
    let addr = simple_boot(5).await;
    let (status, body) = call(addr, "POST", "/search", Some(r#"{"query":"rust"}"#)).await;
    assert_eq!(status, 200, "legacy search: {body}");
    // The legacy reproducibility/response fields are present…
    assert!(body["query_analysis"].is_object());
    assert!(body["results"].is_array());
    assert!(!body["snapshot_id"].as_str().unwrap().is_empty());
    assert_eq!(body["index_manifest_id"], "manifest-7");
    // …and NONE of the additive engine fields leak into the legacy shape.
    for k in ["engine", "coverage", "stop_reason", "trace", "answer"] {
        assert!(body.get(k).is_none(), "legacy response must not carry `{k}`: {body}");
    }
    // The legacy per-result shape carries no engine-only keys either.
    for r in body["results"].as_array().unwrap() {
        assert!(r.get("lane_ranks").is_none());
        assert!(r.get("unit_support").is_none());
    }
}

#[tokio::test]
async fn engine_true_returns_ranked_results_with_lane_ranks() {
    let addr = simple_boot(5).await;
    let (status, body) =
        call(addr, "POST", "/search", Some(r#"{"query":"rust","engine":true}"#)).await;
    assert_eq!(status, 200, "engine search: {body}");
    assert_eq!(body["engine"], true);

    let results = body["results"].as_array().expect("results array");
    assert!(!results.is_empty(), "engine returned ranked results");
    for r in results {
        // The legacy fields are still present on every result…
        assert!(r["doc_id"].is_string());
        assert!(r["canonical_url"].is_string());
        assert!(r["title"].is_string());
        // …plus the additive provenance.
        assert!(r["lane_ranks"].is_object(), "lane_ranks on {r}");
        assert!(r["unit_support"].is_array(), "unit_support on {r}");
    }
    // A valid stop reason string from the §17 vocabulary.
    let stop = body["stop_reason"].as_str().expect("stop_reason");
    assert!(
        [
            "deadline",
            "budget_exhausted",
            "sufficient_coverage",
            "no_admissible_action",
            "cancelled",
            "plateau"
        ]
        .contains(&stop),
        "unexpected stop_reason {stop}"
    );
    assert!(body["trace"].as_array().is_some());
}

#[tokio::test]
async fn engine_on_empty_index_returns_empty_results_ok() {
    let addr = simple_boot(0).await;
    let (status, body) =
        call(addr, "POST", "/search", Some(r#"{"query":"rust","engine":true}"#)).await;
    assert_eq!(status, 200, "empty index must not error: {body}");
    assert_eq!(body["results"].as_array().unwrap().len(), 0);
    assert!(body["stop_reason"].as_str().is_some());
}

#[tokio::test]
async fn mode_is_honoured_in_trace() {
    let addr = simple_boot(5).await;
    let (s, fast) = call(
        addr,
        "POST",
        "/search",
        Some(r#"{"query":"rust","engine":true,"mode":"fast"}"#),
    )
    .await;
    assert_eq!(s, 200, "fast: {fast}");
    let (s, deep) = call(
        addr,
        "POST",
        "/search",
        Some(r#"{"query":"rust","engine":true,"mode":"deep"}"#),
    )
    .await;
    assert_eq!(s, 200, "deep: {deep}");

    let trace0 = |b: &serde_json::Value| b["trace"][0].as_str().unwrap_or("").to_string();
    assert!(trace0(&fast).contains("Fast"), "fast trace: {}", trace0(&fast));
    assert!(trace0(&deep).contains("Deep"), "deep trace: {}", trace0(&deep));
}

#[tokio::test]
async fn answerer_text_appears_and_results_unchanged() {
    // With answerer.
    let addr_a = boot(
        AppState::new(Arc::new(StubSearcher::new(5)))
            .with_answerer(Arc::new(StubAnswerer)),
    )
    .await;
    let (s, with) = call(
        addr_a,
        "POST",
        "/search",
        Some(r#"{"query":"rust","engine":true}"#),
    )
    .await;
    assert_eq!(s, 200, "with answerer: {with}");
    assert_eq!(with["answer"], "ANSWER for rust");

    // Without answerer.
    let addr_b = simple_boot(5).await;
    let (s, without) = call(
        addr_b,
        "POST",
        "/search",
        Some(r#"{"query":"rust","engine":true}"#),
    )
    .await;
    assert_eq!(s, 200, "without answerer: {without}");
    assert!(without["answer"].is_null());

    // Results are identical (compare the doc_id order, which the engine fixes).
    let ids = |b: &serde_json::Value| -> Vec<String> {
        b["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["doc_id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(ids(&with), ids(&without), "answerer must not change results");
}

#[tokio::test]
async fn answerer_receives_resolved_passage_text_not_doc_ids() {
    // Regression: the engine passes doc ids as "passages"; the API must resolve
    // them to real title+snippet text before the answerer sees them, otherwise an
    // LLM gets identifiers it cannot ground an answer in.
    let addr = boot(
        AppState::new(Arc::new(StubSearcher::new(3)))
            .with_answerer(Arc::new(FirstPassageAnswerer)),
    )
    .await;
    let (s, b) = call(
        addr,
        "POST",
        "/search",
        Some(r#"{"query":"rust","engine":true,"mode":"fast","limit":3}"#),
    )
    .await;
    assert_eq!(s, 200, "{b}");
    let answer = b["answer"].as_str().unwrap_or_default();
    assert!(
        !answer.is_empty(),
        "answerer saw no passages at all: {b}"
    );
    assert!(
        !answer.starts_with("doc-"),
        "passage is still a bare doc id: {answer}"
    );
    // The stub corpus titles/snippets are "rust doc {i}" / "snippet about rust number {i}".
    assert!(
        answer.contains("rust doc") && answer.contains("snippet about rust"),
        "passage text was not resolved from the retained hits: {answer}"
    );
}

#[tokio::test]
async fn answerer_excerpts_the_stored_body_around_the_query_terms() {
    // With no retained fragment the passage must come from the page body, and
    // must be excerpted around the query terms rather than taken from the head
    // (the head of a scraped page is navigation — a live answerer read it as
    // "table of contents").
    struct BodySearcher(StubSearcher);
    impl Searcher for BodySearcher {
        fn search(
            &self,
            q: &str,
            limit: usize,
            filters: Option<&SearchFilters>,
        ) -> AppResult<Vec<IndexHit>> {
            let mut hits = self.0.search(q, limit, filters)?;
            // Strip the fragments so resolution has to fall back to the body.
            for h in hits.iter_mut() {
                h.snippet = String::new();
            }
            Ok(hits)
        }
        fn doc_text(&self, _doc_id: &str) -> AppResult<Option<String>> {
            let body = format!(
                "{}{}",
                "nav home about contact ".repeat(40),
                "Transformers replaced recurrence in 2017 and now dominate NLP."
            );
            Ok(Some(body))
        }
    }

    let addr = boot(
        AppState::new(Arc::new(BodySearcher(StubSearcher::new(2))))
            .with_answerer(Arc::new(FirstPassageAnswerer)),
    )
    .await;
    let (s, b) = call(
        addr,
        "POST",
        "/search",
        Some(r#"{"query":"transformers nlp","engine":true,"mode":"fast","limit":2}"#),
    )
    .await;
    assert_eq!(s, 200, "{b}");
    let answer = b["answer"].as_str().unwrap_or_default();
    assert!(
        answer.contains("Transformers replaced recurrence"),
        "body excerpt missing from the passage: {answer}"
    );
    assert!(
        !answer.starts_with("nav home about"),
        "the passage is the page head, not an excerpt around the query terms: {answer}"
    );
}

/// A searcher that can serve stored bodies, used to prove that §16 coverage is
/// verified against real document text and that question words in the query
/// ("what", "is") do not make verification fail.
struct TextSearcher(StubSearcher);

impl Searcher for TextSearcher {
    fn search(
        &self,
        q: &str,
        limit: usize,
        filters: Option<&SearchFilters>,
    ) -> AppResult<Vec<IndexHit>> {
        self.0.search(q, limit, filters)
    }

    fn doc_text(&self, _doc_id: &str) -> AppResult<Option<String>> {
        Ok(Some(
            "retrieval augmented generation grounds a model on a corpus".into(),
        ))
    }
}

#[tokio::test]
async fn coverage_is_verified_against_document_text_not_titles() {
    // Neither title nor snippet says "augmented", and the query carries the
    // function words "what"/"is": with title-only matching this scored 0.0
    // coverage against 115 matching documents in a live run.
    let addr = boot(AppState::new(Arc::new(TextSearcher(StubSearcher::new(3))))).await;
    let (s, b) = call(
        addr,
        "POST",
        "/search",
        Some(r#"{"query":"what is retrieval augmented generation","engine":true,"mode":"fast","limit":3}"#),
    )
    .await;
    assert_eq!(s, 200, "{b}");
    let coverage = b["coverage"]["coverage"].as_f64().unwrap_or(-1.0);
    assert!(
        coverage > 0.0,
        "coverage must be > 0 when the stored text supports the unit, got {coverage}: {b}"
    );
    let unsupported = b["coverage"]["unsupported"].as_array().cloned().unwrap_or_default();
    assert!(
        unsupported.is_empty(),
        "no unit should be unsupported once verified against text: {unsupported:?}"
    );
}

#[tokio::test]
async fn limit_clamping_applies_on_engine_path() {
    let addr = simple_boot(150).await;
    // 500 is clamped to the 100 cap.
    let (s, b) = call(
        addr,
        "POST",
        "/search",
        Some(r#"{"query":"rust","engine":true,"limit":500}"#),
    )
    .await;
    assert_eq!(s, 200, "engine clamp: {b}");
    let n = b["results"].as_array().unwrap().len();
    assert!(n > 0 && n <= 100, "engine results must be clamped to MAX_LIMIT, got {n}");

    // 0 is meaningless: 400, same as the legacy path.
    let (s, b) = call(
        addr,
        "POST",
        "/search",
        Some(r#"{"query":"rust","engine":true,"limit":0}"#),
    )
    .await;
    assert_eq!(s, 400, "zero limit: {b}");
}

#[tokio::test]
async fn failing_searcher_does_not_500_engine_route() {
    let addr = boot(AppState::new(Arc::new(FailingSearcher))).await;
    let (s, b) = call(
        addr,
        "POST",
        "/search",
        Some(r#"{"query":"rust","engine":true}"#),
    )
    .await;
    assert_eq!(s, 200, "a failing lane must degrade, not 500: {b}");
    assert_eq!(b["results"].as_array().unwrap().len(), 0);
    // The failure is surfaced in the trace, not hidden.
    let trace = b["trace"].as_array().unwrap();
    assert!(
        trace.iter().any(|t| t.as_str().unwrap_or("").contains("lane failed")),
        "trace should report the lane failure: {trace:?}"
    );
}

#[tokio::test]
async fn lane_ranks_keys_are_stable_lowercase() {
    let addr = simple_boot(5).await;
    let (s, b) = call(
        addr,
        "POST",
        "/search",
        Some(r#"{"query":"rust","engine":true}"#),
    )
    .await;
    assert_eq!(s, 200, "engine: {b}");
    let first = &b["results"][0]["lane_ranks"];
    let map = first.as_object().expect("lane_ranks object");
    let keys: Vec<&str> = map.keys().map(|k| k.as_str()).collect();
    // Exact, stable lowercase keys — never the Rust Debug form.
    assert_eq!(keys, vec!["bm25", "source_first"], "keys: {keys:?}");
    for (k, v) in map {
        assert_eq!(k.as_str(), k.to_lowercase().as_str(), "lane key must be lowercase");
        assert!(v.as_u64().unwrap() >= 1, "rank starts at 1");
    }
}

#[tokio::test]
async fn engine_empty_query_is_400() {
    let addr = simple_boot(3).await;
    let (s, b) = call(
        addr,
        "POST",
        "/search",
        Some(r#"{"query":"   ","engine":true}"#),
    )
    .await;
    assert_eq!(s, 400, "engine empty query: {b}");
    assert!(b["error"].as_str().unwrap().contains("query"));
}
