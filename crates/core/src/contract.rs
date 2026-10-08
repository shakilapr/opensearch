//! The `/search` wire contract (spec §7).
//!
//! This is shared by the API crate and the UI-facing JSON. It is defined here,
//! in `core`, so every layer can depend on it without a circular dependency.

use serde::{Deserialize, Serialize};

/// One request to `/search`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_tz")]
    pub timezone: String,
    #[serde(default)]
    pub deadline_ms: Option<u64>,
    #[serde(default)]
    pub filters: Option<SearchFilters>,
}

fn default_limit() -> usize {
    10
}
fn default_tz() -> String {
    "UTC".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchFilters {
    pub doc_type: Option<String>,
    pub source_id: Option<String>,
    pub language: Option<String>,
    /// Inclusive Unix-second bounds on `published_at`.
    pub after: Option<i64>,
    pub before: Option<i64>,
}

/// Structured understanding of the query. Stage 0 fills this heuristically;
/// later stages replace parts with classifiers/embeddings without changing the
/// API shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryAnalysis {
    pub original: String,
    pub normalized_text: String,
    pub language: Option<String>,
    pub exact_phrases: Vec<String>,
    pub entities: Vec<String>,
    /// Multiple hypotheses retained until context resolves. Stage 0: usually empty.
    pub entity_hypotheses: Vec<String>,
    pub concepts: Vec<String>,
    pub hard_predicates: Vec<String>,
    pub soft_preferences: Vec<String>,
    pub time_interval: Option<TimeInterval>,
    pub preferred_types: Vec<String>,
    pub intent: Intent,
    /// Heuristic 0..=4.
    pub complexity: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    Factual,
    Navigational,
    Current,
    Research,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeInterval {
    pub start: i64,
    pub end: i64,
}

/// The full response. `snapshot_id` hashes the reproducibility tuple.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub query_analysis: QueryAnalysis,
    pub results: Vec<SearchResult>,
    pub timing_ms: u64,
    pub index_manifest_id: String,
    pub overlay_watermark: u64,
    pub graph_version: String,
    pub ranking_policy_version: String,
    pub query_plan_version: String,
    pub snapshot_id: String,
    /// The configured procedure completed — NOT a claim the whole web was searched.
    pub complete: bool,
    #[serde(default)]
    pub degradations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub doc_id: String,
    pub version: u64,
    pub resource_id: String,
    pub work_id: Option<String>,
    pub content_id: Option<String>,
    pub canonical_url: String,
    pub title: String,
    pub snippet: String,
    pub source_id: Option<String>,
    pub published_at: Option<i64>,
    pub modified_at: Option<i64>,
    pub fetched_at: i64,
    pub lane: String,
    pub provenance: crate::manifest::Provenance,
    pub score: f32,
    /// Per-signal provenance so the UI's "Why this result?" panel works from Stage 0.
    pub signals: Vec<Signal>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    pub name: String,
    pub value: f32,
    pub note: String,
}
