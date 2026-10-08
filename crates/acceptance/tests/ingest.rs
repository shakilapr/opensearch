//! Acceptance tests 1–3: the ingest → search round trip, restart safety and
//! duplicate rejection. Hermetic: a loopback fixture server + a tempdir data
//! root; no public internet, no fixed port.

mod common;

use common::*;
use search_agent0::Pipeline;

// ---------------------------------------------------------------------------
// (1) ingest → search round trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ingest_search_round_trip() {
    let (base, _fx) = spawn_fixture().await;
    let url = format!("{base}/page");
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());

    let p = Pipeline::open_with(&cfg, true).unwrap();
    let rid = p.resource_id_for(&url).unwrap();

    let out = p.crawl_one(&url).await.unwrap();
    assert!(out.is_ingested(), "a fresh page must be ingested: {out:?}");
    assert_eq!(dv_count(&cfg, &rid), 1, "exactly one DocumentVersion");

    let resp = p.search("zephyrquokka", 10).await.unwrap();
    assert_eq!(resp.results.len(), 1, "the ingested page is searchable");
    let hit = &resp.results[0];
    assert_eq!(hit.canonical_url, url, "url round-trips");
    assert_eq!(hit.title, "Stage Eight Page", "title round-trips");
    assert_eq!(hit.resource_id, rid, "resource identity is stable");
    assert!(hit.score > 0.0, "non-trivial score");

    println!(
        "PASS ingest_search_round_trip: ingested {url}, search returned {} hit(s)",
        resp.results.len()
    );
}

// ---------------------------------------------------------------------------
// (2) restart safety: reopen the same data dir → same result
// ---------------------------------------------------------------------------

#[tokio::test]
async fn restart_safety_reopen_same_result() {
    let (base, _fx) = spawn_fixture().await;
    let url = format!("{base}/page");
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());

    let rid = {
        let p = Pipeline::open_with(&cfg, true).unwrap();
        let rid = p.resource_id_for(&url).unwrap();
        assert!(p.crawl_one(&url).await.unwrap().is_ingested());
        assert_eq!(p.search("zephyrquokka", 10).await.unwrap().results.len(), 1);
        rid
    };

    // Drop the pipeline entirely and reopen on the same data directory.
    let p = Pipeline::open_with(&cfg, true).unwrap();
    let resp = p.search("zephyrquokka", 10).await.unwrap();
    assert_eq!(resp.results.len(), 1, "the result survives a restart");
    assert_eq!(resp.results[0].canonical_url, url);
    assert_eq!(resp.results[0].title, "Stage Eight Page");
    assert_eq!(resp.results[0].resource_id, rid, "resource id is stable");

    println!(
        "PASS restart_safety_reopen_same_result: reopened data dir, {} result(s) identical",
        resp.results.len()
    );
}

// ---------------------------------------------------------------------------
// (3) duplicate rejection: re-ingest identical bytes → Unchanged, 1 version
// ---------------------------------------------------------------------------

#[tokio::test]
async fn duplicate_reingest_is_unchanged() {
    let (base, _fx) = spawn_fixture().await;
    let url = format!("{base}/page");
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());

    let p = Pipeline::open_with(&cfg, true).unwrap();
    let rid = p.resource_id_for(&url).unwrap();

    assert!(p.crawl_one(&url).await.unwrap().is_ingested());
    assert_eq!(dv_count(&cfg, &rid), 1);

    let stages_before = stages_for(&cfg, &rid);

    let out = p.crawl_one(&url).await.unwrap();
    assert!(
        out.is_unchanged(),
        "re-ingesting identical bytes must be Unchanged: {out:?}"
    );
    assert_eq!(
        dv_count(&cfg, &rid),
        1,
        "no second DocumentVersion for unchanged content"
    );
    assert_eq!(
        stages_for(&cfg, &rid),
        stages_before,
        "the journal is not re-applied for unchanged content"
    );
    assert_eq!(
        p.search("zephyrquokka", 10).await.unwrap().results.len(),
        1,
        "still exactly one searchable document"
    );

    println!("PASS duplicate_reingest_is_unchanged: re-ingest -> Unchanged, dv_count stays 1");
}
