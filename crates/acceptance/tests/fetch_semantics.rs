//! Acceptance tests 4–8: robots denial, the SSRF block, conditional `304`
//! revalidation, discovery without a full fetch, and PDF routing. All hermetic:
//! a loopback fixture server + a tempdir data root.

mod common;

use common::*;
use search_agent0::Pipeline;
use search_core::AppError;

// ---------------------------------------------------------------------------
// (4) robots denial: refused before fetch, nothing indexed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn robots_denial_blocks_and_indexes_nothing() {
    let (base, _fx) = spawn_fixture().await;
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());
    let p = Pipeline::open_with(&cfg, true).unwrap();

    // A permitted page first, so the index exists.
    assert!(p
        .crawl_one(&format!("{base}/page"))
        .await
        .unwrap()
        .is_ingested());

    let blocked_url = format!("{base}/blocked");
    let blocked_rid = p.resource_id_for(&blocked_url).unwrap();
    let out = p.crawl_one(&blocked_url).await.unwrap();
    assert!(out.is_denied(), "robots-disallowed path must be Denied: {out:?}");
    assert_eq!(
        dv_count(&cfg, &blocked_rid),
        0,
        "no DocumentVersion for a robots-denied URL"
    );
    assert!(
        stages_for(&cfg, &blocked_rid).is_empty(),
        "no journal stages for a robots-denied URL"
    );

    // Its distinctive word is not searchable.
    assert!(
        p.search("zzzdeniedword", 10).await.unwrap().results.is_empty(),
        "robots-denied content must not be indexed"
    );
    // No collateral damage: the permitted page is still there.
    assert_eq!(p.search("zephyrquokka", 10).await.unwrap().results.len(), 1);

    println!(
        "PASS robots_denial_blocks_and_indexes_nothing: /blocked denied, 0 versions, word unindexed"
    );
}

// ---------------------------------------------------------------------------
// (5) SSRF block: the cloud-metadata IP is refused before any fetch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ssrf_block_metadata_ip_before_fetch() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());
    let p = Pipeline::open_with(&cfg, true).unwrap();

    let target = "http://169.254.169.254/latest/meta-data/";
    let err = p.crawl_one(target).await.unwrap_err();
    assert!(
        matches!(err, AppError::SsrfBlocked(_)),
        "cloud-metadata IP must be SSRF-blocked: {err:?}"
    );

    let rid = p.resource_id_for(target).unwrap();
    assert!(
        stages_for(&cfg, &rid).is_empty(),
        "nothing was recorded for the blocked resource"
    );

    println!(
        "PASS ssrf_block_metadata_ip_before_fetch: 169.254.169.254 -> AppError::SsrfBlocked"
    );
}

// ---------------------------------------------------------------------------
// (6) conditional 304 revalidation → Unchanged, still one version
// ---------------------------------------------------------------------------

#[tokio::test]
async fn conditional_304_is_unchanged() {
    let (base, fx) = spawn_fixture().await;
    let url = format!("{base}/article");
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());
    let p = Pipeline::open_with(&cfg, true).unwrap();
    let rid = p.resource_id_for(&url).unwrap();

    // First fetch stores the ETag validator.
    assert!(p.crawl_one(&url).await.unwrap().is_ingested());
    assert_eq!(dv_count(&cfg, &rid), 1);

    // A second fetch sends If-None-Match; the fixture answers 304.
    let before = fx.not_modified();
    let out = p.crawl_one(&url).await.unwrap();
    assert!(out.is_unchanged(), "a 304 must be Unchanged: {out:?}");
    assert!(
        fx.not_modified() > before,
        "the fixture served a 304 on the conditional revalidation"
    );
    assert_eq!(dv_count(&cfg, &rid), 1, "no second DocumentVersion on 304");
    assert_eq!(p.search("zephyrarticleword", 10).await.unwrap().results.len(), 1);

    println!(
        "PASS conditional_304_is_unchanged: If-None-Match -> 304 -> Unchanged, dv_count stays 1"
    );
}

// ---------------------------------------------------------------------------
// (7) discovery without a full fetch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn discovery_without_full_fetch() {
    let (base, fx) = spawn_fixture().await;
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());
    let p = Pipeline::open_with(&cfg, true).unwrap();

    let report = p.discover(&format!("{base}/")).await.unwrap();
    assert_eq!(report.seed_url, format!("{base}/"));
    assert!(report.sitemaps >= 1, "a sitemap was persisted: {report:?}");
    assert_eq!(report.feeds, 1, "one feed was discovered: {report:?}");
    assert!(
        report.resources >= 3,
        "article + pdf + latest + feed entry pushed: {report:?}"
    );

    // The discovery paths were fetched...
    let paths = fx.paths();
    for expected in ["/", "/robots.txt", "/sitemap.xml", "/feed.xml"] {
        assert!(
            paths.iter().any(|p| p == expected),
            "discovery should fetch {expected}: {paths:?}"
        );
    }
    // ... but the advertised pages themselves were NOT crawled.
    for not_expected in ["/article", "/pdf/doc.pdf", "/feed-entry", "/latest"] {
        assert!(
            !paths.iter().any(|p| p == not_expected),
            "discovery must NOT fetch every page ({not_expected}): {paths:?}"
        );
    }

    println!(
        "PASS discovery_without_full_fetch: sitemaps={} feeds={} resources={} pages_not_fetched",
        report.sitemaps, report.feeds, report.resources
    );
}

// ---------------------------------------------------------------------------
// (8) PDF routing: the PDF is routed and its text is indexed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pdf_routing_indexes_text() {
    let (base, _fx) = spawn_fixture().await;
    let url = format!("{base}/pdf/doc.pdf");
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());
    let p = Pipeline::open_with(&cfg, true).unwrap();

    let out = p.crawl_one(&url).await.unwrap();
    assert!(out.is_ingested(), "the PDF must be routed + ingested: {out:?}");

    let resp = p.search(PDF_MARKER, 10).await.unwrap();
    assert_eq!(resp.results.len(), 1, "the PDF text is searchable");
    assert_eq!(resp.results[0].canonical_url, url, "PDF url round-trips");

    println!(
        "PASS pdf_routing_indexes_text: routed {url}, marker '{PDF_MARKER}' searchable"
    );
}
