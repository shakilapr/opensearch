//! Ingest journal + state machine types (spec §3). OWNER: ingest agent.

use serde::{Deserialize, Serialize};

/// The strictly ordered ingestion lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IngestStage {
    Fetched,
    Normalized,
    BlobWritten,
    MetadataCommitted,
    LexicalIndexed,
    GraphIndexed,
    Visible,
}

impl IngestStage {
    pub fn as_str(self) -> &'static str {
        match self {
            IngestStage::Fetched => "FETCHED",
            IngestStage::Normalized => "NORMALIZED",
            IngestStage::BlobWritten => "BLOB_WRITTEN",
            IngestStage::MetadataCommitted => "METADATA_COMMITTED",
            IngestStage::LexicalIndexed => "LEXICAL_INDEXED",
            IngestStage::GraphIndexed => "GRAPH_INDEXED",
            IngestStage::Visible => "VISIBLE",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "FETCHED" => IngestStage::Fetched,
            "NORMALIZED" => IngestStage::Normalized,
            "BLOB_WRITTEN" => IngestStage::BlobWritten,
            "METADATA_COMMITTED" => IngestStage::MetadataCommitted,
            "LEXICAL_INDEXED" => IngestStage::LexicalIndexed,
            "GRAPH_INDEXED" => IngestStage::GraphIndexed,
            "VISIBLE" => IngestStage::Visible,
            _ => return None,
        })
    }

    /// Every stage, in strict lifecycle order. `ALL` is the canonical walk used
    /// by the idempotency driver.
    pub const ALL: [IngestStage; 7] = [
        IngestStage::Fetched,
        IngestStage::Normalized,
        IngestStage::BlobWritten,
        IngestStage::MetadataCommitted,
        IngestStage::LexicalIndexed,
        IngestStage::GraphIndexed,
        IngestStage::Visible,
    ];

    /// The next stage in the strict order, or `None` at `VISIBLE`.
    pub fn next(self) -> Option<IngestStage> {
        let idx = self.ordinal() as usize;
        IngestStage::ALL.get(idx + 1).copied()
    }

    /// Zero-based position in the strict order. Backwards/duplicate detection
    /// uses this ordering (`IngestStage` also derives `Ord`).
    pub fn ordinal(self) -> u8 {
        self as u8
    }
}

/// A single idempotency key's progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestState {
    pub op_key: String,
    pub resource_id: String,
    pub docver_id: Option<String>,
    pub stage: IngestStage,
}
