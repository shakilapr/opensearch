//! agent0 — the runnable Stage-0 vertical slice.
//!
//! This crate is the *driver*: it composes the frozen pieces (frontier leases,
//! fetcher + robots + SSRF, parser normalization, the ingest journal/state
//! machine, the metadata single-writer, the Tantivy indexer, and the `/search`
//! API) into one process.
//!
//! The single entry point is [`Pipeline`]. The Stage-0 lifecycle it walks for a
//! single URL is, in order:
//!
//! ```text
//! lease -> robots check -> SSRF guard -> fetch -> normalize
//!       -> ContentId -> DocumentVersion -> journal stages
//!       -> Tantivy (overlay) -> publish manifest -> VISIBLE
//! ```
//!
//! Journal stages recorded (spec §3): `FETCHED -> NORMALIZED -> BLOB_WRITTEN ->
//! METADATA_COMMITTED -> LEXICAL_INDEXED -> GRAPH_INDEXED -> VISIBLE`.
//! `GRAPH_INDEXED` is a recorded no-op in Stage 0: there is no graph projection
//! yet, but the stage is still journalled so the frozen walk stays intact.

pub mod pipeline;

pub use pipeline::{IngestOutcome, Pipeline};
