//! Stage-3 exit-gate acceptance test: a hermetic, no-public-internet,
//! no-fixed-port end-to-end exercise of the wired Stage-3 freshness scheduler in
//! `crates/agent0`'s `Pipeline`.
//!
//! A local axum fixture serves `/robots.txt` (with a `Sitemap:` line),
//! `/sitemap.xml` (advertising `/latest`) and `/latest`, plus a catch-all page.
//! It records every request (`host + path`) in arrival order so the tests can
//! assert what was — and was not — fetched, and how the per-origin and total
//! fan-out caps held under a burst.
//!
//! Covered:
//! * a time-sensitive query spawns a P0 refresh whose newly fetched document is
//!   immediately searchable from the live overlay with overlay provenance;
//! * a plain query triggers zero refresh fetches;
//! * a scheduled burst never exceeds the configured per-origin or total fan-out.

use std::sync::{Arc, Mutex};

use axum::{
    extract::State,
    http::{header, HeaderMap, Uri},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};

use search_agent0::Pipeline;
use search_core::config::Config;
use search_core::manifest::Provenance;
use search_scheduler::{SchedulerConfig, SourceClass, SourceDue};

// ---------------------------------------------------------------------------
// Fixture content
// ---------------------------------------------------------------------------

const ROBOTS: &str = "User-agent: *\nAllow: /\nSitemap: /sitemap.xml\n";

const SITEMAP: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\
<url><loc>/latest</loc><lastmod>2026-02-01</lastmod></url>\
</urlset>";

/// A page that is advertised by the sitemap but never crawled during discovery.
const LATEST_HTML: &str = "<!doctype html><html><head><title>Latest Fixture</title></head>\
<body><h1>Latest News</h1><p>zephyrlatestword appears only in the freshly fetched page.</p>\
</body></html>";

/// Catch-all page used as the seed and by the burst test.
const BURST_HTML: &str = "<!doctype html><html><head><title>Burst Fixture</title></head>\
<body><h1>Burst</h1><p>zephyrburstword generic page body.</p></body></html>";

/// Shared fixture state: every request as `"<host><path>"`, in arrival order.
#[derive(Clone, Default)]
struct Fixture {
    hits: Arc<Mutex<Vec<String>>>,
}

impl Fixture {
    fn note(&self, host: &str, path: &str) {
        self.hits.lock().unwrap().push(format!("{host}{path}"));
    }
    fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }
    /// Requests for real resources (everything but `robots.txt`).
    fn resource_hits(&self) -> Vec<String> {
        self.hits()
            .into_iter()
            .filter(|h| !h.ends_with("/robots.txt"))
            .collect()
    }
    fn resource_hits_for(&self, host: &str) -> Vec<String> {
        self.resource_hits()
            .into_iter()
            .filter(|h| h.starts_with(host))
            .collect()
    }
}

fn host_of(headers: &HeaderMap) -> String {
    headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

async fn robots(State(fx): State<Fixture>, headers: HeaderMap) -> impl IntoResponse {
    fx.note(&host_of(&headers), "/robots.txt");
    ([(header::CONTENT_TYPE, "text/plain")], ROBOTS).into_response()
}

async fn sitemap(State(fx): State<Fixture>, headers: HeaderMap) -> impl IntoResponse {
    fx.note(&host_of(&headers), "/sitemap.xml");
    ([(header::CONTENT_TYPE, "application/xml")], SITEMAP).into_response()
}

async fn latest(State(fx): State<Fixture>, headers: HeaderMap) -> impl IntoResponse {
    fx.note(&host_of(&headers), "/latest");
    ([(header::CONTENT_TYPE, "text/html")], LATEST_HTML).into_response()
}

/// Catch-all: any other path returns a generic HTML page so the burst test can
/// fetch an arbitrary set of URLs.
async fn catchall(State(fx): State<Fixture>, headers: HeaderMap, uri: Uri) -> Response {
    fx.note(&host_of(&headers), uri.path());
    ([(header::CONTENT_TYPE, "text/html")], BURST_HTML).into_response()
}

/// Start the fixture server on an ephemeral loopback port; return its base URL
/// and the shared fixture state.
async fn spawn_fixture() -> (String, Fixture) {
    let fx = Fixture::default();
    let app = Router::new()
        .route("/robots.txt", get(robots))
        .route("/sitemap.xml", get(sitemap))
        .route("/latest", get(latest))
        .fallback(catchall)
        .with_state(fx.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), fx)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn test_config(data_dir: &std::path::Path) -> Config {
    let mut cfg = Config::default();
    cfg.data_dir = data_dir.to_path_buf();
    cfg.user_agent = "agent0-stage3-test/0.1 (+test)".to_string();
    cfg.bind = "127.0.0.1:0".to_string();
    cfg
}

async fn open_pipeline(cfg: &Config, scheduler: SchedulerConfig) -> Pipeline {
    Pipeline::open_with_scheduler(cfg, true, scheduler, 10_000).expect("open pipeline")
}

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

// ---------------------------------------------------------------------------
// (1) A time-sensitive query surfaces a newly fetched document from the overlay
// ---------------------------------------------------------------------------

#[tokio::test]
async fn latest_query_surfaces_a_newly_fetched_doc() {
    let (base, fx) = spawn_fixture().await;
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());
    let p = open_pipeline(&cfg, SchedulerConfig::default()).await;

    // Discovery learns /latest from the sitemap and queues it, but does NOT
    // crawl the page itself.
    let report = p.discover(&format!("{base}/")).await.unwrap();
    assert!(report.resources >= 1, "the sitemap advertised a page: {report:?}");
    assert!(
        !fx.resource_hits().iter().any(|h| h.ends_with("/latest")),
        "discovery must not crawl the page: {:?}",
        fx.resource_hits()
    );

    let now = search_core::time::now_unix_s();

    // The document is NOT in the index before the refresh.
    let before = p.search("zephyrlatestword", 10).await.unwrap();
    assert!(
        before.results.is_empty(),
        "the doc must not be in the index before refresh: {:?}",
        before.results
    );

    // A time-sensitive query ("latest ...") triggers a P0 query refresh.
    let refresh = p
        .query_refresh("latest zephyrlatestword", now)
        .await
        .unwrap();
    assert_eq!(refresh.admitted, 1, "one P0 activation: {refresh:?}");
    assert_eq!(refresh.new_documents, 1, "a new document was fetched: {refresh:?}");
    assert_eq!(refresh.fetched, 1);

    // The P0 refresh fetched /latest from the fixture.
    assert!(
        fx.resource_hits().iter().any(|h| h.ends_with("/latest")),
        "the P0 refresh must fetch /latest: {:?}",
        fx.resource_hits()
    );

    // And the newly fetched doc is immediately searchable, from the live overlay.
    let resp = p.search("zephyrlatestword", 10).await.unwrap();
    assert_eq!(resp.results.len(), 1, "freshly fetched doc is searchable");
    let hit = &resp.results[0];
    assert!(
        hit.canonical_url.ends_with("/latest"),
        "url round-trips: {}",
        hit.canonical_url
    );
    assert_eq!(
        hit.provenance,
        Provenance::Overlay,
        "must come from the live overlay before any global reindex"
    );
    assert!(hit.score > 0.0, "scored against the active query");
}

// ---------------------------------------------------------------------------
// (2) A non-time-sensitive query triggers no refresh work at all
// ---------------------------------------------------------------------------

#[tokio::test]
async fn non_time_sensitive_query_triggers_no_refresh() {
    let (base, fx) = spawn_fixture().await;
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());
    let p = open_pipeline(&cfg, SchedulerConfig::default()).await;

    let report = p.discover(&format!("{base}/")).await.unwrap();
    assert!(report.resources >= 1);

    let now = search_core::time::now_unix_s();
    assert!(
        !search_scheduler::is_time_sensitive("zephyrlatestword", now),
        "the plain query must not be time-sensitive"
    );

    let before = fx.resource_hits().len();

    // Direct query refresh does nothing.
    let refresh = p.query_refresh("zephyrlatestword", now).await.unwrap();
    assert_eq!(refresh.admitted, 0);
    assert!(refresh.activations.is_empty());

    // ... and neither does the search-with-refresh entry point.
    let resp = p.search_with_refresh("zephyrlatestword", 10, now).await.unwrap();
    assert!(resp.results.is_empty(), "nothing was fetched or indexed");

    assert_eq!(
        fx.resource_hits().len(),
        before,
        "a plain query issues zero refresh fetches: {:?}",
        fx.resource_hits()
    );
}

// ---------------------------------------------------------------------------
// (3) A burst never exceeds the configured per-origin or total fan-out
// ---------------------------------------------------------------------------

#[tokio::test]
async fn burst_respects_per_origin_and_total_fanout() {
    let (base, fx) = spawn_fixture().await;

    // Two distinct origins that resolve to the same fixture server, so the
    // fixture can record genuinely per-origin request counts.
    let parsed = url::Url::parse(&base).unwrap();
    let port = parsed.port().unwrap();
    let base_host = format!("127.0.0.1:{port}");
    let alt = format!("http://localhost:{port}");
    let alt_host = format!("localhost:{port}");

    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());

    let scheduler_cfg = SchedulerConfig {
        total_live_fanout: 4,
        per_origin_fanout: 2,
        query_refresh_limit: 8,
        ..Default::default()
    };
    let p = open_pipeline(&cfg, scheduler_cfg.clone()).await;

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

    // Total dispatch never exceeds the total fan-out cap.
    assert!(
        report.admitted <= scheduler_cfg.total_live_fanout,
        "admitted {} > total cap {}",
        report.admitted,
        scheduler_cfg.total_live_fanout
    );

    let res = fx.resource_hits();
    let a = fx.resource_hits_for(&base_host);
    let b = fx.resource_hits_for(&alt_host);

    // Observed at the server: total fetches bounded by the total cap.
    assert!(
        res.len() <= scheduler_cfg.total_live_fanout,
        "server saw {} fetches > total cap {}",
        res.len(),
        scheduler_cfg.total_live_fanout
    );
    // ... and each origin bounded by the per-origin cap.
    assert!(
        a.len() <= scheduler_cfg.per_origin_fanout,
        "origin {base_host} saw {} > per-origin cap {}",
        a.len(),
        scheduler_cfg.per_origin_fanout
    );
    assert!(
        b.len() <= scheduler_cfg.per_origin_fanout,
        "origin {alt_host} saw {} > per-origin cap {}",
        b.len(),
        scheduler_cfg.per_origin_fanout
    );

    // Enough work was queued that both caps actually bound the burst, and the
    // first origin's slots fill before the second origin's (ordering recorded).
    assert_eq!(a.len(), scheduler_cfg.per_origin_fanout, "first origin fills its cap");
    assert_eq!(b.len(), scheduler_cfg.per_origin_fanout, "second origin fills its cap");
    assert_eq!(res.len(), scheduler_cfg.total_live_fanout);
    assert!(res[0].starts_with(&base_host) && res[1].starts_with(&base_host));
    assert!(res[2].starts_with(&alt_host) && res[3].starts_with(&alt_host));

    // The dispatch ledger was charged once per dispatched activation (no refunds
    // on success), never more than the budget.
    assert_eq!(p.budget_spent(), report.fetched as u64);
}
