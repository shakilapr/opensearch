//! Integration tests: boot the real router with a stub `Searcher` via
//! `axum::serve` on an ephemeral loopback port and exercise it over TCP.

use search_api::{router, AppState, IndexHit, Searcher};
use search_core::contract::Signal;
use search_core::{AppError, AppResult, IndexManifest, Provenance, SearchFilters};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A deterministic stub corpus: `count` hits titled "rust doc {i}".
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
        q: &str,
        limit: usize,
        filters: Option<&SearchFilters>,
    ) -> AppResult<Vec<IndexHit>> {
        let q = q.to_lowercase();
        let mut v: Vec<IndexHit> = self
            .hits
            .iter()
            .filter(|h| h.title.to_lowercase().contains(&q))
            .filter(|h| match filters.and_then(|f| f.source_id.as_deref()) {
                Some(src) => h.source_id.as_deref() == Some(src),
                None => true,
            })
            .cloned()
            .collect();
        v.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
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

/// A searcher that always fails, to exercise error mapping.
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

/// A searcher that always reports `AppError::Deadline` from `search`.
struct DeadlineSearcher;
impl Searcher for DeadlineSearcher {
    fn search(
        &self,
        _q: &str,
        _limit: usize,
        _filters: Option<&SearchFilters>,
    ) -> AppResult<Vec<IndexHit>> {
        Err(AppError::Deadline)
    }
}

/// Wraps the stub with a delay so concurrent load exceeds the shed threshold.
struct SlowSearcher {
    inner: StubSearcher,
}
impl Searcher for SlowSearcher {
    fn search(
        &self,
        q: &str,
        limit: usize,
        filters: Option<&SearchFilters>,
    ) -> AppResult<Vec<IndexHit>> {
        std::thread::sleep(std::time::Duration::from_millis(400));
        self.inner.search(q, limit, filters)
    }
    fn manifest(&self) -> AppResult<IndexManifest> {
        self.inner.manifest()
    }
}

/// A searcher that sleeps long enough for any small deadline to be blown.
struct SleepySearcher;
impl Searcher for SleepySearcher {
    fn search(
        &self,
        _q: &str,
        _limit: usize,
        _filters: Option<&SearchFilters>,
    ) -> AppResult<Vec<IndexHit>> {
        std::thread::sleep(std::time::Duration::from_millis(60));
        Ok(Vec::new())
    }
}

async fn boot(searcher: Arc<dyn Searcher + Send + Sync>) -> std::net::SocketAddr {
    let app = router(AppState::new(searcher));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// Minimal HTTP/1.1 client (no external HTTP dep): one request per connection.
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
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .expect("status line")
        .parse()
        .unwrap();
    let raw_body = text.split("\r\n\r\n").nth(1).unwrap_or("");
    let json = serde_json::from_str(raw_body).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn health_reports_generation_and_uptime() {
    let addr = boot(Arc::new(StubSearcher::new(5))).await;
    let (status, body) = call(addr, "GET", "/health", None).await;
    assert_eq!(status, 200, "health: {body}");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["generation"], 7);
    assert_eq!(body["index_manifest_id"], "manifest-7");
    assert!(body["uptime_seconds"].is_number());
}

#[tokio::test]
async fn search_returns_ranked_results() {
    let addr = boot(Arc::new(StubSearcher::new(150))).await;
    let (status, body) = call(addr, "POST", "/search", Some(r#"{"query":"rust"}"#)).await;
    assert_eq!(status, 200, "search: {body}");

    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 10, "default limit is 10");

    // Ranked: strictly descending score, and the top hit carries provenance.
    let mut prev = f64::INFINITY;
    for r in results {
        let score = r["score"].as_f64().unwrap();
        assert!(score <= prev, "scores must be non-increasing");
        prev = score;
        assert_eq!(r["provenance"], "snapshot");
        let signals = r["signals"].as_array().expect("signals array");
        assert!(!signals.is_empty());
        // The API guarantees the five Stage-1 signals for "Why this result?".
        let names: Vec<&str> = signals
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        for want in [
            "bm25",
            "source_authority",
            "freshness",
            "entity_match",
            "graph_expansion",
        ] {
            assert!(names.contains(&want), "missing signal {want}: {names:?}");
        }
        // `live_overlay` is derived from the frozen lane/provenance fields.
        assert_eq!(r["lane"], "lexical_base");
    }
    assert_eq!(results[0]["score"].as_f64().unwrap(), 150.0);

    // Reproducibility tuple is populated from the manifest.
    assert_eq!(body["index_manifest_id"], "manifest-7");
    assert_eq!(body["overlay_watermark"], 42);
    assert!(!body["snapshot_id"].as_str().unwrap().is_empty());
    assert_eq!(body["query_analysis"]["intent"], "factual");
}

#[tokio::test]
async fn limit_is_clamped_and_zero_rejected() {
    let addr = boot(Arc::new(StubSearcher::new(150))).await;

    let (s, b) = call(addr, "POST", "/search", Some(r#"{"query":"rust","limit":5}"#)).await;
    assert_eq!(s, 200);
    assert_eq!(b["results"].as_array().unwrap().len(), 5);

    // 101 is clamped to the 100 cap rather than rejected.
    let (s, b) = call(
        addr,
        "POST",
        "/search",
        Some(r#"{"query":"rust","limit":101}"#),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(b["results"].as_array().unwrap().len(), 100);

    // 0 is meaningless and rejected with 400.
    let (s, b) = call(addr, "POST", "/search", Some(r#"{"query":"rust","limit":0}"#)).await;
    assert_eq!(s, 400, "body: {b}");
    assert!(b["error"].as_str().unwrap().contains("limit"));
}

#[tokio::test]
async fn invalid_body_and_empty_query_are_400() {
    let addr = boot(Arc::new(StubSearcher::new(3))).await;

    let (s, _) = call(addr, "POST", "/search", Some("{not json")).await;
    assert_eq!(s, 400);

    let (s, b) = call(addr, "POST", "/search", Some(r#"{"query":"   "}"#)).await;
    assert_eq!(s, 400, "body: {b}");
    assert!(b["error"].as_str().unwrap().contains("query"));
}

#[tokio::test]
async fn searcher_failure_maps_to_500() {
    let addr = boot(Arc::new(FailingSearcher)).await;
    let (s, b) = call(addr, "POST", "/search", Some(r#"{"query":"rust"}"#)).await;
    assert_eq!(s, 500, "body: {b}");
    assert!(b["error"].as_str().unwrap().contains("boom"));
}

#[tokio::test]
async fn saturating_concurrency_sheds_with_429() {
    let addr = boot(Arc::new(SlowSearcher {
        inner: StubSearcher::new(3),
    }))
    .await;

    let mut handles = Vec::new();
    for _ in 0..200 {
        handles.push(tokio::spawn(async move {
            let (s, _) = call(addr, "POST", "/search", Some(r#"{"query":"rust"}"#)).await;
            s
        }));
    }
    let mut ok = 0;
    let mut shed = 0;
    for h in handles {
        match h.await.unwrap() {
            200 => ok += 1,
            429 => shed += 1,
            other => panic!("unexpected status {other}"),
        }
    }
    assert!(ok >= 1, "at least some requests must succeed");
    assert!(
        shed >= 1,
        "a saturated server must shed load with 429 (ok={ok}, shed={shed})"
    );
}

#[tokio::test]
async fn deadline_maps_to_504() {
    // A searcher that reports `Deadline` maps straight to 504.
    let addr = boot(Arc::new(DeadlineSearcher)).await;
    let (s, b) = call(addr, "POST", "/search", Some(r#"{"query":"rust"}"#)).await;
    assert_eq!(s, 504, "body: {b}");
    assert!(b["error"].as_str().unwrap().contains("deadline"));

    // A search that overruns its `deadline_ms` budget is turned into 504 by the
    // API itself, not by the searcher.
    let addr = boot(Arc::new(SleepySearcher)).await;
    let (s, b) = call(
        addr,
        "POST",
        "/search",
        Some(r#"{"query":"rust","deadline_ms":1}"#),
    )
    .await;
    assert_eq!(s, 504, "body: {b}");
}

#[tokio::test]
async fn search_filters_are_applied_over_http() {
    let addr = boot(Arc::new(StubSearcher::new(20))).await;
    // The stub is tagged source_id "stub"; a matching filter keeps results...
    let (s, b) = call(
        addr,
        "POST",
        "/search",
        Some(r#"{"query":"rust","filters":{"source_id":"stub"}}"#),
    )
    .await;
    assert_eq!(s, 200, "body: {b}");
    assert!(!b["results"].as_array().unwrap().is_empty());

    // ...and a non-matching source filter drops every hit.
    let (s, b) = call(
        addr,
        "POST",
        "/search",
        Some(r#"{"query":"rust","filters":{"source_id":"nowhere"}}"#),
    )
    .await;
    assert_eq!(s, 200, "body: {b}");
    assert!(b["results"].as_array().unwrap().is_empty());
}
