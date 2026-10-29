//! Print the number of live documents in a data root's Tantivy index.
//!
//! `_manifest.json` is written by `publish_manifest`, which the rebuild path does not call, so it
//! goes stale after `agent0 index build`. This reads the index itself.
use search_indexer::{Indexer, IndexerConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for arg in std::env::args().skip(1) {
        let cfg = IndexerConfig::read_only(std::path::Path::new(&arg).join("tantivy"));
        let idx = Indexer::open(cfg)?;
        println!("{arg}: {} documents", idx.doc_count()?);
    }
    Ok(())
}
