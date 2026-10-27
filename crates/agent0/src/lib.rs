//! `agent0` — the runnable Stage-0 vertical slice.
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
//!
//! ## Stage 4 — deep navigation
//!
//! [`deep`] wires `crates/nav`'s [`search_nav::Navigator`] into the pipeline: a
//! real [`search_nav::SearchExecutor`] over the index + retrieval lanes, a
//! per-phase trace emitter, and [`Pipeline::deep_search`] (see
//! `docs/STAGE4_SPEC.md`).

pub mod crawl;
pub mod embedbuild;
pub mod evalset;
pub mod frontier_recover;
pub mod graphbuild;
pub mod deep;
pub mod pipeline;
pub mod planner_cache;
pub mod portable;
pub mod seeds;

pub use deep::{
    run_deep_search, DeepHit, DeepResult, DeepSearchReport, DocText, NoopEmitter, Phase,
    PhaseEmitter, PhaseEvent, PipelineDeepSearcher,
};
pub use search_api::init_tracing;
pub use search_nav::StopReason;
pub use search_trace::{JsonlTraceSink, PolicyTrace, TraceEvent};
pub use pipeline::{
    open_honouring_read_only, scheduler_config_from_env, DiscoveryReport, ImportRecord,
    IngestOutcome, Pipeline, RefreshReport, SourceFirstSearcher, DEFAULT_REFRESH_BUDGET,
};
