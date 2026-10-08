//! Tantivy base + overlay store (spec §6). OWNER: indexer agent.

use crate::schema::build_schema;
use search_core::{AppError, AppResult, IndexManifest, Provenance, SearchResult};
use std::path::{Path, PathBuf};

pub struct IndexerConfig {
    pub base_dir: PathBuf,
    pub overlay_dir: PathBuf,
}

pub struct Indexer {
    schema: tantivy::schema::Schema,
    base_dir: PathBuf,
    overlay_dir: PathBuf,
}

impl Indexer {
    pub fn open(cfg: IndexerConfig) -> AppResult<Self> {
        std::fs::create_dir_all(&cfg.base_dir)?;
        std::fs::create_dir_all(&cfg.overlay_dir)?;
        Ok(Self {
            schema: build_schema(),
            base_dir: cfg.base_dir,
            overlay_dir: cfg.overlay_dir,
        })
    }

    pub fn schema(&self) -> &tantivy::schema::Schema {
        &self.schema
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }
    pub fn overlay_dir(&self) -> &Path {
        &self.overlay_dir
    }

    /// Search both base and overlay, returning candidates tagged with provenance.
    pub fn search(&self, _query: &str, _limit: usize) -> AppResult<Vec<SearchResult>> {
        Err(AppError::Internal("indexer::search not yet implemented".into()))
    }

    /// Publish a new manifest generation.
    pub fn publish_manifest(&self) -> AppResult<IndexManifest> {
        Err(AppError::Internal("indexer::publish_manifest not yet implemented".into()))
    }

    /// Current overlay watermark.
    pub fn overlay_watermark(&self) -> u64 {
        0
    }

    pub fn provenance_of(&self, _doc_id: &str) -> Provenance {
        Provenance::Snapshot
    }
}
