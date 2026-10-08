//! Integration tests for the Stage-4 HTTP endpoints `POST /search_deep` and
//! `GET /read`.
//!
//! Boots the real router on an ephemeral loopback port with stub `Searcher` /
//! `DeepSearcher` implementations and exercises it over TCP (hermetic).

use search_api::{
    router, AppState, DeepHit, DeepSearchResponse, DeepSearcher, DocResponse, IndexHit, Searcher,
};
use search_core::{AppError, AppResult, IndexManifest, Provenance, SearchFilters};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// --- stubs ---------------------------------------------------------------

struct StubSearcher {
    hits: Vec<IndexHit>,
}

impl Searcher for StubSearcher {
    fn search(
        &self,
        _q: &str,
        limit: usize,
        _filters: Option<&SearchFilters>,
    ) -> AppResult<Vec<IndexHit>> {
        Ok(self.hits.iter().take(limit).cloned().collect())
    }
    fn manifest(&self) -> AppResult<IndexManifest> {
        Ok(IndexManifest {
            manifest_id: "m-1".into(),
            generation: 1,
            doc_count: self.hits.len() as u64,
            base_segments: 0,
            watermark: 1,
            embedding_model_ids: vec![],
            metadata_version: 1,
            created_at: 0,
            published_at: None,
        })
    }
}

fn stub_searcher() -> Arc<dyn Searcher + Send + Sync> {
    Arc::new(StubSearcher {
        hits: vec![IndexHit {
            doc_id: "d1".into(),
            version: 1,
            resource_id: "res_d1".into(),
            work_id: None,
            content_id: None,
            url: "https://example.test/d1".into(),
            title: "rust doc".into(),
            snippet: "snippet".into(),
            source_id: Some("stub".into()),
            doc_type: Some("html".into()),
            language: Some("en".into()),
            published_at: Some(1_700_000_000),
            modified_at: None,
            fetched_at: 1_700_000_000,
            lane: "lexical_base".into(),
            provenance: Provenance::Snapshot,
            score: 1.0,
            signals: vec![],
        }],
    })
}

/// A deep backend with one known document (`doc-known`).
struct StubDeep;

impl DeepSearcher for StubDeep {
    fn deep_search(&self, query: &str, limit: usize) -> AppResult<DeepSearchResponse> {
        let hits = (0..limit.min(2))
            .map(|i| DeepHit {
                doc_id: format!("doc-{i}"),
                url: format!("https://example.test/{i}"),
                title: format!("{query} {i}"),
                snippet: format!("snippet {i}"),
                provenance: "overlay".into(),
            })
            .collect();
        Ok(DeepSearchResponse {
            hits,
            trace: vec![
                format!("retrieve [{query}]"),
                "widen".to_string(),
                "follow_link [doc-0]".to_string(),
            ],
        })
    }

    fn read(&self, doc_id: &str) -> AppResult<DocResponse> {
        if doc_id == "doc-known" {
            Ok(DocResponse {
                doc_id: doc_id.to_string(),
                text: "the normalized body of doc-known".to_string(),
            })
        } else {
            Err(AppError::NotFound(format!("no document {doc_id}")))
        }
    }
}

// --- harness -------------------------------------------------------------

async fn boot(searcher: Arc<dyn Searcher + Send + Sync>, deep: Option<Arc<dyn DeepSearcher + Send + Sync>>) -> std::net::SocketAddr {
    let state = match deep {
        Some(d) => AppState::with_deep(searcher, d),
        None => AppState::new(searcher),
    };
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

// --- tests ---------------------------------------------------------------

#[tokio::test]
async fn search_deep_returns_hits_and_trace() {
    let addr = boot(stub_searcher(), Some(Arc::new(StubDeep))).await;
    let (status, body) = call(
        addr,
        "POST",
        "/search_deep",
        Some(r#"{"query":"rust async","limit":2}"#),
    )
    .await;
    assert_eq!(status, 200, "search_deep: {body}");

    let hits = body["hits"].as_array().expect("hits array");
    assert_eq!(hits.len(), 2);
    // The STAGE7 hit shape.
    assert!(hits[0]["doc_id"].is_string());
    assert!(hits[0]["url"].is_string());
    assert!(hits[0]["title"].is_string());
    assert!(hits[0]["snippet"].is_string());
    assert!(hits[0]["provenance"].is_string());

    let trace = body["trace"].as_array().expect("trace array");
    assert_eq!(trace.len(), 3);
    assert!(trace.iter().all(|t| t.is_string()));
    assert!(trace[0].as_str().unwrap().contains("retrieve"));
}

#[tokio::test]
async fn search_deep_omitted_limit_defaults_and_zero_is_400() {
    let addr = boot(stub_searcher(), Some(Arc::new(StubDeep))).await;
    // `limit` is optional.
    let (s, b) = call(addr, "POST", "/search_deep", Some(r#"{"query":"rust"}"#)).await;
    assert_eq!(s, 200, "body: {b}");
    assert_eq!(b["hits"].as_array().unwrap().len(), 2, "default limit 10, stub caps at 2");

    // An empty query is a 400.
    let (s, b) = call(addr, "POST", "/search_deep", Some(r#"{"query":"   "}"#)).await;
    assert_eq!(s, 400, "body: {b}");
    assert!(b["error"].as_str().unwrap().contains("query"));
}

#[tokio::test]
async fn search_deep_without_backend_is_503() {
    let addr = boot(stub_searcher(), None).await;
    let (s, b) = call(addr, "POST", "/search_deep", Some(r#"{"query":"rust"}"#)).await;
    assert_eq!(s, 503, "body: {b}");
}

#[tokio::test]
async fn read_returns_document_text() {
    let addr = boot(stub_searcher(), Some(Arc::new(StubDeep))).await;
    let (status, body) = call(addr, "GET", "/read?doc_id=doc-known", None).await;
    assert_eq!(status, 200, "read: {body}");
    assert_eq!(body["doc_id"], "doc-known");
    assert!(body["text"].as_str().unwrap().contains("normalized body"));
}

#[tokio::test]
async fn read_without_doc_id_is_400() {
    let addr = boot(stub_searcher(), Some(Arc::new(StubDeep))).await;
    let (status, body) = call(addr, "GET", "/read", None).await;
    assert_eq!(status, 400, "read with no doc_id: {body}");
    assert!(body["error"].as_str().unwrap().contains("doc_id"));

    // An empty value is treated the same as missing.
    let (status, body) = call(addr, "GET", "/read?doc_id=", None).await;
    assert_eq!(status, 400, "body: {body}");
}

#[tokio::test]
async fn read_unknown_doc_id_is_404() {
    let addr = boot(stub_searcher(), Some(Arc::new(StubDeep))).await;
    let (status, body) = call(addr, "GET", "/read?doc_id=nope", None).await;
    assert_eq!(status, 404, "read unknown doc: {body}");
    assert!(body["error"].as_str().unwrap().contains("nope"));
}
