//! Typed row structs mirroring the SQLite tables (spec §4). OWNER: metadata agent.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceRow {
    pub resource_id: String,
    pub url: String,
    pub canonical_url: String,
    pub domain_id: String,
    pub source_id: Option<String>,
    pub kind: String,
    pub status: String,
    pub first_seen: i64,
    pub last_fetch: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontierRow {
    pub resource_id: String,
    pub url: String,
    pub domain_id: String,
    pub priority: i64,
    pub earliest_fetch_at: i64,
    pub retry_count: i64,
    pub status: String,
    pub lease_until: Option<i64>,
    pub attempt: i64,
    pub owner: Option<String>,
    pub epoch: i64,
}

/// One immutable document version (`document_version`, spec §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocVersionRow {
    pub docver_id: String,
    pub resource_id: String,
    pub version_no: i64,
    pub content_id: Option<String>,
    pub title: Option<String>,
    pub body_hash: Option<String>,
    pub parser_version: String,
    pub published_at: Option<i64>,
    pub modified_at: Option<i64>,
    pub fetched_at: i64,
    pub created_at: i64,
}

/// A published/unpublished index manifest generation (`manifest`, spec §4/§6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestRow {
    pub manifest_id: String,
    pub generation: i64,
    pub published: i64,
    pub doc_count: i64,
    pub base_segments: i64,
    pub created_at: i64,
    pub published_at: Option<i64>,
}

/// One append-only journal row (`ingest_event`, spec §3/§4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestEventRow {
    pub seq: i64,
    pub op_key: String,
    pub resource_id: String,
    pub stage: String,
    pub detail: Option<String>,
    pub at: i64,
}
