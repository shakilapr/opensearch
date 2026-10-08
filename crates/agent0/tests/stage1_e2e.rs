//! Stage-1 exit-gate acceptance test: a hermetic, no-public-internet,
//! no-fixed-port end-to-end exercise of the wired Stage-1 components in
//! `crates/agent0`'s `Pipeline`.
//!
//! A local axum fixture serves `/robots.txt` (with a `Sitemap:` line),
//! `/sitemap.xml` (an article + a PDF + a feed), `/feed.xml`, the article (with
//! an `ETag`, honouring `If-None-Match` with a `304`), the feed entry and the
//! PDF. It records every requested path and the number of `304` responses so
//! the test can assert discovery did **not** fetch every page and that
//! conditional refresh works.
//!
//! Covered: discovery without a full crawl; PDF routing + text indexing; feed
//! entry ingest; `SourceProfile` persistence; freshness columns; the
//! `source_first` signal in `/search`; a `304` revalidation as `Unchanged`;
//! restart safety; and no-duplicate re-ingest.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};

use search_agent0::{IngestOutcome, Pipeline};
use search_core::config::Config;

// ---------------------------------------------------------------------------
// Fixture content
// ---------------------------------------------------------------------------

const ROBOTS: &str = "User-agent: *\nDisallow: /blocked\nSitemap: /sitemap.xml\n";

const INDEX_HTML: &str = "<!doctype html><html><head><title>Stage One Index</title>\
<link rel=\"alternate\" type=\"application/rss+xml\" href=\"/feed.xml\"></head>\
<body><h1>Index</h1><p>indexword landing page.</p></body></html>";

const SITEMAP: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\
<url><loc>/article</loc><lastmod>2024-03-05</lastmod></url>\
<url><loc>/pdf/doc.pdf</loc></url>\
<url><loc>/feed.xml</loc></url>\
</urlset>";

const FEED: &str = "<?xml version=\"1.0\"?>\
<rss version=\"2.0\"><channel><title>Fixture Feed</title><link>/</link>\
<item><title>Feed Fixture</title><link>/feed-entry</link>\
<guid>/feed-entry</guid><pubDate>Tue, 10 Jun 2025 09:00:00 GMT</pubDate></item>\
</channel></rss>";

const ARTICLE_HTML: &str = "<!doctype html><html><head><title>Stage One Article</title>\
<meta property=\"article:published_time\" content=\"2024-03-05T10:00:00Z\"></head>\
<body><h1>Stage One Article</h1><p>zephyrarticleword appears in this article.</p></body></html>";

/// The `ETag` the article route serves, and revalidates against.
const ARTICLE_ETAG: &str = "\"article-v1\"";

const FEED_ENTRY_HTML: &str = "<!doctype html><html><head><title>Feed Entry</title></head>\
<body><h1>Feed Entry</h1><p>zephyrfeedword appears in this entry.</p></body></html>";

/// Sentinel embedded in the fixture PDF's text object.
const PDF_MARKER: &str = "zephyrpdfmarker";

/// Build a byte-exact single-page PDF with a real text layer, computing the
/// cross-reference offsets programmatically so the fixture is always valid.
fn build_pdf(text: &str) -> Vec<u8> {
    let content = format!("BT /F1 24 Tf 72 700 Td ({text}) Tj ET");
    let objects: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources \
         << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>"
            .to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_string(),
        format!("<< /Length {} >>\nstream\n{}\nendstream", content.len(), content),
    ];

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n");
    let mut offsets: Vec<usize> = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body.as_bytes());
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_pos = out.len();
    let n = objects.len() + 1;
    out.extend_from_slice(format!("xref\n0 {n}\n").as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {n} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n").as_bytes(),
    );
    out
}

/// Shared fixture state: requested paths + count of `304` responses served.
#[derive(Clone, Default)]
struct Fixture {
    paths: Arc<Mutex<Vec<String>>>,
    not_modified: Arc<AtomicUsize>,
}

impl Fixture {
    fn note(&self, path: &str) {
        self.paths.lock().unwrap().push(path.to_string());
    }
    fn paths(&self) -> Vec<String> {
        self.paths.lock().unwrap().clone()
    }
    fn not_modified(&self) -> usize {
        self.not_modified.load(Ordering::SeqCst)
    }
}

async fn robots(State(fx): State<Fixture>) -> impl IntoResponse {
    fx.note("/robots.txt");
    ([(header::CONTENT_TYPE, "text/plain")], ROBOTS).into_response()
}

async fn index(State(fx): State<Fixture>) -> impl IntoResponse {
    fx.note("/");
    Html(INDEX_HTML).into_response()
}

async fn sitemap(State(fx): State<Fixture>) -> impl IntoResponse {
    fx.note("/sitemap.xml");
    ([(header::CONTENT_TYPE, "application/xml")], SITEMAP).into_response()
}

async fn feed(State(fx): State<Fixture>) -> impl IntoResponse {
    fx.note("/feed.xml");
    ([(header::CONTENT_TYPE, "application/rss+xml")], FEED).into_response()
}

/// The article: serves an `ETag` and answers a matching `If-None-Match` with
/// `304 Not Modified` (no body).
async fn article(State(fx): State<Fixture>, headers: HeaderMap) -> Response {
    fx.note("/article");
    let inm = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    if inm == Some(ARTICLE_ETAG) {
        fx.not_modified.fetch_add(1, Ordering::SeqCst);
        return (
            StatusCode::NOT_MODIFIED,
            [(header::ETAG, ARTICLE_ETAG)],
        )
            .into_response();
    }
    (
        [
            (header::CONTENT_TYPE, "text/html"),
            (header::ETAG, ARTICLE_ETAG),
        ],
        ARTICLE_HTML,
    )
        .into_response()
}

async fn feed_entry(State(fx): State<Fixture>) -> impl IntoResponse {
    fx.note("/feed-entry");
    Html(FEED_ENTRY_HTML).into_response()
}

async fn pdf_doc(State(fx): State<Fixture>) -> impl IntoResponse {
    fx.note("/pdf");
    (
        [(header::CONTENT_TYPE, "application/pdf")],
        build_pdf(PDF_MARKER),
    )
        .into_response()
}

/// Start the fixture server on an ephemeral loopback port; return its base URL
/// and the shared fixture state.
async fn spawn_fixture() -> (String, Fixture) {
    let fx = Fixture::default();
    let app = Router::new()
        .route("/", get(index))
        .route("/robots.txt", get(robots))
        .route("/sitemap.xml", get(sitemap))
        .route("/feed.xml", get(feed))
        .route("/article", get(article))
        .route("/feed-entry", get(feed_entry))
        .route("/pdf/doc.pdf", get(pdf_doc))
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
    cfg.user_agent = "agent0-stage1-test/0.1 (+test)".to_string();
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

fn host_of(base: &str) -> String {
    url::Url::parse(base).unwrap().host_str().unwrap().to_string()
}

fn has_signal(resp: &search_core::SearchResponse, name: &str) -> bool {
    resp.results
        .iter()
        .any(|r| r.signals.iter().any(|s| s.name == name))
}

// ---------------------------------------------------------------------------
// Discovery -> routing -> profile -> freshness -> source-first
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stage1_discovery_routes_pdf_feed_profile_and_freshness() {
    let (base, fx) = spawn_fixture().await;
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());
    let p = Pipeline::open_with(&cfg, true).unwrap();

    // (1) Discovery: robots -> sitemap -> feeds, bounded, no page crawl.
    let report = p.discover(&format!("{base}/")).await.unwrap();
    assert_eq!(report.seed_url, format!("{base}/"));
    assert!(report.sitemaps >= 1, "sitemap persisted: {report:?}");
    assert_eq!(report.feeds, 1, "one feed discovered: {report:?}");
    assert!(
        report.resources >= 3,
        "article + pdf + feed entry pushed: {report:?}"
    );

    let requested = fx.paths();
    for expected in ["/", "/robots.txt", "/sitemap.xml", "/feed.xml"] {
        assert!(
            requested.iter().any(|r| r == expected),
            "discovery should fetch {expected}: {requested:?}"
        );
    }
    for not_expected in ["/article", "/pdf", "/feed-entry"] {
        assert!(
            !requested.iter().any(|r| r == not_expected),
            "discovery must NOT fetch every page ({not_expected}): {requested:?}"
        );
    }

    // Sitemap + feed rows persisted through the Mutation API.
    let db = open_db(&cfg);
    let n_sitemap: i64 = db
        .query_row("SELECT COUNT(*) FROM sitemap", [], |r| r.get(0))
        .unwrap();
    let n_feed: i64 = db
        .query_row("SELECT COUNT(*) FROM feed", [], |r| r.get(0))
        .unwrap();
    assert!(n_sitemap >= 1, "a sitemap row is persisted");
    assert_eq!(n_feed, 1, "a feed row is persisted");

    // (2) PDF routed: its text is indexed.
    let pdf_url = format!("{base}/pdf/doc.pdf");
    let out = p.crawl_one(&pdf_url).await.unwrap();
    assert!(out.is_ingested(), "pdf must be ingested, got {out:?}");
    let resp = p.search(PDF_MARKER, 10).await.unwrap();
    assert_eq!(resp.results.len(), 1, "pdf text is searchable");
    assert_eq!(resp.results[0].canonical_url, pdf_url);

    // Crawl the article too (so freshness + source-first have a doc).
    let article_url = format!("{base}/article");
    assert!(p.crawl_one(&article_url).await.unwrap().is_ingested());

    // (3) Feed routed directly through crawl_one: entries pushed, no docversion.
    let feed_url = format!("{base}/feed.xml");
    let out = p.crawl_one(&feed_url).await.unwrap();
    assert_eq!(out.route(), Some("feed"), "feed is routed, got {out:?}");
    match &out {
        IngestOutcome::Routed { discovered, .. } => {
            assert!(*discovered >= 1, "feed entry discovered");
        }
        other => panic!("expected Routed, got {other:?}"),
    }

    // The feed entry ingests as a page.
    let entry_url = format!("{base}/feed-entry");
    assert!(p.crawl_one(&entry_url).await.unwrap().is_ingested());
    let resp = p.search("zephyrfeedword", 10).await.unwrap();
    assert_eq!(resp.results.len(), 1, "feed entry is searchable");

    // (4) SourceProfile row written with observed fetch stats.
    let host = host_of(&base);
    let (fetch_count, error_count, change_rate) = open_db(&cfg)
        .query_row(
            "SELECT fetch_count, error_count, change_rate FROM source_profile WHERE source_id = ?1",
            rusqlite::params![host],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, f64>(2)?)),
        )
        .expect("a source_profile row exists for the domain");
    assert!(fetch_count >= 3, "fetch_count observed: {fetch_count}");
    assert_eq!(error_count, 0, "no fetch errors");
    assert!(change_rate > 0.0, "change rate observed: {change_rate}");

    // (5) Freshness columns populated on the article resource.
    let article_rid = p.resource_id_for(&article_url).unwrap();
    let (published, modified, fetched, verified) = open_db(&cfg)
        .query_row(
            "SELECT published_at, modified_at, fetched_at, verified_at FROM resource \
             WHERE resource_id = ?1",
            rusqlite::params![article_rid],
            |r| {
                Ok((
                    r.get::<_, Option<i64>>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .expect("article resource row exists");
    assert_eq!(
        published,
        Some(1_709_632_800),
        "published_at from the HTML meta"
    );
    assert!(modified.is_none(), "no modified timestamp in the fixture");
    assert!(fetched.is_some(), "fetched_at populated");
    assert!(verified.is_some(), "verified_at populated");

    // Feed-sourced entry carries a feed_watermark.
    let entry_rid = p.resource_id_for(&entry_url).unwrap();
    let fw: Option<i64> = open_db(&cfg)
        .query_row(
            "SELECT feed_watermark FROM resource WHERE resource_id = ?1",
            rusqlite::params![entry_rid],
            |r| r.get(0),
        )
        .unwrap();
    assert!(fw.is_some(), "feed-sourced entry has a feed_watermark");

    // (6) /search carries a source_first provenance signal.
    let resp = p.search("zephyrarticleword", 10).await.unwrap();
    assert_eq!(resp.results.len(), 1);
    assert!(
        has_signal(&resp, "source_first"),
        "expected a source_first signal, got {:?}",
        resp.results[0].signals
    );
}

// ---------------------------------------------------------------------------
// Conditional refresh (304 -> Unchanged), restart safety, no-duplicate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stage1_restart_safe_conditional_304_and_no_duplicate_reingest() {
    let (base, fx) = spawn_fixture().await;
    let url = format!("{base}/article");
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());

    let rid = {
        let p = Pipeline::open_with(&cfg, true).unwrap();
        let rid = p.resource_id_for(&url).unwrap();
        assert!(p.crawl_one(&url).await.unwrap().is_ingested());

        let resp = p.search("zephyrarticleword", 10).await.unwrap();
        assert_eq!(resp.results.len(), 1);
        assert!(has_signal(&resp, "source_first"));
        assert_eq!(dv_count(&cfg, &rid), 1);
        rid
    };

    // RESTART: reopen on the same data dir; the document is still searchable.
    {
        let p = Pipeline::open_with(&cfg, true).unwrap();
        let resp = p.search("zephyrarticleword", 10).await.unwrap();
        assert_eq!(resp.results.len(), 1, "result survives restart");
        assert_eq!(resp.results[0].canonical_url, url);

        // RE-INGEST identical bytes -> Unchanged, still exactly one version.
        let out = p.crawl_one(&url).await.unwrap();
        assert!(out.is_unchanged(), "re-ingest must be Unchanged, got {out:?}");

        // A second re-fetch sends the stored ETag -> the fixture answers 304,
        // which is Unchanged without re-parsing or re-indexing.
        let before = fx.not_modified();
        let out = p.crawl_one(&url).await.unwrap();
        assert!(out.is_unchanged(), "304 must be Unchanged, got {out:?}");
        assert!(
            fx.not_modified() > before,
            "the fixture should have served a 304 on conditional refresh"
        );

        assert_eq!(
            dv_count(&cfg, &rid),
            1,
            "no second DocumentVersion for unchanged content"
        );
    }
}
