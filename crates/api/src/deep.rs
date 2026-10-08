//! Deep-search + document-read request/response shapes and the [`DeepSearcher`]
//! boundary the HTTP handlers dispatch to.
//!
//! Kept in the API crate (mirroring `docs/STAGE7_SPEC.md`'s Hit/DocText and the
//! Stage-4 `/search_deep` + `/read` endpoints) so the router never depends on
//! the pipeline or the navigation crate: agent0 implements [`DeepSearcher`] over
//! its `Pipeline` and injects it into [`crate::AppState`].

use search_core::AppResult;
use serde::{Deserialize, Serialize};

/// Request body for `POST /search_deep`: `{ query, limit? }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeepSearchRequest {
    pub query: String,
    /// Optional result cap; defaults to [`crate::query::DEFAULT_LIMIT`].
    #[serde(default)]
    pub limit: Option<usize>,
}

/// One deep-search hit: the STAGE7 wire shape `{doc_id,url,title,snippet,provenance}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeepHit {
    pub doc_id: String,
    pub url: String,
    pub title: String,
    pub snippet: String,
    pub provenance: String,
}

/// Response of `POST /search_deep`: the hits plus the navigation trace strings
/// (one per executed nav `Step`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeepSearchResponse {
    pub hits: Vec<DeepHit>,
    pub trace: Vec<String>,
}

/// Response of `GET /read`: the normalized document text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocResponse {
    pub doc_id: String,
    pub text: String,
}

/// The navigation/read capability the HTTP layer needs.
///
/// Implemented by agent0's `PipelineDeepSearcher`; stubbed in API tests. A
/// missing document must fail with [`search_core::AppError::NotFound`] so the
/// handler maps it to `404`.
pub trait DeepSearcher: Send + Sync {
    /// Run a deep (navigation-driven) search for `query`.
    fn deep_search(&self, query: &str, limit: usize) -> AppResult<DeepSearchResponse>;

    /// Return the normalized text of `doc_id`.
    fn read(&self, doc_id: &str) -> AppResult<DocResponse>;
}
