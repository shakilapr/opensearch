//! Acceptance test 9: freshness columns. The article's `article:published_time`
//! meta is persisted to the resource's `published_at`, distinct from the fetch
//! timestamps. Hermetic: loopback fixture + tempdir data root.

mod common;

use common::*;
use search_agent0::Pipeline;

#[tokio::test]
async fn freshness_columns_published_at() {
    let (base, _fx) = spawn_fixture().await;
    let url = format!("{base}/article");
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());
    let p = Pipeline::open_with(&cfg, true).unwrap();

    assert!(p.crawl_one(&url).await.unwrap().is_ingested());
    let rid = p.resource_id_for(&url).unwrap();

    let (published, modified, fetched, verified) = open_db(&cfg)
        .query_row(
            "SELECT published_at, modified_at, fetched_at, verified_at FROM resource \
             WHERE resource_id = ?1",
            rusqlite::params![rid],
            |r| {
                Ok((
                    r.get::<_, Option<i64>>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .expect("the article resource row exists");

    assert_eq!(
        published,
        Some(ARTICLE_PUBLISHED_AT),
        "published_at comes from the HTML meta (2024-03-05T10:00:00Z)"
    );
    assert!(
        modified.is_none(),
        "the fixture has no modified timestamp"
    );
    assert!(fetched.is_some(), "fetched_at is populated");
    assert!(verified.is_some(), "verified_at is populated");

    // The published date is also surfaced on the search result.
    let resp = p.search("zephyrarticleword", 10).await.unwrap();
    assert_eq!(resp.results.len(), 1);
    assert_eq!(
        resp.results[0].published_at,
        Some(ARTICLE_PUBLISHED_AT),
        "published_at round-trips onto the search result"
    );

    println!(
        "PASS freshness_columns_published_at: published_at={published:?}, fetched/verified populated, modified=None"
    );
}
