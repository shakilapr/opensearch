//! Backend abstraction the MCP tools dispatch to.
//!
//! The server is generic over [`Backend`] so the whole protocol — including
//! `tools/call` for all three tools — is testable against a fake in-process
//! backend, with no live engine and no network.

use serde::Serialize;
use serde_json::{json, Value};

use crate::error::McpError;

/// One search result exposed to MCP clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hit {
    pub doc_id: String,
    pub url: String,
    pub title: String,
    pub snippet: String,
    pub provenance: String,
}

impl Hit {
    /// The wire shape of a hit: `url/title/snippet/provenance` (+ `doc_id`).
    pub fn to_json(&self) -> Value {
        json!({
            "doc_id": self.doc_id,
            "url": self.url,
            "title": self.title,
            "snippet": self.snippet,
            "provenance": self.provenance,
        })
    }
}

/// Normalized document text for one `doc_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DocText {
    pub doc_id: String,
    pub text: String,
}

/// The result of a `deep_search`: the hits plus the navigation trace the engine
/// walked to reach them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeepResult {
    pub hits: Vec<Hit>,
    pub trace: Vec<String>,
}

/// The one thing the MCP tools need from retrieval.
///
/// Kept intentionally small so it is trivial to stub in tests and to implement
/// or adapt over the real pipeline / HTTP API.
pub trait Backend: Send + Sync {
    /// Return up to `limit` ranked hits for `query`.
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>, McpError>;

    /// Run a deeper, navigation-trace-carrying search for `query`.
    fn deep_search(&self, query: &str, limit: usize) -> Result<DeepResult, McpError>;

    /// Return the normalized text of a previously-seen document.
    fn fetch_doc(&self, doc_id: &str) -> Result<DocText, McpError>;
}
