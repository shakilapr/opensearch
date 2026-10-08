//! Stage-0 acceptance test: a hermetic, no-public-internet, no-fixed-port
//! end-to-end exercise of `crates/agent0`'s `Pipeline`.
//!
//! Covers the two explicit Stage-0 acceptance criteria — **restart safety** and
//! **no-duplicate re-ingest** — plus robots denial, the HTTP error taxonomy and
//! the SSRF guard. The only network involved is a local axum fixture server
//! bound to `127.0.0.1:0`.

use std::path::Path;

use axum::{
    http::{header, StatusCode},
    response::{Html, IntoResponse},
    routing::get,
    Router,
};

use search_agent0::{IngestOutcome, Pipeline};
use search_core::config::Config;
use search_core::manifest::Provenance;
use search_core::AppError;

// ---------------------------------------------------------------------------
// Fixture server
// ---------------------------------------------------------------------------

const PAGE_HTML: &str = "<!doctype html><html><head><title>Stage Zero Fixture</title>\
</head><body><h1>Stage Zero Fixture</h1>\
<p>Welcome to the zephyrquokka observatory. A zephyrquokka sighting is rare.</p>\
</body></html>";

const BLOCKED_HTML: &str = "<html><head><title>Blocked</title></head>\
<body><p>zzzdeniedword</p></body></html>";

async fn robots_txt() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/plain")],
        "User-agent: *\nDisallow: /blocked\nAllow: /\n",
    )
}

async fn page() -> Html<&'static str> {
    Html(PAGE_HTML)
}

async fn blocked() -> Html<&'static str> {
    Html(BLOCKED_HTML)
}

async fn missing() -> (StatusCode, &'static str) {
    (StatusCode::NOT_FOUND, "not found")
}

async fn boom() -> (StatusCode, &'static str) {
    (StatusCode::INTERNAL_SERVER_ERROR, "boom")
}

/// Start the fixture server on an ephemeral loopback port; returns its base URL.
async fn spawn_fixture_server() -> String {
    let app = Router::new()
        .route("/robots.txt", get(robots_txt))
        .route("/page", get(page))
        .route("/blocked", get(blocked))
        .route("/404", get(missing))
        .route("/500", get(boom));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn test_config(data_dir: &Path) -> Config {
    let mut cfg = Config::default();
    cfg.data_dir = data_dir.to_path_buf();
    cfg.user_agent = "agent0-acceptance-test/0.1 (+test)".to_string();
    cfg.bind = "127.0.0.1:0".to_string();
    cfg
}

fn open_db(cfg: &Config) -> rusqlite::Connection {
    rusqlite::Connection::open(cfg.meta_db()).expect("open metadata db")
}

fn dv_count(cfg: &Config, resource_id: &str) -> i64 {
    open_db(cfg)
        .query_row(
            "SELECT COUNT(*) FROM document_version WHERE resource_id = ?1",
            rusqlite::params![resource_id],
            |r| r.get(0),
        )
        .expect("count document_version")
}

fn stages_for(cfg: &Config, resource_id: &str) -> Vec<String> {
    let conn = open_db(cfg);
    let mut stmt = conn
        .prepare("SELECT stage FROM ingest_event WHERE resource_id = ?1 ORDER BY seq")
        .expect("prepare");
    let rows = stmt
        .query_map(rusqlite::params![resource_id], |r| r.get::<_, String>(0))
        .expect("query");
    rows.map(|r| r.expect("stage")).collect()
}

/// The frozen Stage-0 lifecycle, exactly once, in order.
const FULL_WALK: [&str; 7] = [
    "FETCHED",
    "NORMALIZED",
    "BLOB_WRITTEN",
    "METADATA_COMMITTED",
    "LEXICAL_INDEXED",
    "GRAPH_INDEXED",
    "VISIBLE",
];

// ---------------------------------------------------------------------------
// (a)-(e): ingest, search, restart, no-duplicate re-ingest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stage0_full_lifecycle_restart_and_idempotency() {
    let base = spawn_fixture_server().await;
    let url = format!("{base}/page");
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());

    let rid = {
        let p = Pipeline::open_with(&cfg, true).unwrap();
        let rid = p.resource_id_for(&url).unwrap();

        // (c) ingest and make searchable.
        let out = p.crawl_one(&url).await.unwrap();
        let (generation, content_id) = match &out {
            IngestOutcome::Ingested {
                generation,
                content_id,
                version_no,
                ..
            } => {
                assert_eq!(*version_no, 1, "first ingest is version 1");
                (*generation, content_id.clone())
            }
            other => panic!("expected Ingested, got {other:?}"),
        };
        assert!(generation >= 1, "generation must be nonzero");

        // blob + text exist on disk under the configured data dir.
        let hex = content_id.trim_start_matches("cnt_");
        let blob = cfg.blob_path(hex);
        let text = cfg.text_path(hex);
        assert!(blob.exists(), "raw blob written to {}", blob.display());
        assert!(text.exists(), "text written to {}", text.display());
        assert!(std::fs::read_to_string(&text)
            .unwrap()
            .to_lowercase()
            .contains("zephyrquokka"));

        let resp = p.search("zephyrquokka", 10).await.unwrap();
        assert_eq!(resp.results.len(), 1, "exactly one indexed document");
        let hit = &resp.results[0];
        assert_eq!(hit.canonical_url, url, "url round-trips");
        assert_eq!(hit.title, "Stage Zero Fixture", "title round-trips");
        assert_eq!(hit.resource_id, rid, "resource identity is stable");
        assert_eq!(hit.content_id.as_deref(), Some(content_id.as_str()));
        assert_eq!(
            hit.provenance,
            Provenance::Overlay,
            "fresh ingest is visible from the overlay"
        );
        assert!(resp.overlay_watermark >= 1, "overlay watermark advanced");
        assert!(!resp.index_manifest_id.is_empty(), "manifest published");
        assert!(hit.score > 0.0, "non-trivial score");

        // the journal shows the full frozen walk, once each.
        assert_eq!(stages_for(&cfg, &rid), FULL_WALK.to_vec());
        assert_eq!(dv_count(&cfg, &rid), 1);

        rid
    };

    // (d) RESTART: drop the pipeline, reopen on the same data dir.
    {
        let p = Pipeline::open_with(&cfg, true).unwrap();
        let resp = p.search("zephyrquokka", 10).await.unwrap();
        assert_eq!(resp.results.len(), 1, "result survives restart");
        assert_eq!(resp.results[0].canonical_url, url);
        assert_eq!(resp.results[0].title, "Stage Zero Fixture");
        assert_eq!(resp.results[0].resource_id, rid);
    }

    // (e) RE-INGEST: no duplicate DocumentVersion, no re-applied journal walk.
    {
        let p = Pipeline::open_with(&cfg, true).unwrap();
        let before_stages = stages_for(&cfg, &rid);
        assert_eq!(dv_count(&cfg, &rid), 1);

        let out = p.crawl_one(&url).await.unwrap();
        assert!(
            out.is_unchanged(),
            "re-ingest of identical bytes must be Unchanged, got {out:?}"
        );

        assert_eq!(
            dv_count(&cfg, &rid),
            1,
            "no second DocumentVersion for unchanged content"
        );
        assert_eq!(
            stages_for(&cfg, &rid),
            before_stages,
            "the journal is not re-applied from scratch"
        );

        // still exactly one searchable document.
        let resp = p.search("zephyrquokka", 10).await.unwrap();
        assert_eq!(resp.results.len(), 1);
    }
}

// ---------------------------------------------------------------------------
// (f): robots-denied path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn robots_denied_path_refuses_and_indexes_nothing() {
    let base = spawn_fixture_server().await;
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());

    let p = Pipeline::open_with(&cfg, true).unwrap();

    // A permitted page first, so the index exists and search has a manifest.
    let page_url = format!("{base}/page");
    assert!(p.crawl_one(&page_url).await.unwrap().is_ingested());

    // The disallowed path must be refused without being fetched or indexed.
    let blocked_url = format!("{base}/blocked");
    let blocked_rid = p.resource_id_for(&blocked_url).unwrap();
    let out = p.crawl_one(&blocked_url).await.unwrap();
    assert!(
        out.is_denied(),
        "robots-disallowed path must be Denied, got {out:?}"
    );
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
    let resp = p.search("zzzdeniedword", 10).await.unwrap();
    assert!(
        resp.results.is_empty(),
        "robots-denied content must not be indexed"
    );

    // The permitted page is still there (no collateral damage).
    let resp = p.search("zephyrquokka", 10).await.unwrap();
    assert_eq!(resp.results.len(), 1);
}

// ---------------------------------------------------------------------------
// (g): HTTP error taxonomy — no panic, no index entry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_error_taxonomy_no_panic_no_index_entry() {
    let base = spawn_fixture_server().await;
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());
    let p = Pipeline::open_with(&cfg, true).unwrap();

    // A permitted page first, so the index exists.
    assert!(p
        .crawl_one(&format!("{base}/page"))
        .await
        .unwrap()
        .is_ingested());

    // 404 -> NotFound.
    let notfound_url = format!("{base}/404");
    let rid404 = p.resource_id_for(&notfound_url).unwrap();
    let err = p.crawl_one(&notfound_url).await.unwrap_err();
    assert!(
        matches!(err, AppError::NotFound(_)),
        "404 must map to AppError::NotFound, got {err:?}"
    );
    assert_eq!(dv_count(&cfg, &rid404), 0);
    assert!(stages_for(&cfg, &rid404).is_empty());

    // 500 -> Network (retryable server error), still no index entry, no panic.
    let err_url = format!("{base}/500");
    let rid500 = p.resource_id_for(&err_url).unwrap();
    let err = p.crawl_one(&err_url).await.unwrap_err();
    assert!(
        matches!(err, AppError::Network(_)),
        "500 must map to AppError::Network, got {err:?}"
    );
    assert_eq!(dv_count(&cfg, &rid500), 0);
    assert!(stages_for(&cfg, &rid500).is_empty());

    // The pipeline is still healthy afterwards.
    let resp = p.search("zephyrquokka", 10).await.unwrap();
    assert_eq!(resp.results.len(), 1);
}

// ---------------------------------------------------------------------------
// SSRF guard: blocked before any fetch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ssrf_guard_blocks_metadata_ip_before_fetch() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());
    let p = Pipeline::open_with(&cfg, true).unwrap();

    let err = p
        .crawl_one("http://169.254.169.254/latest/meta-data/")
        .await
        .unwrap_err();
    assert!(
        matches!(err, AppError::SsrfBlocked(_)),
        "cloud-metadata IP must be SSRF-blocked, got {err:?}"
    );

    // Nothing was recorded for the blocked resource.
    let rid = p
        .resource_id_for("http://169.254.169.254/latest/meta-data/")
        .unwrap();
    assert!(stages_for(&cfg, &rid).is_empty());
}
