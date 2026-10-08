//! Shared hermetic test fixtures for the Stage-8 acceptance suite.
//!
//! Everything here is local: a single axum fixture server bound to
//! `127.0.0.1:0` (no fixed port, no public internet) plus the small helpers
//! every test needs (temp data dirs, metadata reads, a PDF builder).
//!
//! The fixture is deliberately rich enough to drive every one of the twelve
//! acceptance tests: it serves `robots.txt` (with a `Sitemap:` line), a
//! sitemap, an RSS feed, a plain page, a robots-disallowed path, an article
//! with an `ETag` that honours `If-None-Match` with `304`, a real-text PDF, a
//! `/latest` page and a catch-all used by the fan-out burst test. It records
//! every request as `"<host><path>"` so tests can assert what was — and was
//! not — fetched.

#![allow(dead_code)]

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode, Uri},
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};

use search_core::config::Config;

// ---------------------------------------------------------------------------
// Fixture content
// ---------------------------------------------------------------------------

pub const ROBOTS: &str = "User-agent: *\nDisallow: /blocked\nAllow: /\nSitemap: /sitemap.xml\n";

pub const INDEX_HTML: &str = "<!doctype html><html><head><title>Stage Eight Index</title>\
<link rel=\"alternate\" type=\"application/rss+xml\" href=\"/feed.xml\"></head>\
<body><h1>Index</h1><p>indexword landing page.</p></body></html>";

pub const SITEMAP: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\
<url><loc>/article</loc><lastmod>2024-03-05</lastmod></url>\
<url><loc>/pdf/doc.pdf</loc></url>\
<url><loc>/feed.xml</loc></url>\
<url><loc>/latest</loc></url>\
</urlset>";

pub const FEED: &str = "<?xml version=\"1.0\"?>\
<rss version=\"2.0\"><channel><title>Fixture Feed</title><link>/</link>\
<item><title>Feed Fixture</title><link>/feed-entry</link>\
<guid>/feed-entry</guid><pubDate>Tue, 10 Jun 2025 09:00:00 GMT</pubDate></item>\
</channel></rss>";

/// Plain round-trip / restart / duplicate page.
pub const PAGE_HTML: &str = "<!doctype html><html><head><title>Stage Eight Page</title></head>\
<body><h1>Stage Eight Page</h1>\
<p>Welcome to the zephyrquokka observatory. A zephyrquokka sighting is rare.</p>\
</body></html>";

/// The article: carries `article:published_time` and an `ETag`.
pub const ARTICLE_HTML: &str = "<!doctype html><html><head><title>Stage Eight Article</title>\
<meta property=\"article:published_time\" content=\"2024-03-05T10:00:00Z\"></head>\
<body><h1>Stage Eight Article</h1><p>zephyrarticleword appears in this article.</p></body></html>";

/// The `ETag` the article route serves and revalidates against.
pub const ARTICLE_ETAG: &str = "\"article-v1\"";

/// `2024-03-05T10:00:00Z` as Unix seconds — the article's `published_at`.
pub const ARTICLE_PUBLISHED_AT: i64 = 1_709_632_800;

pub const BLOCKED_HTML: &str = "<html><head><title>Blocked</title></head>\
<body><p>zzzdeniedword</p></body></html>";

pub const FEED_ENTRY_HTML: &str = "<!doctype html><html><head><title>Feed Entry</title></head>\
<body><h1>Feed Entry</h1><p>zephyrfeedword appears in this entry.</p></body></html>";

/// Sentinel embedded in the fixture PDF's text object.
pub const PDF_MARKER: &str = "zephyrpdfmarker";

/// A page advertised by the sitemap but never crawled during discovery.
pub const LATEST_HTML: &str = "<!doctype html><html><head><title>Latest Fixture</title></head>\
<body><h1>Latest News</h1><p>zephyrlatestword appears only in the freshly fetched page.</p>\
</body></html>";

/// Catch-all page body used by the per-origin fan-out burst test.
pub const BURST_HTML: &str = "<!doctype html><html><head><title>Burst Fixture</title></head>\
<body><h1>Burst</h1><p>zephyrburstword generic page body.</p></body></html>";

/// Build a byte-exact single-page PDF with a real text layer, computing the
/// cross-reference offsets programmatically so the fixture is always valid.
pub fn build_pdf(text: &str) -> Vec<u8> {
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

// ---------------------------------------------------------------------------
// Fixture server
// ---------------------------------------------------------------------------

/// Shared fixture state: every request as `"<host><path>"` plus the count of
/// `304` responses served.
#[derive(Clone, Default)]
pub struct Fixture {
    hits: Arc<Mutex<Vec<String>>>,
    not_modified: Arc<AtomicUsize>,
}

impl Fixture {
    fn note(&self, host: &str, path: &str) {
        self.hits.lock().unwrap().push(format!("{host}{path}"));
    }
    /// Every request as `"<host><path>"`, in arrival order.
    pub fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }
    /// Just the request paths, in arrival order.
    pub fn paths(&self) -> Vec<String> {
        self.hits()
            .into_iter()
            .map(|h| match h.find('/') {
                Some(i) => h[i..].to_string(),
                None => h,
            })
            .collect()
    }
    /// Requests that began with the given origin (`host:port`).
    pub fn hits_for(&self, host: &str) -> Vec<String> {
        self.hits().into_iter().filter(|h| h.starts_with(host)).collect()
    }
    /// Number of `304 Not Modified` responses served.
    pub fn not_modified(&self) -> usize {
        self.not_modified.load(Ordering::SeqCst)
    }
}

fn host_of(headers: &HeaderMap) -> String {
    headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

async fn robots(State(fx): State<Fixture>, headers: HeaderMap, uri: Uri) -> Response {
    fx.note(&host_of(&headers), uri.path());
    ([(header::CONTENT_TYPE, "text/plain")], ROBOTS).into_response()
}

async fn index(State(fx): State<Fixture>, headers: HeaderMap, uri: Uri) -> Response {
    fx.note(&host_of(&headers), uri.path());
    Html(INDEX_HTML).into_response()
}

async fn sitemap(State(fx): State<Fixture>, headers: HeaderMap, uri: Uri) -> Response {
    fx.note(&host_of(&headers), uri.path());
    ([(header::CONTENT_TYPE, "application/xml")], SITEMAP).into_response()
}

async fn feed(State(fx): State<Fixture>, headers: HeaderMap, uri: Uri) -> Response {
    fx.note(&host_of(&headers), uri.path());
    ([(header::CONTENT_TYPE, "application/rss+xml")], FEED).into_response()
}

async fn page(State(fx): State<Fixture>, headers: HeaderMap, uri: Uri) -> Response {
    fx.note(&host_of(&headers), uri.path());
    Html(PAGE_HTML).into_response()
}

async fn blocked(State(fx): State<Fixture>, headers: HeaderMap, uri: Uri) -> Response {
    fx.note(&host_of(&headers), uri.path());
    Html(BLOCKED_HTML).into_response()
}

async fn feed_entry(State(fx): State<Fixture>, headers: HeaderMap, uri: Uri) -> Response {
    fx.note(&host_of(&headers), uri.path());
    Html(FEED_ENTRY_HTML).into_response()
}

async fn latest(State(fx): State<Fixture>, headers: HeaderMap, uri: Uri) -> Response {
    fx.note(&host_of(&headers), uri.path());
    ([(header::CONTENT_TYPE, "text/html")], LATEST_HTML).into_response()
}

async fn pdf_doc(State(fx): State<Fixture>, headers: HeaderMap, uri: Uri) -> Response {
    fx.note(&host_of(&headers), uri.path());
    (
        [(header::CONTENT_TYPE, "application/pdf")],
        build_pdf(PDF_MARKER),
    )
        .into_response()
}

/// The article: serves an `ETag` and answers a matching `If-None-Match` with
/// `304 Not Modified` (no body).
async fn article(State(fx): State<Fixture>, headers: HeaderMap, uri: Uri) -> Response {
    fx.note(&host_of(&headers), uri.path());
    let inm = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    if inm == Some(ARTICLE_ETAG) {
        fx.not_modified.fetch_add(1, Ordering::SeqCst);
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, ARTICLE_ETAG)]).into_response();
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

async fn catchall(State(fx): State<Fixture>, headers: HeaderMap, uri: Uri) -> Response {
    fx.note(&host_of(&headers), uri.path());
    ([(header::CONTENT_TYPE, "text/html")], BURST_HTML).into_response()
}

/// Start the fixture server on an ephemeral loopback port; return its base URL
/// and the shared fixture state.
pub async fn spawn_fixture() -> (String, Fixture) {
    let fx = Fixture::default();
    let app = Router::new()
        .route("/", get(index))
        .route("/robots.txt", get(robots))
        .route("/sitemap.xml", get(sitemap))
        .route("/feed.xml", get(feed))
        .route("/page", get(page))
        .route("/article", get(article))
        .route("/blocked", get(blocked))
        .route("/feed-entry", get(feed_entry))
        .route("/pdf/doc.pdf", get(pdf_doc))
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
// Data-dir / metadata helpers
// ---------------------------------------------------------------------------

pub fn test_config(data_dir: &Path) -> Config {
    let mut cfg = Config::default();
    cfg.data_dir = data_dir.to_path_buf();
    cfg.user_agent = "stage8-acceptance-test/0.1 (+test)".to_string();
    cfg.bind = "127.0.0.1:0".to_string();
    cfg
}

pub fn open_db(cfg: &Config) -> rusqlite::Connection {
    rusqlite::Connection::open(cfg.meta_db()).expect("open metadata db")
}

/// The number of `document_version` rows for a resource.
pub fn dv_count(cfg: &Config, resource_id: &str) -> i64 {
    open_db(cfg)
        .query_row(
            "SELECT COUNT(*) FROM document_version WHERE resource_id = ?1",
            rusqlite::params![resource_id],
            |r| r.get(0),
        )
        .expect("count document_version")
}

/// The ingest journal stages for a resource, in sequence order.
pub fn stages_for(cfg: &Config, resource_id: &str) -> Vec<String> {
    let conn = open_db(cfg);
    let mut stmt = conn
        .prepare("SELECT stage FROM ingest_event WHERE resource_id = ?1 ORDER BY seq")
        .expect("prepare");
    let rows = stmt
        .query_map(rusqlite::params![resource_id], |r| r.get::<_, String>(0))
        .expect("query");
    rows.map(|r| r.expect("stage")).collect()
}

/// Whether a search response carries a signal with the given name.
pub fn has_signal(resp: &search_core::SearchResponse, name: &str) -> bool {
    resp.results
        .iter()
        .any(|r| r.signals.iter().any(|s| s.name == name))
}
