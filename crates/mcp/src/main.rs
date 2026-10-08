//! `search-mcp` binary: exposes the local search-api over the Model Context
//! Protocol (JSON-RPC 2.0 over stdio).
//!
//! The protocol layer lives in the library; this binary supplies the concrete
//! [`Backend`] — a thin loopback client of the HTTP search-api — and runs the
//! stdio loop. The API base is `SEARCH_API_BASE`, falling back to
//! `http://<SEARCH_BIND>` and finally `http://127.0.0.1:8080`.
//!
//! ```text
//! search-mcp          # reads newline-delimited JSON-RPC on stdin
//! ```

#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::time::Duration;

use search_mcp::{Backend, DeepResult, DocText, Hit, McpError};
use serde_json::{json, Value};

/// The default search-api base when no environment override is set.
const DEFAULT_API_BASE: &str = "http://127.0.0.1:8080";

/// A [`Backend`] that talks to the local HTTP search-api over loopback.
struct HttpBackend {
    base: String,
    client: reqwest::blocking::Client,
}

impl HttpBackend {
    fn new(base: String) -> Result<Self, McpError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| McpError::backend(format!("failed to build http client: {e}")))?;
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            client,
        })
    }

    fn post_json(&self, path: &str, body: Value) -> Result<Value, McpError> {
        let url = format!("{}{}", self.base, path);
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .map_err(|e| McpError::backend(format!("search-api unreachable at {url}: {e}")))?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if !status.is_success() {
            return Err(McpError::backend(format!(
                "search-api {path} returned HTTP {status}: {}",
                text.trim()
            )));
        }
        serde_json::from_str(&text)
            .map_err(|e| McpError::backend(format!("invalid JSON from search-api {path}: {e}")))
    }
}

/// Map one search-api `SearchResult` JSON object onto an MCP [`Hit`].
fn hit_from_json(v: &Value) -> Hit {
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let provenance = v
        .get("provenance")
        .map(|p| match p {
            Value::String(x) => x.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    Hit {
        doc_id: s("doc_id"),
        url: v
            .get("canonical_url")
            .and_then(Value::as_str)
            .or_else(|| v.get("url").and_then(Value::as_str))
            .unwrap_or("")
            .to_string(),
        title: s("title"),
        snippet: s("snippet"),
        provenance,
    }
}

impl Backend for HttpBackend {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>, McpError> {
        let body = self.post_json("/search", json!({ "query": query, "limit": limit }))?;
        let results = body
            .get("results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(results.iter().map(hit_from_json).collect())
    }

    fn deep_search(&self, query: &str, limit: usize) -> Result<DeepResult, McpError> {
        // The HTTP API's `/search_deep` endpoint is part of the Stage-4/7 HTTP
        // extension (docs/STAGE7_SPEC.md). Until it lands, report an honest
        // tool error rather than guessing a response shape.
        let _ = (query, limit);
        Err(McpError::backend(
            "deep_search is not yet exposed by the HTTP API (/search_deep pending the Stage-7 HTTP extension)",
        ))
    }

    fn fetch_doc(&self, doc_id: &str) -> Result<DocText, McpError> {
        let _ = doc_id;
        Err(McpError::backend(
            "fetch_doc is not yet exposed by the HTTP API (/read pending the Stage-7 HTTP extension)",
        ))
    }
}

fn api_base() -> String {
    if let Ok(base) = std::env::var("SEARCH_API_BASE") {
        if !base.trim().is_empty() {
            return base;
        }
    }
    if let Ok(bind) = std::env::var("SEARCH_BIND") {
        if !bind.trim().is_empty() {
            return format!("http://{bind}");
        }
    }
    DEFAULT_API_BASE.to_string()
}

fn main() -> ExitCode {
    let backend = match HttpBackend::new(api_base()) {
        Ok(b) => Box::new(b) as Box<dyn Backend>,
        Err(e) => {
            eprintln!("search-mcp: {e}");
            return ExitCode::from(1);
        }
    };
    if let Err(e) = search_mcp::serve(backend) {
        eprintln!("search-mcp: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
