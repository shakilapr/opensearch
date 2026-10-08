//! Round-trip tests for the library [`HttpBackend`] through `handle_line`.
//!
//! A tiny hand-rolled loopback HTTP server (std only, no new deps) stands in for
//! the search-api, so every tool — including the newly-wired `deep_search` and
//! `fetch_doc` — round-trips exactly as the stdio binary drives it.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

use search_mcp::{handle_line, HttpBackend};
use serde_json::{json, Value};

// --- loopback HTTP server --------------------------------------------------

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn content_length(head: &str) -> usize {
    head.lines()
        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

/// Spawn a one-connection-at-a-time loopback server; returns its base URL.
fn spawn_server(
    handler: impl Fn(&str, &str, &str) -> (u16, String) + Send + Sync + 'static,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handler = Arc::new(handler);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let h = handler.clone();
            std::thread::spawn(move || {
                let mut buf: Vec<u8> = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    let n = s.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = find(&buf, b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                        if buf.len() >= pos + 4 + content_length(&head) {
                            break;
                        }
                    }
                }
                let head_end = find(&buf, b"\r\n\r\n").unwrap_or(buf.len());
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let req = head.lines().next().unwrap_or("");
                let mut parts = req.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let target = parts.next().unwrap_or("").to_string();
                let body = if buf.len() > head_end + 4 {
                    String::from_utf8_lossy(&buf[head_end + 4..]).to_string()
                } else {
                    String::new()
                };
                let (status, payload) = h(&method, &target, &body);
                let reason = match status {
                    200 => "OK",
                    400 => "Bad Request",
                    404 => "Not Found",
                    _ => "Status",
                };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = s.write_all(resp.as_bytes());
                let _ = s.flush();
            });
        }
    });
    format!("http://{addr}")
}

fn fixture_server() -> String {
    spawn_server(|method, target, _body| match (method, target) {
        ("POST", "/search") => (
            200,
            json!({ "results": [
                { "doc_id": "d1", "canonical_url": "https://example.test/d1",
                  "title": "t1", "snippet": "s1", "provenance": "snapshot" }
            ]})
            .to_string(),
        ),
        ("POST", "/search_deep") => (
            200,
            json!({
                "hits": [
                    { "doc_id": "dx", "url": "https://example.test/dx",
                      "title": "td", "snippet": "sd", "provenance": "overlay" }
                ],
                "trace": ["retrieve [rust]", "widen", "follow_link [dx]"]
            })
            .to_string(),
        ),
        ("GET", t) if t.starts_with("/read") => {
            if t.contains("doc_id=missing") {
                (404, json!({ "error": "no document missing" }).to_string())
            } else {
                (
                    200,
                    json!({ "doc_id": "doc-known", "text": "the normalized text of doc-known" })
                        .to_string(),
                )
            }
        }
        _ => (404, json!({ "error": "no route" }).to_string()),
    })
}

fn call(backend: &HttpBackend, req: Value) -> Value {
    let resp = handle_line(backend, &req.to_string()).expect("request must yield a response");
    serde_json::from_str(&resp).expect("response must be valid JSON")
}

// --- tests ---------------------------------------------------------------

#[test]
fn deep_search_tool_round_trips_through_loopback_http() {
    let backend = HttpBackend::new(fixture_server()).unwrap();
    let resp = call(
        &backend,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "deep_search", "arguments": { "query": "rust", "limit": 3 } }
        }),
    );
    assert_eq!(resp["result"]["isError"], false, "resp: {resp}");
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let v: Value = serde_json::from_str(text).unwrap();
    let hits = v["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["doc_id"], "dx");
    assert_eq!(hits[0]["url"], "https://example.test/dx");
    assert!(hits[0]["provenance"].is_string());
    let trace = v["trace"].as_array().unwrap();
    assert_eq!(trace.len(), 3, "the navigation trace round-trips");
    assert!(trace[0].as_str().unwrap().contains("retrieve"));
}

#[test]
fn fetch_doc_tool_round_trips_through_loopback_http() {
    let backend = HttpBackend::new(fixture_server()).unwrap();
    let resp = call(
        &backend,
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "fetch_doc", "arguments": { "doc_id": "doc-known" } }
        }),
    );
    assert_eq!(resp["result"]["isError"], false, "resp: {resp}");
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let v: Value = serde_json::from_str(text).unwrap();
    assert_eq!(v["doc_id"], "doc-known");
    assert!(v["text"].as_str().unwrap().contains("normalized text"));
}

#[test]
fn search_tool_still_round_trips_through_loopback_http() {
    let backend = HttpBackend::new(fixture_server()).unwrap();
    let resp = call(
        &backend,
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "search", "arguments": { "query": "rust" } }
        }),
    );
    assert_eq!(resp["result"]["isError"], false, "resp: {resp}");
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let arr: Value = serde_json::from_str(text).unwrap();
    assert_eq!(arr.as_array().unwrap().len(), 1);
    assert_eq!(arr[0]["url"], "https://example.test/d1");
}

#[test]
fn loopback_backend_failure_is_an_is_error_block() {
    let backend = HttpBackend::new(fixture_server()).unwrap();
    let resp = call(
        &backend,
        json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": { "name": "fetch_doc", "arguments": { "doc_id": "missing" } }
        }),
    );
    assert!(resp.get("error").is_none(), "tool errors are not JSON-RPC errors");
    assert_eq!(resp["result"]["isError"], true);
    assert!(resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("404"));
}
