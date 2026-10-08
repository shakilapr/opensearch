//! api: `POST /search`, `GET /health`, `POST /search_deep` and `GET /read`
//! (spec §7 + Stage-4/7 HTTP extension).
//!
//! The crate depends only on `search-core` plus the small [`query::Searcher`]
//! and [`deep::DeepSearcher`] traits; the real indexer is wired in behind the
//! `real-indexer` feature via [`indexer_adapter`], so this crate always compiles
//! independently.

pub mod deep;
pub mod query;
pub mod server;

#[cfg(feature = "real-indexer")]
pub mod indexer_adapter;

pub use deep::{
    DeepHit, DeepSearchRequest, DeepSearchResponse, DeepSearcher, DocResponse,
};
pub use query::{analyze, build_signals, execute, IndexHit, Searcher, DEFAULT_LIMIT, MAX_LIMIT};
pub use server::{router, serve, AppState, ApiError, DEFAULT_MAX_CONCURRENCY};

#[cfg(feature = "real-indexer")]
pub use indexer_adapter::IndexerSearcher;
