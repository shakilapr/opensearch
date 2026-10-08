//! indexer: Tantivy base + overlay with manifest publish (spec §6).

pub mod schema;
pub mod store;

pub use store::{Indexer, IndexerConfig};
