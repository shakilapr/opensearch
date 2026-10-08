//! metadata: the SQLite single-writer subsystem (spec §4).
//!
//! One process owns the writer. All other tasks emit [`Mutation`]s over a
//! bounded channel to a [`writer::MetadataWriter`] that batches transactions.
//! SQLite permits only one WAL writer at a time, so N crawler tasks must never
//! contend directly on the database.
//!
//! The `ingest_event` table is the append-only source of truth; everything else
//! (including `ingest_op`) is a projection that must be rebuildable from the
//! journal.

pub mod migrations;
pub mod schema;
pub mod writer;

pub use schema::{DocVersionRow, FrontierRow, IngestEventRow, ManifestRow, ResourceRow};
pub use writer::{
    spawn_writer, spawn_writer_with, MetadataReader, MetadataWriter, Mutation, WriterHandle,
    DEFAULT_BATCH_SIZE, DEFAULT_CHANNEL_CAPACITY, DEFAULT_FLUSH_INTERVAL_MS,
};

/// Map a rusqlite error into the shared taxonomy. Lives here (not in core) to
/// keep `core` free of storage dependencies.
pub fn db_err(e: rusqlite::Error) -> search_core::AppError {
    search_core::AppError::Db(e.to_string())
}
