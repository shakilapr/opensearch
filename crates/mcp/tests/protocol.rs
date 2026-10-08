//! Hermetic protocol tests: a fake [`Backend`], no network, no real engine.
//!
//! Every request round-trips through `handle_line`, exactly as the stdio loop
//! drives it.

use search_mcp::{handle_line, Backend, DeepResult, DocText, Hit, McpError};
use serde_json::{json, Value};

// --- fakes ---------------------------------------------------------------

struct FakeBackend;

fn fake_hit(doc_id: &str) -> Hit {
    Hit {
        doc_id: doc_id.to_string(),
        url: format!("https://example.test/{doc_id}"),
        title: format!("title {doc_id}"),
        snippet: format!("snippet {doc_id}"),
        provenance: "snapshot".to_string(),
    }
}

impl Backend for FakeBackend {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>, McpError> {
        let hits = (0..limit.min(2))
            .map(|i| fake_hit(&format!("d{i}-{query}")))
            .collect();
        Ok(hits)
    }

    fn deep_search(&self, query: &str, limit: usize) -> Result<DeepResult, McpError> {
        Ok(DeepResult {
            hits: (0..limit.min(1)).map(|i| fake_hit(&format!("deep{i}"))).collect(),
            trace: vec![format!("start:{query}"), "expand-links".to_string()],
        })
    }

    fn fetch_doc(&self, doc_id: &str) -> Result<DocText, McpError> {
        Ok(DocText {
            doc_id: doc_id.to_string(),
            text: format!("normalized text for {doc_id}"),
        })
    }
}

/// Always fails, so the `isError` path can be exercised.
struct FailingBackend;

impl Backend for FailingBackend {
    fn search(&self, _q: &str, _limit: usize) -> Result<Vec<Hit>, McpError> {
        Err(McpError::backend("engine unavailable: index offline"))
    }
    fn deep_search(&self, _q: &str, _limit: usize) -> Result<DeepResult, McpError> {
        Err(McpError::backend("engine unavailable"))
    }
    fn fetch_doc(&self, _d: &str) -> Result<DocText, McpError> {
        Err(McpError::backend("engine unavailable"))
    }
}

// --- helpers -------------------------------------------------------------

/// Send one request object through `handle_line` and parse the response.
fn call(backend: &dyn Backend, req: Value) -> Value {
    let resp = handle_line(backend, &req.to_string()).expect("request must yield a response");
    serde_json::from_str(&resp).expect("response must be valid JSON")
}

/// Send a raw line (for malformed-JSON cases).
fn call_raw(backend: &dyn Backend, line: &str) -> Value {
    let resp = handle_line(backend, line).expect("this line must yield a response");
    serde_json::from_str(&resp).expect("response must be valid JSON")
}

// --- tests ---------------------------------------------------------------

#[test]
fn initialize_returns_protocol_version_and_tools_capability() {
    let resp = call(
        &FakeBackend,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2024-11-05", "clientInfo": { "name": "test", "version": "0" } }
        }),
    );
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 1);
    assert!(
        resp["result"]["protocolVersion"].is_string(),
        "initialize must return a protocolVersion"
    );
    assert!(
        resp["result"]["capabilities"]["tools"].is_object(),
        "initialize must advertise the tools capability"
    );
    assert!(resp["result"]["serverInfo"]["name"].is_string());
}

#[test]
fn tools_list_returns_exactly_three_tools_with_input_schema() {
    let resp = call(
        &FakeBackend,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    );
    let tools = resp["result"]["tools"]
        .as_array()
        .expect("tools must be an array");
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["search", "deep_search", "fetch_doc"]);

    for t in tools {
        assert_eq!(t["inputSchema"]["type"], "object", "schema type for {}", t["name"]);
        assert!(
            t["inputSchema"]["properties"].is_object(),
            "properties for {}",
            t["name"]
        );
        assert!(
            t["inputSchema"]["required"].is_array(),
            "required for {}",
            t["name"]
        );
    }

    // The per-tool parameters are exactly the frozen ones.
    let search = &tools[0]["inputSchema"];
    assert!(search["properties"]["query"].is_object());
    assert!(search["properties"]["limit"].is_object());
    assert_eq!(search["required"], json!(["query"]));

    let fetch = &tools[2]["inputSchema"];
    assert!(fetch["properties"]["doc_id"].is_object());
    assert_eq!(fetch["required"], json!(["doc_id"]));
}

#[test]
fn tools_call_search_round_trips_for_search() {
    let resp = call(
        &FakeBackend,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "search", "arguments": { "query": "rust", "limit": 2 } }
        }),
    );
    let result = &resp["result"];
    assert_eq!(result["isError"], false);
    assert_eq!(result["content"][0]["type"], "text");
    let text = result["content"][0]["text"].as_str().unwrap();
    let hits: Value = serde_json::from_str(text).unwrap();
    let arr = hits.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert!(arr[0]["url"].is_string());
    assert!(arr[0]["title"].is_string());
    assert!(arr[0]["snippet"].is_string());
    assert!(arr[0]["provenance"].is_string());
    assert!(headers_have_no_error(&resp));
}

#[test]
fn tools_call_round_trips_for_deep_search() {
    let resp = call(
        &FakeBackend,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": { "name": "deep_search", "arguments": { "query": "async runtimes" } }
        }),
    );
    let result = &resp["result"];
    assert_eq!(result["isError"], false);
    let text = result["content"][0]["text"].as_str().unwrap();
    let v: Value = serde_json::from_str(text).unwrap();
    assert!(v["hits"].is_array());
    let trace = v["trace"].as_array().unwrap();
    assert!(!trace.is_empty(), "deep_search returns a navigation trace");
}

#[test]
fn tools_call_round_trips_for_fetch_doc() {
    let resp = call(
        &FakeBackend,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": { "name": "fetch_doc", "arguments": { "doc_id": "d0-rust" } }
        }),
    );
    let result = &resp["result"];
    assert_eq!(result["isError"], false);
    let text = result["content"][0]["text"].as_str().unwrap();
    let v: Value = serde_json::from_str(text).unwrap();
    assert_eq!(v["doc_id"], "d0-rust");
    assert!(v["text"].as_str().unwrap().contains("d0-rust"));
}

#[test]
fn malformed_json_returns_parse_error() {
    let resp = call_raw(&FakeBackend, "{ this is not json");
    assert_eq!(resp["error"]["code"], -32700);
    assert!(resp["id"].is_null(), "a parse error has a null id");

    // A valid-JSON scalar is not a request envelope.
    let resp2 = call_raw(&FakeBackend, "42");
    assert!(resp2["error"]["code"].is_i64());
    assert!(resp2["error"]["code"].as_i64().unwrap() < 0);
}

#[test]
fn unknown_method_returns_method_not_found() {
    let resp = call(
        &FakeBackend,
        json!({ "jsonrpc": "2.0", "id": 6, "method": "does/not/exist" }),
    );
    assert_eq!(resp["error"]["code"], -32601);
    assert_eq!(resp["id"], 6);
}

#[test]
fn bad_params_returns_invalid_params() {
    // tools/call with no `name`.
    let resp = call(
        &FakeBackend,
        json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {} }),
    );
    assert_eq!(resp["error"]["code"], -32602);

    // tools/call missing the required tool argument.
    let resp2 = call(
        &FakeBackend,
        json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/call",
            "params": { "name": "search", "arguments": {} }
        }),
    );
    assert_eq!(resp2["error"]["code"], -32602);

    // params of the wrong type entirely.
    let resp3 = call(
        &FakeBackend,
        json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/list", "params": [] }),
    );
    assert_eq!(resp3["error"]["code"], -32602);
}

#[test]
fn backend_error_returns_is_error_block_and_loop_survives() {
    // A backend failure is a *result* with isError: true — never a crash, never
    // a dropped connection.
    let resp = call(
        &FailingBackend,
        json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": { "name": "search", "arguments": { "query": "rust" } }
        }),
    );
    assert!(resp.get("error").is_none(), "tool errors are not JSON-RPC errors");
    assert_eq!(resp["result"]["isError"], true);
    assert_eq!(resp["result"]["content"][0]["type"], "text");
    assert!(
        !resp["result"]["content"][0]["text"].as_str().unwrap().is_empty(),
        "the error must carry a text message"
    );

    // The very next request on the same backend (and the same backend type)
    // still gets a proper response — the loop survived.
    let next = call(
        &FailingBackend,
        json!({ "jsonrpc": "2.0", "id": 11, "method": "tools/list" }),
    );
    assert_eq!(next["id"], 11);
    assert_eq!(
        next["result"]["tools"].as_array().unwrap().len(),
        3,
        "the loop is still serving after a tool error"
    );
}

#[test]
fn notification_without_id_returns_none() {
    // `notifications/initialized` is a notification: no id, no reply.
    let resp = handle_line(
        &FakeBackend,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    );
    assert!(resp.is_none(), "a notification must not be answered");

    // Any other method sent as a notification is likewise answered with None.
    let resp2 = handle_line(&FakeBackend, r#"{"jsonrpc":"2.0","method":"tools/list"}"#);
    assert!(resp2.is_none());
}

/// True when the top-level response object has no JSON-RPC `error` member.
fn headers_have_no_error(resp: &Value) -> bool {
    resp.get("error").is_none()
}
