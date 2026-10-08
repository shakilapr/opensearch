//! api: `POST /search` and `GET /health` (spec §7).
//!
//! The crate depends only on `search-core` plus the small [`query::Searcher`]
//! trait; the real indexer is wired in behind the `real-indexer` feature via
//! [`indexer_adapter`], so this crate always compiles independently.

pub mod query;
pub mod server;

#[cfg(feature = "real-indexer")]
pub mod indexer_adapter;

pub use query::{analyze, execute, IndexHit, Searcher, DEFAULT_LIMIT, MAX_LIMIT};
pub use server::{router, serve, AppState, ApiError, DEFAULT_MAX_CONCURRENCY};

#[cfg(feature = "real-indexer")]
pub use indexer_adapter::IndexerSearcher;
