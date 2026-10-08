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
//! The watermark is stored in the `published_watermark` table (a single row,
//! created lazily by this module because it is owned by the ingest/publish
//! path). It is **monotonic**: `publish` never lowers it, so a stale publisher
//! cannot make already-visible documents disappear.

use search_core::{AppError, AppResult};
use search_metadata::{db_err, MetadataReader, MetadataWriter};

/// Publication watermark barrier.
///
/// Persisted as a single row in `published_watermark`:
/// `(singleton INTEGER PRIMARY KEY CHECK(singleton = 1), generation, updated_at)`.
pub struct Watermark;

impl Watermark {
    /// Sentinel row id; the table holds exactly one row.
    const SINGLETON: i64 = 1;

    fn ensure_table(conn: &rusqlite::Connection) -> AppResult<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS published_watermark (
                 singleton  INTEGER PRIMARY KEY CHECK (singleton = 1),
                 generation INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL
             );",
        )
        .map_err(db_err)?;
        Ok(())
    }

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
        Self::ensure_table(conn)?;
        let now = search_core::time::now_unix_s();
        conn.execute(
            "INSERT INTO published_watermark(singleton, generation, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(singleton) DO UPDATE SET
                 generation = MAX(published_watermark.generation, excluded.generation),
                 updated_at = excluded.updated_at",
            rusqlite::params![Self::SINGLETON, generation as i64, now],
        )
        .map_err(db_err)?;
        tracing::debug!(generation, "published watermark advanced");
        Ok(())
    }

    /// The highest generation currently visible to search.
    ///
    /// Returns `0` when nothing has been published yet (nothing is visible), or
    /// when the table has not been created.
    pub fn visible(reader: &MetadataReader) -> AppResult<u64> {
        let conn = reader.conn();
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'published_watermark'",
                [],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        if exists == 0 {
            return Ok(0);
        }
        let g: i64 = conn
            .query_row(
                "SELECT generation FROM published_watermark WHERE singleton = ?1",
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
