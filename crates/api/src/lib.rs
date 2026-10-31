//! api: `POST /search`, `GET /health`, `POST /search_deep` and `GET /read`
//! (spec §7 + Stage-4/7 HTTP extension).
//!
//! The crate depends only on `search-core` plus the small [`query::Searcher`]
//! and [`deep::DeepSearcher`] traits; the real indexer is wired in behind the
//! `real-indexer` feature via [`indexer_adapter`], so this crate always compiles
//! independently.

pub mod deep;
pub mod engine;
pub mod federate;
pub mod graph_lane;
pub mod query;
pub mod server;

#[cfg(feature = "real-indexer")]
pub mod indexer_adapter;

pub use deep::{
    DeepHit, DeepSearchRequest, DeepSearchResponse, DeepSearcher, DocResponse,
};
pub use federate::FederatedSearcher;
pub use engine::{
    lane_name, run_engine_search, stop_reason_name, EngineCoverage, EngineMode, EngineResult,
    EngineSearchRequest, EngineSearchResponse, SearcherLane,
};
pub use query::{analyze, build_signals, execute, IndexHit, Searcher, DEFAULT_LIMIT, MAX_LIMIT};
pub use server::{
    cors_layer_from_origins, router, router_with_cors, serve, serve_with, AppState, ApiError,
    DEFAULT_MAX_CONCURRENCY,
};

#[cfg(feature = "real-indexer")]
pub use indexer_adapter::IndexerSearcher;

/// Initialise the process-wide tracing subscriber.
///
/// Honours `RUST_LOG` (via `EnvFilter`), defaulting to `info` when unset or
/// invalid. Idempotent: a second call is a no-op, so a binary can call this and
/// [`serve`] can call it again safely. Logs go to stderr; no credentials or
/// document bodies are ever logged by this crate.
pub fn init_tracing() {
    use std::io::IsTerminal;
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // ANSI only to a terminal. `tracing_subscriber::fmt()` colours unconditionally by default, so
    // every crawler log written through `>> shard-N.log` contained escape sequences: the field
    // values came out as `origin=\x1b[2m\x1b[0mhttps://host`, which defeated grep, awk and the
    // progress watcher's own parsing. Measured 2026-10-31 while diagnosing a stall: the watcher
    // could not read a single ingest timestamp out of a log it had been writing for days.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(std::io::stdout().is_terminal())
        .try_init();
}
