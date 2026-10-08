//! Publish watermark for search visibility gating (spec §3 rule 3, §6).
//!
//! A document version is returned by `/search` only when **both** hold:
//!
//! 1. its `ingest_op.state = VISIBLE`, and
//! 2. its `doc_version` is covered by a published manifest generation, i.e.
//!    its Tantivy `visible_generation` field is `<=` the published watermark.
//!
//! Tantivy has no transactions, so the watermark is the publication barrier
//! between "written to the index" and "visible to readers". The indexer writes
//! documents with `visible_generation = G`; a search only considers documents
//! whose `visible_generation <= Watermark::visible()`. Raising the watermark is
//! therefore the atomic publish step that makes a whole generation searchable.
//!
//! The watermark lives in the `published_watermark` table (a single row). The
//! **schema is owned by metadata** and created by migration
//! `crates/metadata/migrations/0002_watermark.sql` — this module never runs DDL.
//! The watermark is **monotonic**: `publish` never lowers it, so a stale
//! publisher cannot make already-visible documents disappear.

use search_core::{AppError, AppResult};
use search_metadata::{db_err, MetadataReader, MetadataWriter};

/// Publication watermark barrier.
///
/// Persisted as a single row in `published_watermark`:
/// `(id INTEGER PRIMARY KEY CHECK(id = 1), generation, published_at)`.
pub struct Watermark;

impl Watermark {
    /// Sentinel row id; the table holds exactly one row.
    const SINGLETON: i64 = 1;

    /// Publish `generation` as the new visible watermark.
    ///
    /// Monotonic: if `generation` is not greater than the current watermark the
    /// call is a no-op (the watermark is never lowered).
    ///
    /// Takes the [`MetadataWriter`] because publishing is a write. The reader
    /// handle is read-only (`SQLITE_OPEN_READ_ONLY`) in this workspace, so the
    /// publish path must own the single writer — matching spec §4 (one process
    /// owns the writer).
    pub fn publish(writer: &MetadataWriter, generation: u64) -> AppResult<()> {
        let conn = writer.conn();
        let now = search_core::time::now_unix_s();
        conn.execute(
            "INSERT INTO published_watermark(id, generation, published_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET
                 generation = MAX(published_watermark.generation, excluded.generation),
                 published_at = excluded.published_at",
            rusqlite::params![Self::SINGLETON, generation as i64, now],
        )
        .map_err(db_err)?;
        tracing::debug!(generation, "published watermark advanced");
        Ok(())
    }

    /// The highest generation currently visible to search.
    ///
    /// Returns `0` when nothing has been published yet (nothing is visible).
    /// The table always exists — it is created by migration 0002 — so a missing
    /// row simply means \"no publish yet\".
    pub fn visible(reader: &MetadataReader) -> AppResult<u64> {
        let conn = reader.conn();
        let g: i64 = conn
            .query_row(
                "SELECT generation FROM published_watermark WHERE id = ?1",
                rusqlite::params![Self::SINGLETON],
                |r| r.get(0),
            )
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(0),
                other => Err(db_err(other)),
            })?;
        Ok(g.max(0) as u64)
    }

    /// Whether a document with generation `visible_generation` is searchable
    /// under the current `published` watermark. This is the exact predicate the
    /// Tantivy query applies as a fast-field filter.
    pub fn is_visible(visible_generation: u64, published: u64) -> bool {
        visible_generation <= published
    }

    /// Reject a publish attempt that would violate the barrier, used by the
    /// publish path before committing a manifest.
    pub fn check_publish(generation: u64, current: u64) -> AppResult<()> {
        if generation < current {
            return Err(AppError::Invalid(format!(
                "refusing to lower publish watermark from {current} to {generation}"
            )));
        }
        Ok(())
    }
}
