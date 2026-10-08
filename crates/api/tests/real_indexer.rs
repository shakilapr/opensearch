//! Integration test for the real-indexer wiring: build a tempdir indexer, index
//! documents, and run `/search` through the actual router over TCP. Compiled
//! only with `--features real-indexer`.
//!
//! Hermetic: ephemeral loopback port, tempdir index, no external network.
#![cfg(feature = "real-indexer")]

use search_api::{router, AppState, IndexerSearcher};
use search_indexer::{IndexDoc, Indexer, IndexerConfig};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Build a tempdir-backed indexer and index two documents that both contain the
/// term "rust". Returns the `TempDir` guard too so the files outlive the test.
fn seed_indexer() -> (tempfile::TempDir, Arc<Indexer>) {
    let dir = tempfile::TempDir::new().unwrap();
    let indexer = Indexer::open(IndexerConfig::new(dir.path().join("tantivy"))).unwrap();
    indexer
        .index_batch(vec![
            IndexDoc::new(
                "d1",
                "res_d1",
                "https://example.test/rust-async",
                "Rust async runtimes",
                "tokio is a rust asynchronous runtime engine",
            )
            .with_source("news"),
            IndexDoc::new(
                "d2",
                "res_d2",
                "https://example.test/rust-ownership",
                "Rust ownership",
                "the borrow checker keeps rust memory safe",
            )
            .with_source("blog"),
        ])
        .unwrap();
    // Publish a manifest so the generation/watermark are real.
    indexer.publish_manifest().unwrap();
    (dir, Arc::new(indexer))
}

async fn boot(searcher: Arc<IndexerSearcher>) -> std::net::SocketAddr {
    let app = router(AppState::new(searcher));
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
    let status: u16 = text.split_whitespace().nth(1).unwrap().parse().unwrap();
    let raw_body = text.split("\r\n\r\n").nth(1).unwrap_or("");
    let json = serde_json::from_str(raw_body).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn real_indexer_search_returns_indexed_docs_through_the_router() {
    let (_dir, indexer) = seed_indexer();
    let addr = boot(Arc::new(IndexerSearcher::new(indexer))).await;

    let (status, body) = call(addr, "POST", "/search", Some(r#"{"query":"rust"}"#)).await;
    assert_eq!(status, 200, "search: {body}");

    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2, "both indexed docs must come back: {body}");

    let mut ids: Vec<&str> = results.iter().map(|r| r["doc_id"].as_str().unwrap()).collect();
    ids.sort();
    assert_eq!(ids, vec!["d1", "d2"]);

    // The adapter maps stored fields faithfully (canonical_url -> url, lane, ...).
    let d1 = results.iter().find(|r| r["doc_id"] == "d1").unwrap();
    assert_eq!(d1["canonical_url"], "https://example.test/rust-async");
    assert_eq!(d1["title"], "Rust async runtimes");
    assert_eq!(d1["source_id"], "news");
    assert_eq!(d1["lane"], "lexical_base");
    assert_eq!(d1["provenance"], "snapshot");
    let signals = d1["signals"].as_array().expect("signals array");
    assert!(!signals.is_empty());
    let names: Vec<&str> = signals.iter().map(|s| s["name"].as_str().unwrap()).collect();
    for want in [
        "bm25",
        "source_authority",
        "freshness",
        "entity_match",
        "graph_expansion",
    ] {
        assert!(names.contains(&want), "missing signal {want}: {names:?}");
    }
    assert!(d1["version"].as_u64().unwrap() >= 1);

    // Reproducibility tuple is populated from the real published manifest.
    assert_eq!(body["index_manifest_id"], "gen-1");
    assert_eq!(body["overlay_watermark"], 1);
    assert!(body["complete"].as_bool().unwrap());
    assert!(body["degradations"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn real_indexer_honours_source_and_doc_type_filters() {
    let (_dir, indexer) = seed_indexer();
    let addr = boot(Arc::new(IndexerSearcher::new(indexer))).await;

    // source_id filter is pushed down and re-checked by the API.
    let (s, b) = call(
        addr,
        "POST",
        "/search",
        Some(r#"{"query":"rust","filters":{"source_id":"news"}}"#),
    )
    .await;
    assert_eq!(s, 200, "body: {b}");
    let results = b["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "only the news doc: {b}");
    assert_eq!(results[0]["doc_id"], "d1");

    // doc_type is indexed but not stored; the adapter pushes it into the query.
    let (s, b) = call(
        addr,
        "POST",
        "/search",
        Some(r#"{"query":"rust","filters":{"doc_type":"pdf"}}"#),
    )
    .await;
    assert_eq!(s, 200, "body: {b}");
    assert!(b["results"].as_array().unwrap().is_empty(), "no pdf docs: {b}");

    let (s, b) = call(
        addr,
        "POST",
        "/search",
        Some(r#"{"query":"rust","filters":{"doc_type":"html"}}"#),
    )
    .await;
    assert_eq!(s, 200, "body: {b}");
    assert_eq!(b["results"].as_array().unwrap().len(), 2, "both are html: {b}");
}

#[tokio::test]
async fn real_indexer_overlay_result_is_marked_live() {
    let (_dir, indexer) = seed_indexer();
    // Write a fresh overlay doc (not yet folded into the immutable base).
    indexer
        .index_overlay(
            IndexDoc::new(
                "d3",
                "res_d3",
                "https://example.test/rust-live",
                "Rust live overlay",
                "a rust document served from the recent overlay window",
            )
            .with_source("live"),
        )
        .unwrap();
    let addr = boot(Arc::new(IndexerSearcher::new(indexer))).await;

    let (s, b) = call(addr, "POST", "/search", Some(r#"{"query":"rust"}"#)).await;
    assert_eq!(s, 200, "body: {b}");
    let results = b["results"].as_array().unwrap();
    let d3 = results
        .iter()
        .find(|r| r["doc_id"] == "d3")
        .expect("overlay doc returned");
    // `live_overlay` on the UI is derived from these frozen fields.
    assert_eq!(d3["lane"], "lexical_overlay");
    assert_eq!(d3["provenance"], "overlay");
}

#[tokio::test]
async fn real_indexer_health_reports_the_published_generation() {
    let (_dir, indexer) = seed_indexer();
    let addr = boot(Arc::new(IndexerSearcher::new(indexer))).await;

    let (status, body) = call(addr, "GET", "/health", None).await;
    assert_eq!(status, 200, "health: {body}");
    assert_eq!(body["generation"], 1);
    assert_eq!(body["index_manifest_id"], "gen-1");
    assert_eq!(body["status"], "ok");
}
