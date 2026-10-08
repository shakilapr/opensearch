//! The HTTP loopback [`Backend`]: a thin blocking client of the local
//! `search-api`.
//!
//! Lives in the library (not the binary) so `tools/call` for every tool —
//! including `deep_search` and `fetch_doc` — can round-trip through
//! [`crate::handle_line`] against a loopback server in tests, exactly as the
//! stdio binary drives it.

use std::time::Duration;

use serde_json::{json, Value};

use crate::backend::{Backend, DeepResult, DocText, Hit};
use crate::error::McpError;

/// A [`Backend`] that talks to the local HTTP search-api over loopback.
pub struct HttpBackend {
    base: String,
    client: reqwest::blocking::Client,
}

impl HttpBackend {
    /// Build a backend pointed at `base` (e.g. `http://127.0.0.1:8080`).
    pub fn new(base: impl Into<String>) -> Result<Self, McpError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| McpError::backend(format!("failed to build http client: {e}")))?;
        Ok(Self {
            base: base.into().trim_end_matches('/').to_string(),
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
        decode(path, resp)
    }

    fn get_json(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, McpError> {
        let url = format!("{}{}", self.base, path);
        let resp = self
            .client
            .get(&url)
            .query(query)
            .send()
            .map_err(|e| McpError::backend(format!("search-api unreachable at {url}: {e}")))?;
        decode(path, resp)
    }
}

/// Decode a response, mapping a non-2xx status onto an honest tool error.
fn decode(path: &str, resp: reqwest::blocking::Response) -> Result<Value, McpError> {
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

/// Map one search-api hit JSON object onto an MCP [`Hit`]. Accepts either the
/// `SearchResult` shape (`canonical_url`) or the deep `DeepHit` shape (`url`).
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

fn hits_from_array(body: &Value, key: &str) -> Vec<Hit> {
    body.get(key)
        .and_then(Value::as_array)
        .map(|arr| arr.iter().map(hit_from_json).collect())
        .unwrap_or_default()
}

impl Backend for HttpBackend {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>, McpError> {
        let body = self.post_json("/search", json!({ "query": query, "limit": limit }))?;
        Ok(hits_from_array(&body, "results"))
    }

    fn deep_search(&self, query: &str, limit: usize) -> Result<DeepResult, McpError> {
        let body = self.post_json("/search_deep", json!({ "query": query, "limit": limit }))?;
        let hits = hits_from_array(&body, "hits");
        let trace = body
            .get("trace")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(|v| v.as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default();
        Ok(DeepResult { hits, trace })
    }

    fn fetch_doc(&self, doc_id: &str) -> Result<DocText, McpError> {
        let body = self.get_json("/read", &[("doc_id", doc_id)])?;
        Ok(DocText {
            doc_id: body
                .get("doc_id")
                .and_then(Value::as_str)
                .unwrap_or(doc_id)
                .to_string(),
            text: body
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    }
}
