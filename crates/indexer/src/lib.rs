//! indexer: Tantivy base + overlay with manifest publish (spec §6).

pub mod doc;
pub mod schema;
pub mod store;

pub use doc::IndexDoc;
pub use store::{Indexer, IndexerConfig};
