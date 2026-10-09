//! The portable knowledge model, end to end: crawl a page into machine A, compile the index
//! into a pack, carry the pack to a *fresh* machine B and query it — no network on B.
//!
//! Hermetic: a loopback fixture server for A, two tempdirs for the two data roots, and no
//! model call (`--raw` semantics). Nothing touches the public internet or the real data root.

mod common;

use common::*;
use search_agent0::{ImportRecord, Pipeline};

#[tokio::test]
async fn export_then_import_round_trips_a_real_page() {
    let (base, _fx) = spawn_fixture().await;
    let url = format!("{base}/page");

    // ---- machine A: scrape and index ----
    let dir_a = tempfile::TempDir::new().unwrap();
    let cfg_a = test_config(dir_a.path());
    let docs = {
        let p = Pipeline::open_with(&cfg_a, true).unwrap();
        assert!(p.crawl_one(&url).await.unwrap().is_ingested());
        p.all_docs().unwrap()
    };
    assert_eq!(docs.len(), 1, "one live doc to export");

    // ---- compile: index -> portable pack ----
    let (pages, skipped) = search_agent0::portable::pages_from_docs(&docs, usize::MAX);
    assert_eq!(pages.len(), 1);
    assert_eq!(skipped, 0, "the crawled page carries url and body");
    let records: Vec<_> = pages
        .iter()
        .map(search_agent0::portable::raw_record)
        .collect();

    let pack_dir = tempfile::TempDir::new().unwrap();
    let manifest = search_dataset::write_pack(pack_dir.path(), "acceptance", "1", records, 100)
        .expect("pack writes");
    assert_eq!(manifest.name, "acceptance");

    // The pack is self-describing and verifiable on disk.
    let read_back = search_dataset::read_pack(pack_dir.path()).expect("pack reads back");
    assert_eq!(read_back.len(), 1, "one record survives the pack");
    let rec = &read_back[0];
    assert_eq!(rec.url.as_deref(), Some(url.as_str()));
    assert_eq!(rec.title.as_deref(), Some("Stage Eight Page"));
    assert!(rec.text.contains("zephyrquokka"), "body text travels in the pack");

    // ---- machine B: import the pack into a fresh data root, then query it ----
    let dir_b = tempfile::TempDir::new().unwrap();
    let cfg_b = test_config(dir_b.path());
    let p_b = Pipeline::open_with(&cfg_b, true).unwrap();

    let import = ImportRecord {
        url: rec.url.clone().unwrap(),
        title: rec.title.clone(),
        text: rec.text.clone(),
        source: rec.source.clone(),
        published_at: rec.published_at,
        extras: rec.extras.clone(),
    };
    assert!(
        p_b.import_record(&import).unwrap().is_ingested(),
        "a pack record ingests on a machine that never crawled"
    );

    let resp = p_b.search("zephyrquokka", 10).await.unwrap();
    assert_eq!(resp.results.len(), 1, "the imported page is searchable on B");
    assert_eq!(resp.results[0].canonical_url, url, "url round-trips");
    assert_eq!(resp.results[0].title, "Stage Eight Page", "title round-trips");

    // ---- the import is idempotent: the same pack again changes nothing ----
    assert!(
        p_b.import_record(&import).unwrap().is_unchanged(),
        "re-importing the same pack is a no-op, not a duplicate"
    );
    assert_eq!(
        p_b.search("zephyrquokka", 10).await.unwrap().results.len(),
        1,
        "still exactly one document after a repeated import"
    );

    println!(
        "PASS export_then_import_round_trips_a_real_page: {url} -> pack({} record(s)) -> machine B",
        read_back.len()
    );
}
