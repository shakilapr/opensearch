//! Acceptance test 12: the MCP `search` tool round trip.
//!
//! Hermetic end-to-end surface test: the real `search-api` router is booted on
//! `127.0.0.1:0` over the real pipeline's searcher, `HttpBackend` points the MCP
//! server at it, and `handle_line` is driven exactly as the stdio loop drives
//! it. No public internet, no fixed port, no external service.

mod common;

use common::*;
use search_agent0::Pipeline;
use search_api::{router, AppState, Searcher};
use search_mcp::{handle_line, HttpBackend};
use serde_json::json;
use std::sync::Arc;

/// Boot the real search-api router on an ephemeral loopback port in a
/// dedicated runtime thread; return its base URL.
fn spawn_api_server(searcher: Arc<dyn Searcher + Send + Sync>) -> String {
    let (tx, rx) = std::sync::mpsc::channel::<std::net::SocketAddr>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("server runtime");
        rt.block_on(async move {
            let app = router(AppState::new(searcher));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind api port");
            let addr = listener.local_addr().expect("api local addr");
            tx.send(addr).expect("send api addr");
            axum::serve(listener, app).await.expect("serve api");
        });
    });
    let addr = rx.recv().expect("api server address");
    format!("http://{addr}")
}

#[test]
fn mcp_search_tool_round_trip() {
    // A runtime for the fixture server + the async ingest; kept alive for the
    // whole test so the spawned fixture server keeps serving.
    let rt = tokio::runtime::Runtime::new().unwrap();

    let (base, _fx) = rt.block_on(spawn_fixture());
    let url = format!("{base}/page");

    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());
    let pipeline = Pipeline::open_with(&cfg, true).unwrap();
    rt.block_on(pipeline.crawl_one(&url)).unwrap();
    let rid = pipeline.resource_id_for(&url).unwrap();

    // The MCP server talks to the real HTTP search-api over loopback.
    let api_base = spawn_api_server(pipeline.searcher());
    let backend = HttpBackend::new(api_base).expect("build MCP HTTP backend");

    // `initialize` — the protocol handshake round-trips.
    let init = json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": { "protocolVersion": search_mcp::PROTOCOL_VERSION }
    })
    .to_string();
    let init_resp: serde_json::Value =
        serde_json::from_str(&handle_line(&backend, &init).expect("initialize response")).unwrap();
    assert_eq!(init_resp["result"]["protocolVersion"], search_mcp::PROTOCOL_VERSION);

    // `tools/call` for `search` — the tool round-trips real indexed hits.
    let req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "search", "arguments": { "query": "zephyrquokka", "limit": 5 } }
    })
    .to_string();
    let line = handle_line(&backend, &req).expect("tools/call must yield a response");
    let resp: serde_json::Value = serde_json::from_str(&line).expect("valid JSON response");

    assert!(resp.get("error").is_none(), "no JSON-RPC error: {resp}");
    assert_eq!(resp["result"]["isError"], false, "isError false: {resp}");
    assert_eq!(resp["result"]["content"][0]["type"], "text");

    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let hits: serde_json::Value = serde_json::from_str(text).expect("hits JSON");
    let arr = hits.as_array().expect("hits array");
    assert_eq!(arr.len(), 1, "exactly one indexed hit: {hits}");
    assert_eq!(arr[0]["doc_id"], rid, "stable doc id round-trips");
    assert_eq!(arr[0]["url"], url, "url round-trips");
    assert!(
        arr[0]["title"].as_str().unwrap().contains("Stage Eight Page"),
        "title round-trips: {hits}"
    );
    assert!(arr[0]["provenance"].is_string(), "provenance exposed");

    println!(
        "PASS mcp_search_tool_round_trip: initialize + tools/call search -> {} hit(s) for {url}",
        arr.len()
    );
}
