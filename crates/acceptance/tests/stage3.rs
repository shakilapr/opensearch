//! Acceptance tests 10–11: Stage-3 freshness. A time-sensitive query triggers a
//! P0 refresh that surfaces a newly fetched document; a scheduled burst never
//! exceeds the per-origin or total fan-out caps. Hermetic: loopback fixture +
//! tempdir data root, with a scheduler config chosen by each test.

mod common;

use common::*;
use search_agent0::Pipeline;
use search_core::manifest::Provenance;
use search_scheduler::{SchedulerConfig, SourceClass, SourceDue};

// ---------------------------------------------------------------------------
// (10) query-triggered refresh surfaces a new doc
// ---------------------------------------------------------------------------

#[tokio::test]
async fn query_refresh_surfaces_new_doc() {
    let (base, fx) = spawn_fixture().await;
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());

    // A generous query-refresh window so /latest is definitely dispatched.
    let sched = SchedulerConfig {
        query_refresh_limit: 32,
        ..Default::default()
    };
    let p = Pipeline::open_with_scheduler(&cfg, true, sched, 10_000).unwrap();

    // Discovery learns /latest from the sitemap and queues it, but does NOT
    // crawl the page itself.
    let report = p.discover(&format!("{base}/")).await.unwrap();
    assert!(report.resources >= 1, "the sitemap advertised pages: {report:?}");
    assert!(
        !fx.paths().iter().any(|p| p == "/latest"),
        "discovery must not crawl the advertised page: {:?}",
        fx.paths()
    );

    let now = search_core::time::now_unix_s();

    // The document is NOT in the index before the refresh.
    assert!(
        p.search("zephyrlatestword", 10).await.unwrap().results.is_empty(),
        "the doc must be absent before the refresh"
    );

    // A time-sensitive query triggers a P0 refresh.
    let refresh = p
        .query_refresh("latest zephyrlatestword", now)
        .await
        .unwrap();
    assert!(refresh.admitted >= 1, "a P0 activation was admitted: {refresh:?}");
    assert!(
        refresh.new_documents >= 1,
        "the refresh fetched at least one new document: {refresh:?}"
    );
    assert!(
        fx.paths().iter().any(|p| p == "/latest"),
        "the P0 refresh fetched /latest: {:?}",
        fx.paths()
    );

    // The newly fetched document is immediately searchable from the live overlay.
    let resp = p.search("zephyrlatestword", 10).await.unwrap();
    assert_eq!(resp.results.len(), 1, "the freshly fetched doc is searchable");
    let hit = &resp.results[0];
    assert!(hit.canonical_url.ends_with("/latest"), "url round-trips: {}", hit.canonical_url);
    assert_eq!(
        hit.provenance,
        Provenance::Overlay,
        "it comes from the live overlay before any global reindex"
    );

    println!(
        "PASS query_refresh_surfaces_new_doc: admitted={} new_documents={} -> overlay hit /latest",
        refresh.admitted, refresh.new_documents
    );
}

// ---------------------------------------------------------------------------
// (11) per-origin fan-out caps hold under a burst
// ---------------------------------------------------------------------------

fn source(origin: &str, url: &str, class: SourceClass) -> SourceDue {
    SourceDue {
        origin: origin.to_string(),
        class,
        change_rate: 0.01,
        last_fetch: 0,
        next_allowed_at: 0,
        url: url.to_string(),
    }
}

#[tokio::test]
async fn per_origin_fanout_caps_hold() {
    let (base, fx) = spawn_fixture().await;

    // Two distinct origins that resolve to the same fixture server, so the
    // fixture records genuinely per-origin request counts.
    let parsed = url::Url::parse(&base).unwrap();
    let port = parsed.port().unwrap();
    let base_host = format!("127.0.0.1:{port}");
    let alt = format!("http://localhost:{port}");
    let alt_host = format!("localhost:{port}");

    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());

    let sched = SchedulerConfig {
        total_live_fanout: 4,
        per_origin_fanout: 2,
        query_refresh_limit: 8,
        ..Default::default()
    };
    let p = Pipeline::open_with_scheduler(&cfg, true, sched.clone(), 10_000).unwrap();

    // 4 due sources per origin (8 total) — far more than the caps allow.
    let mut sources = Vec::new();
    for i in 0..4 {
        sources.push(source(
            &format!("http://{base_host}"),
            &format!("http://{base_host}/burst/a{i}"),
            SourceClass::Hot,
        ));
    }
    for i in 0..4 {
        sources.push(source(
            &alt,
            &format!("{alt}/burst/b{i}"),
            SourceClass::Hot,
        ));
    }

    let now = search_core::time::now_unix_s();
    let report = p.run_refresh_tick(&sources, now).await.unwrap();

    // The scheduler never admits more than the total fan-out cap.
    assert!(
        report.admitted <= sched.total_live_fanout,
        "admitted {} > total cap {}",
        report.admitted,
        sched.total_live_fanout
    );

    let a: Vec<String> = fx
        .hits_for(&base_host)
        .into_iter()
        .filter(|h| !h.ends_with("/robots.txt"))
        .collect();
    let b: Vec<String> = fx
        .hits_for(&alt_host)
        .into_iter()
        .filter(|h| !h.ends_with("/robots.txt"))
        .collect();
    let total = a.len() + b.len();

    assert!(
        total <= sched.total_live_fanout,
        "server saw {total} fetches > total cap {}",
        sched.total_live_fanout
    );
    assert!(
        a.len() <= sched.per_origin_fanout,
        "origin {base_host} saw {} > per-origin cap {}",
        a.len(),
        sched.per_origin_fanout
    );
    assert!(
        b.len() <= sched.per_origin_fanout,
        "origin {alt_host} saw {} > per-origin cap {}",
        b.len(),
        sched.per_origin_fanout
    );
    // The burst carried enough work that both caps actually bound it.
    assert_eq!(a.len(), sched.per_origin_fanout, "first origin fills its cap");
    assert_eq!(b.len(), sched.per_origin_fanout, "second origin fills its cap");

    println!(
        "PASS per_origin_fanout_caps_hold: origin_a={} origin_b={} total={total} (caps 2/2/4)",
        a.len(),
        b.len()
    );
}
