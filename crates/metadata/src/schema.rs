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
