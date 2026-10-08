//! ingest: the consistency boundary (spec §3).
//!
//! Owns the ingestion state machine
//! `FETCHED -> NORMALIZED -> BLOB_WRITTEN -> METADATA_COMMITTED
//!  -> LEXICAL_INDEXED -> GRAPH_INDEXED -> VISIBLE`
//! and coordinates projections into metadata, Tantivy, and the graph.
//!
//! Every projection is idempotent keyed on `(op_key, stage)`. Search exposes
//! only versions below the published watermark.

pub mod journal;
pub mod pipeline;

pub use journal::{IngestStage, IngestState};
pub use pipeline::IngestCoordinator;
