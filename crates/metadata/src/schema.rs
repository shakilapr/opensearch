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
    // Stage-1 freshness columns (migration 0003). Kept distinct from
    // `first_seen` / `last_fetch`.
    pub published_at: Option<i64>,
    pub modified_at: Option<i64>,
    pub fetched_at: Option<i64>,
    pub verified_at: Option<i64>,
    pub feed_watermark: Option<i64>,
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

// ---------------------------------------------------------------------------
// Stage-1 rows (migration 0003). Plain structs only — no dependency on the
// discovery / sources / graph domain crates.
// ---------------------------------------------------------------------------

/// One sitemap document (`sitemap`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SitemapRow {
    pub sitemap_id: String,
    pub url: String,
    pub source_url: Option<String>,
    pub domain_id: Option<String>,
    pub is_index: i64,
    pub status: String,
    pub url_count: i64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub fetched_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One feed document (`feed`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedRow {
    pub feed_id: String,
    pub url: String,
    pub domain_id: Option<String>,
    pub title: Option<String>,
    pub feed_type: String,
    pub status: String,
    pub entry_count: i64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub watermark: Option<String>,
    pub fetched_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One source registry entry (`source_profile`). `topics`, `entities` and
/// `url_patterns` are JSON-encoded TEXT arrays.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceProfileRow {
    pub source_id: String,
    pub domain: String,
    pub topics: String,
    pub entities: String,
    pub url_patterns: String,
    pub authority: f64,
    pub change_rate: f64,
    pub js_probability: f64,
    pub fetch_count: i64,
    pub error_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One canonical entity node (`entity`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityRow {
    pub entity_id: String,
    pub name: String,
    pub kind: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One alternate surface form of an entity (`entity_alias`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityAliasRow {
    pub entity_id: String,
    pub alias: String,
    pub created_at: i64,
}

/// One typed, weighted edge between entities (`graph_edge`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphEdgeRow {
    pub from_id: String,
    pub to_id: String,
    pub kind: String,
    pub weight: f64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One scholarly work (`work`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkRow {
    pub work_id: String,
    pub id_kind: String,
    pub identifier: String,
    pub title: Option<String>,
    pub published_at: Option<i64>,
    pub modified_at: Option<i64>,
    pub resource_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}
