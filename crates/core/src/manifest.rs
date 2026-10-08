//! IndexManifest — the searchable generation pointer.
//!
//! A response is reproducible from
//! `index_manifest_id + overlay_watermark + graph_version + ranking_policy_version + query_plan_version`.
//! Search only exposes versions below the published watermark.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexManifest {
    pub manifest_id: String,
    pub generation: u64,
    pub doc_count: u64,
    pub base_segments: u64,
    /// Ingestion high-water mark covered by this generation.
    pub watermark: u64,
    pub embedding_model_ids: Vec<String>,
    pub metadata_version: u64,
    pub created_at: i64,
    pub published_at: Option<i64>,
}

/// Where a candidate result came from — persistent snapshot vs live overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// From the immutable published base index.
    Snapshot,
    /// From the recent-document overlay, not yet folded into base.
    Overlay,
}

impl Provenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Provenance::Snapshot => "snapshot",
            Provenance::Overlay => "overlay",
        }
    }
}
