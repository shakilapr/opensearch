//! Read-only index handles: the property that lets a server run beside a crawler.
//!
//! Tantivy's `IndexWriter` takes an exclusive lock on the index directory, so two handles that
//! both want to write cannot coexist — which is exactly why `agent0 serve` could not run while
//! `agent0 crawl` was crawling the same data root. A reader-only open takes no lock, and every
//! write on it fails with a sentence a caller can log instead of a panic.

use search_indexer::{IndexDoc, Indexer, IndexerConfig};

fn doc(id: &str, body: &str) -> IndexDoc {
    IndexDoc::new(id, format!("res_{id}"), format!("https://example.com/{id}"), "Title", body)
}

#[test]
fn a_reader_opens_while_a_writer_holds_the_lock_and_refuses_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("tantivy");

    let writer = Indexer::open(IndexerConfig::new(&root)).expect("writer open");
    writer.index_batch(vec![doc("d1", "paged attention reduces memory")]).expect("index a doc");
    assert!(!writer.is_read_only());

    // The whole point: this succeeds *while* `writer` still holds the index lock.
    let reader = Indexer::open(IndexerConfig::read_only(&root))
        .expect("a read-only open must not need the writer lock");
    assert!(reader.is_read_only());

    // It serves reads from the same directory.
    let docs = reader.all_docs().expect("read-only all_docs");
    assert!(
        docs.iter().any(|d| d.doc_id == "d1"),
        "the reader must see what the writer committed: {:?}",
        docs.iter().map(|d| d.doc_id.clone()).collect::<Vec<_>>()
    );

    // And every write path fails with a clear reason rather than panicking.
    for err in [
        reader.index_batch(vec![doc("d2", "x")]).unwrap_err().to_string(),
        reader.index_overlay(doc("d3", "y")).unwrap_err().to_string(),
        reader.delete("d1").unwrap_err().to_string(),
    ] {
        assert!(err.contains("read-only"), "the error must say why: {err}");
    }

    // The writer is unaffected by the reader's presence.
    writer.index_batch(vec![doc("d4", "z")]).expect("writer still works");
}

#[test]
fn a_second_writer_is_refused_on_the_same_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("tantivy");
    let _first = Indexer::open(IndexerConfig::new(&root)).expect("first writer open");

    let second = Indexer::open(IndexerConfig::new(&root));
    assert!(
        second.is_err(),
        "two writers on one directory must not both succeed; that is the failure the read-only \
         mode exists to avoid"
    );
}
