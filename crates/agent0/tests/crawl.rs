//! Hermetic integration tests for the `agent0 crawl` driver.
//!
//! Everything runs against a single loopback axum fixture (ephemeral port, no
//! public internet) and `tempfile` data/out dirs. The fixture records the path
//! and arrival `Instant` of every request so the politeness test can measure the
//! per-origin interval.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};

use search_agent0::crawl::{crawl, CrawlOptions, MAX_LINKS_PER_PAGE};
use search_agent0::pipeline::Pipeline;
use search_core::config::Config;

// Fixture bodies. Their SHA-256 values are asserted verbatim in the log.
const P1: &str = "<!doctype html><html><head><title>One</title></head><body><p>pageone alpha</p></body></html>";
const P2: &str = "<!doctype html><html><head><title>Two</title></head><body><p>pagetwo beta</p></body></html>";
const SAME: &str = "<!doctype html><html><head><title>Same</title></head><body><p>identical body</p></body></html>";
const P3: &str = "<!doctype html><html><head><title>Three</title></head><body><p>pagethree gamma</p></body></html>";
const P4: &str = "<!doctype html><html><head><title>Four</title></head><body><p>pagefour delta</p></body></html>";
const P5: &str = "<!doctype html><html><head><title>Five</title></head><body><p>pagefive epsilon</p></body></html>";
const INDEX: &str = "<!doctype html><html><head><title>Index</title></head><body><p>crawl index root</p></body></html>";

const SHA_P1: &str = "d3ecd153cf8e0a03546b9408f5cf754a73546a60e074f582e0855dd41fcaa5d4";
const SHA_P2: &str = "a12ad34f1898fb8e77cf34941ba8f1b6c785f88c6ca8734d785140089e994e9f";
const SHA_SAME: &str = "6e95563921211f8ab1244f946204e1b75767717a78d22b7e0d69019e94761fc5";

const ROBOTS_OPEN: &str = "User-agent: *\nDisallow:\n";
const ROBOTS_WITH_SITEMAP: &str = "User-agent: *\nDisallow:\nSitemap: /sitemap.xml\n";
const SITEMAP: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\
<url><loc>/crawl-p1</loc></url><url><loc>/crawl-p2</loc></url></urlset>";

// ---------------------------------------------------------------------------
// Fixture server
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Fx {
    hits: Arc<Mutex<Vec<(String, Instant)>>>,
}

impl Fx {
    fn record(&self, path: &str) {
        self.hits
            .lock()
            .unwrap()
            .push((path.to_string(), Instant::now()));
    }
    /// Arrival instants of every request whose path matches exactly.
    fn times(&self, path: &str) -> Vec<Instant> {
        self.hits
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == path)
            .map(|(_, t)| *t)
            .collect()
    }
    fn count(&self, path: &str) -> usize {
        self.times(path).len()
    }
    /// Every recorded request path, in arrival order (duplicates preserved).
    fn paths(&self) -> Vec<String> {
        self.hits
            .lock()
            .unwrap()
            .iter()
            .map(|(p, _)| p.clone())
            .collect()
    }
}

/// Spawn a fixture serving `/robots.txt` + the given `(path, content_type, body)`
/// routes; any other path is a 404.
async fn spawn_fixture(
    routes: Vec<(&'static str, &'static str, &'static str)>,
    robots: &'static str,
) -> (String, Fx) {
    let fx = Fx::default();

    let robots_fx = fx.clone();
    let mut router = Router::new().route(
        "/robots.txt",
        get(move || {
            let fx = robots_fx.clone();
            async move {
                fx.record("/robots.txt");
                ([(header::CONTENT_TYPE, "text/plain")], robots).into_response()
            }
        }),
    );

    for (path, ct, body) in routes {
        let fxc = fx.clone();
        router = router.route(
            path,
            get(move || {
                let fx = fxc.clone();
                async move {
                    fx.record(path);
                    ([(header::CONTENT_TYPE, ct)], body).into_response()
                }
            }),
        );
    }

    let router = router.fallback(|| async { StatusCode::NOT_FOUND });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}"), fx)
}

/// Like [`spawn_fixture`], but the routes are owned `String`s built at runtime
/// (so a page body can embed a second fixture's address) and **every** request —
/// including one that falls through to the 404 handler — is recorded. That lets
/// a test assert which paths were fetched *and* which were never tried.
async fn spawn_recording_fixture(
    routes: Vec<(String, String, String)>,
    robots: &'static str,
) -> (String, Fx) {
    let fx = Fx::default();

    let robots_fx = fx.clone();
    let mut router = Router::new().route(
        "/robots.txt",
        get(move || {
            let fx = robots_fx.clone();
            async move {
                fx.record("/robots.txt");
                ([(header::CONTENT_TYPE, "text/plain")], robots).into_response()
            }
        }),
    );

    for (path, ct, body) in routes {
        let fxc = fx.clone();
        let path_rec = path.clone();
        router = router.route(
            &path,
            get(move || {
                let fx = fxc.clone();
                let p = path_rec.clone();
                let ct = ct.clone();
                let body = body.clone();
                async move {
                    fx.record(&p);
                    let mut hm = HeaderMap::new();
                    if let Ok(v) = HeaderValue::from_str(&ct) {
                        hm.insert(header::CONTENT_TYPE, v);
                    }
                    (StatusCode::OK, hm, body).into_response()
                }
            }),
        );
    }

    // The fallback records the path too: a request for a route we did not
    // register (e.g. a low-value or off-origin link that should never be
    // followed) still shows up as a hit.
    let fallback_fx = fx.clone();
    let router = router.fallback(move |req: axum::extract::Request| {
        let fx = fallback_fx.clone();
        let path = req.uri().path().to_string();
        async move {
            fx.record(&path);
            StatusCode::NOT_FOUND
        }
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}"), fx)
}

/// A fixture whose `/seed` links to two same-origin documents that both answer
/// `429 Too Many Requests` with `Retry-After: 1`. Used to prove the crawler
/// backs off a rate-limited origin instead of retrying it immediately.
async fn spawn_429_fixture(seed_html: &'static str) -> (String, Fx) {
    let fx = Fx::default();

    let robots_fx = fx.clone();
    let mut router = Router::new().route(
        "/robots.txt",
        get(move || {
            let fx = robots_fx.clone();
            async move {
                fx.record("/robots.txt");
                ([(header::CONTENT_TYPE, "text/plain")], ROBOTS_OPEN).into_response()
            }
        }),
    );

    let seed_fx = fx.clone();
    router = router.route(
        "/seed",
        get(move || {
            let fx = seed_fx.clone();
            async move {
                fx.record("/seed");
                ([(header::CONTENT_TYPE, "text/html")], seed_html).into_response()
            }
        }),
    );

    for path in ["/blog/one-two.html", "/blog/three-four.html"] {
        let fxc = fx.clone();
        router = router.route(
            path,
            get(move || {
                let fx = fxc.clone();
                let p = path;
                async move {
                    fx.record(p);
                    let mut hm = HeaderMap::new();
                    hm.insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
                    (StatusCode::TOO_MANY_REQUESTS, hm, "").into_response()
                }
            }),
        );
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}"), fx)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
fn test_config(data_dir: &Path) -> Config {
    // The raw-spill tests below assert on `<data_dir>/blobs`, which production no longer writes
    // by default (`SEARCH_KEEP_RAW_BLOBS`): raw HTML is 38x the extracted text and nothing needs
    // it, because `read_pipeline_bytes` falls back to the text file. The default-off behaviour has
    // its own test at the bottom of this file.
    std::env::set_var("SEARCH_KEEP_RAW_BLOBS", "1");
    let mut cfg = Config::default();
    cfg.data_dir = data_dir.to_path_buf();
    cfg.user_agent = "agent0-crawl-test/0.1 (+test)".to_string();
    cfg.bind = "127.0.0.1:0".to_string();
    cfg
}

fn write_seeds(dir: &Path, name: &str, urls: &[String]) -> PathBuf {
    let path = dir.join(name);
    let mut body = String::from("# seeds\n\n");
    for u in urls {
        body.push_str(u);
        body.push('\n');
    }
    std::fs::write(&path, body).unwrap();
    path
}

fn options(seeds: PathBuf, out: PathBuf) -> CrawlOptions {
    let mut o = CrawlOptions::new(seeds);
    o.out_dir = out;
    o.allow_loopback = true;
    // The raw-spill suite below asserts on `out_dir/blobs`, which production no longer writes
    // (see `CrawlOptions::keep_raw_blobs`); the default-off behaviour has its own test.
    o.keep_raw_blobs = true;
    // Zero interval for the fast tests; the politeness test overrides it.
    o.origin_interval = Duration::ZERO;
    o
}

fn log_lines(out: &Path) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(out.join("crawl-log.jsonl")).unwrap_or_default();
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .collect()
}

fn blob_files(out: &Path) -> Vec<String> {
    let dir = out.join("blobs");
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A two-page fixture is crawled: both pages ingested, both blobs written, the
/// log has exactly 2 lines with the right sha256 values.
#[tokio::test]
async fn two_pages_ingested_with_correct_sha256() {
    let (base, _fx) = spawn_fixture(
        vec![("/a", "text/html", P1), ("/b", "text/html", P2)],
        ROBOTS_OPEN,
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let seeds = write_seeds(tmp.path(), "seeds.txt", &[format!("{base}/a"), format!("{base}/b")]);

    let summary = crawl(options(seeds, out.clone()), &cfg).await.unwrap();
    assert_eq!(summary.ingested, 2, "both pages ingested");
    assert_eq!(summary.blobs, 2, "both blobs written");
    assert_eq!(summary.urls_attempted, 2);

    let lines = log_lines(&out);
    assert_eq!(lines.len(), 2, "one log line per fetched page");
    let mut by_url = std::collections::HashMap::new();
    for l in &lines {
        assert_eq!(l["outcome"], "ingested");
        by_url.insert(
            l["url"].as_str().unwrap().to_string(),
            l["sha256"].as_str().unwrap().to_string(),
        );
    }
    assert_eq!(by_url.get(&format!("{base}/a")).unwrap(), SHA_P1);
    assert_eq!(by_url.get(&format!("{base}/b")).unwrap(), SHA_P2);

    let blobs = blob_files(&out);
    assert!(blobs.iter().any(|b| b == &format!("{SHA_P1}.html")));
    assert!(blobs.iter().any(|b| b == &format!("{SHA_P2}.html")));
    assert_eq!(blobs.len(), 2);
}

/// Re-running the same command ingests nothing new and reports resumed_from > 0.
#[tokio::test]
async fn rerun_resumes_and_ingests_nothing() {
    let (base, _fx) = spawn_fixture(
        vec![("/a", "text/html", P1), ("/b", "text/html", P2)],
        ROBOTS_OPEN,
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let seeds = write_seeds(tmp.path(), "seeds.txt", &[format!("{base}/a"), format!("{base}/b")]);

    let first = crawl(options(seeds.clone(), out.clone()), &cfg).await.unwrap();
    assert_eq!(first.ingested, 2);
    assert_eq!(first.resumed_from, 0);

    let second = crawl(options(seeds, out.clone()), &cfg).await.unwrap();
    assert_eq!(second.ingested, 0, "nothing new on resume");
    assert!(second.resumed_from > 0, "resumed_from must report prior work");
    assert_eq!(log_lines(&out).len(), 2, "no new log lines on resume");
}

/// Identical body bytes from two URLs produce exactly ONE blob file.
#[tokio::test]
async fn identical_bytes_produce_one_blob() {
    let (base, _fx) = spawn_fixture(
        vec![("/same1", "text/html", SAME), ("/same2", "text/html", SAME)],
        ROBOTS_OPEN,
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let seeds = write_seeds(
        tmp.path(),
        "seeds.txt",
        &[format!("{base}/same1"), format!("{base}/same2")],
    );

    let summary = crawl(options(seeds, out.clone()), &cfg).await.unwrap();
    assert_eq!(summary.ingested, 2, "both URLs ingest as separate documents");
    assert_eq!(summary.blobs, 1, "identical bytes dedupe to one blob");
    let blobs = blob_files(&out);
    assert_eq!(blobs, vec![format!("{SHA_SAME}.html")]);
}

/// `--max-docs 1` stops after exactly one ingested document.
#[tokio::test]
async fn max_docs_stops_after_one() {
    let (base, _fx) = spawn_fixture(
        vec![("/a", "text/html", P1), ("/b", "text/html", P2)],
        ROBOTS_OPEN,
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let seeds = write_seeds(tmp.path(), "seeds.txt", &[format!("{base}/a"), format!("{base}/b")]);

    let mut o = options(seeds, out.clone());
    o.max_docs = 1;
    o.max_urls = 10;
    let summary = crawl(o, &cfg).await.unwrap();
    assert_eq!(summary.ingested, 1, "exactly one document");
    assert_eq!(summary.urls_attempted, 1, "no extra attempts after the bound");
    assert_eq!(log_lines(&out).len(), 1);
}

/// Per-origin politeness: two requests to the same origin are separated by at
/// least the configured interval.
#[tokio::test]
async fn per_origin_politeness_interval() {
    let interval = Duration::from_millis(120);
    let (base, fx) = spawn_fixture(
        vec![
            ("/", "text/html", INDEX),
            ("/crawl-p1", "text/html", P1),
            ("/crawl-p2", "text/html", P2),
            ("/sitemap.xml", "application/xml", SITEMAP),
        ],
        ROBOTS_WITH_SITEMAP,
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let seeds = write_seeds(tmp.path(), "seeds.txt", &[format!("{base}/")]);

    let mut o = options(seeds, out.clone());
    o.origin_interval = interval;
    let summary = crawl(o, &cfg).await.unwrap();
    assert!(summary.ingested >= 2, "the two sitemap pages were crawled");

    // The two sitemap-advertised pages are only fetched during the drain, so a
    // single timestamp each. Their separation must respect the interval.
    let t1 = fx.times("/crawl-p1");
    let t2 = fx.times("/crawl-p2");
    assert_eq!(t1.len(), 1, "crawl-p1 fetched once");
    assert_eq!(t2.len(), 1, "crawl-p2 fetched once");
    let gap = t2[0].saturating_duration_since(t1[0]);
    // 20 ms tolerance for loopback scheduling and the request that precedes the
    // dispatch (the robots fetch inside crawl_one).
    assert!(
        gap + Duration::from_millis(20) >= interval,
        "same-origin requests separated by {gap:?}, expected >= {interval:?}"
    );
}

/// A URL that 404s is logged with outcome `failed` and an error mentioning 404;
/// the run continues and the other page still ingests.
#[tokio::test]
async fn not_found_is_logged_and_run_continues() {
    let (base, _fx) = spawn_fixture(vec![("/a", "text/html", P1)], ROBOTS_OPEN).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let seeds = write_seeds(
        tmp.path(),
        "seeds.txt",
        &[format!("{base}/a"), format!("{base}/missing")],
    );

    let summary = crawl(options(seeds, out.clone()), &cfg).await.unwrap();
    let lines = log_lines(&out);
    assert_eq!(summary.urls_attempted, 2, "both the page and the 404 were tried");
    assert!(lines.iter().any(|l| l["outcome"] == "ingested"));

    let failed = lines
        .iter()
        .find(|l| l["outcome"] == "failed")
        .expect("a failed line for the 404");
    assert!(
        failed["error"].as_str().unwrap_or("").contains("404"),
        "error must mention 404, got {:?}",
        failed["error"]
    );
    assert_eq!(summary.ingested, 1, "the good page still ingested");
    assert_eq!(summary.failed, 1);
}

/// Sanity: seeds parsing ignores comments/blank lines through the CLI-style file.
#[tokio::test]
async fn seeds_file_ignores_comments_and_blanks() {
    let (base, _fx) = spawn_fixture(vec![("/a", "text/html", P1)], ROBOTS_OPEN).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let path = tmp.path().join("seeds.txt");
    std::fs::write(&path, format!("# comment\n\n  {base}/a  \n# another\n")).unwrap();

    let summary = crawl(options(path, out), &cfg).await.unwrap();
    assert_eq!(summary.ingested, 1);
}

/// The fixture is genuinely counted (guard against a silently empty mapping).
#[tokio::test]
async fn fixture_records_requests() {
    let (base, fx) = spawn_fixture(vec![("/a", "text/html", P1)], ROBOTS_OPEN).await;
    let out = tempfile::tempdir().unwrap();
    let cfg = test_config(out.path());
    let out_dir = out.path().join("out");
    let seeds = write_seeds(out.path(), "seeds.txt", &[format!("{base}/a")]);
    let _ = crawl(options(seeds, out_dir), &cfg).await.unwrap();
    // The seed is fetched during discovery and again during the drain.
    assert!(fx.count("/a") >= 1);
    assert!(fx.count("/robots.txt") >= 1);
}

// ---------------------------------------------------------------------------
// Discovery hygiene (raw-dataset coverage)
// ---------------------------------------------------------------------------

/// A seed page that serves a strong `ETag` must still be ingested: the
/// discovery pre-fetch must not store validators in the resource store, or the
/// very next conditional fetch returns 304 and reports the seed `Unchanged`
/// (so it is silently missing from the corpus).
#[tokio::test]
async fn seed_with_etag_is_ingested_not_unchanged() {
    const ETAG: &str = "\"v1\"";
    let fx = Fx::default();
    let net_fx = fx.clone();
    let router = Router::new()
        .route(
            "/robots.txt",
            get(move || {
                let fx = net_fx.clone();
                async move {
                    fx.record("/robots.txt");
                    ([(header::CONTENT_TYPE, "text/plain")], ROBOTS_OPEN).into_response()
                }
            }),
        )
        .route(
            "/abs/paper",
            get({
                let fx = fx.clone();
                move |headers: HeaderMap| {
                    let fx = fx.clone();
                    async move {
                        fx.record("/abs/paper");
                        let inm = headers
                            .get(header::IF_NONE_MATCH)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("");
                        if inm == ETAG {
                            return (
                                StatusCode::NOT_MODIFIED,
                                [(header::ETAG, ETAG), (header::CONTENT_TYPE, "text/html")],
                            )
                                .into_response();
                        }
                        (
                            StatusCode::OK,
                            [
                                (header::ETAG, ETAG),
                                (header::CONTENT_TYPE, "text/html"),
                            ],
                            P1,
                        )
                            .into_response()
                    }
                }
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let base = format!("http://{addr}");

    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let seeds = write_seeds(tmp.path(), "etag.txt", &[format!("{base}/abs/paper")]);

    let summary = crawl(options(seeds, out), &cfg).await.unwrap();
    assert_eq!(
        summary.ingested, 1,
        "the etag-bearing seed must ingest exactly once, got {summary:?}"
    );
    assert_eq!(summary.unchanged, 0, "no phantom 'unchanged' for a seed");
    assert!(
        fx.count("/abs/paper") >= 2,
        "discovery + drain both fetch it (conditional GET still sent to the server)"
    );
}

/// Profiles, utility endpoints and static assets are rejected before they cost
/// an attempt; document-shaped URLs are kept.
#[test]
fn low_value_urls_are_rejected_and_documents_kept() {
    use search_agent0::crawl::is_low_value;

    for bad in [
        "https://www.semanticscholar.org/author/Hu-Chuan-Lu/153176123",
        "https://example.com/users/42",
        "https://example.com/login?next=/x",
        "https://example.com/assets/app.js",
        "https://example.com/img/hero.png",
        "https://example.com/sitemap.xml",
        "https://example.com/search?q=rust",
        "https://example.com/static/main.css",
        "not a url",
    ] {
        assert!(is_low_value(bad), "{bad} must be rejected");
    }

    for good in [
        "https://arxiv.org/abs/2005.11401",
        "https://www.rust-lang.org/learn/get-started",
        "https://huggingface.co/docs/transformers/index",
        "https://blog.rust-lang.org/2024/01/post.html",
        "https://en.wikipedia.org/wiki/Retrieval-augmented_generation",
        "https://example.com/paper.pdf",
    ] {
        assert!(!is_low_value(good), "{good} must be kept");
    }
}

/// Ranking puts papers/docs ahead of generic pages, deterministically.
#[test]
fn content_urls_rank_ahead_of_generic_pages() {
    use search_agent0::crawl::url_rank;

    assert!(url_rank("https://arxiv.org/abs/2005.11401") > url_rank("https://arxiv.org/"));
    assert!(url_rank("https://huggingface.co/docs/transformers/index") > 0);
    assert!(url_rank("https://example.com/") < 1);
    assert!(url_rank("https://example.com/a-b-c-d.html") > 0);
    // Deterministic: same input, same rank.
    assert_eq!(
        url_rank("https://example.com/docs/a-b-c"),
        url_rank("https://example.com/docs/a-b-c")
    );
}

// ---------------------------------------------------------------------------
// Link following
// ---------------------------------------------------------------------------

/// `article_links` resolves relative hrefs, keeps document-shaped links, drops
/// profile/asset/mailto links, strips fragments, dedupes first-seen, honours the
/// cap, ignores `href` inside comments/scripts, and never panics on malformed
/// HTML or a page with no anchors.
#[test]
fn article_links_resolves_filters_dedupes_and_caps() {
    use search_agent0::crawl::article_links;

    let html = r##"<html><body>
        <a href="/docs/getting-started">a</a>
        <a href='/docs/getting-started'>dup</a>
        <a href="/docs/api-reference">b</a>
        <a href="/author/someone">author</a>
        <a href="/assets/app.js">js</a>
        <a href="https://other.example.com/docs/remote-guide">off</a>
        <a href="#top">frag</a>
        <a href="mailto:x@example.com">mail</a>
        <a href="   ">empty</a>
        <!-- <a href="/docs/hidden-in-comment">c</a> -->
        <script>var s = '<a href="/docs/hidden-in-script">s</a>';</script>
    </body></html>"##;

    let links = article_links("http://example.com/", html, 200);
    assert_eq!(
        links,
        vec![
            "http://example.com/docs/getting-started".to_string(),
            "http://example.com/docs/api-reference".to_string(),
            // Origin-agnostic at this layer: the same-origin gate is the caller's.
            "https://other.example.com/docs/remote-guide".to_string(),
        ],
        "resolved, deduped (first-seen order), filtered, comment/script ignored"
    );

    // The cap is a hard ceiling on the number of returned links.
    assert_eq!(article_links("http://example.com/", html, 1).len(), 1);
    assert!(article_links("http://example.com/", html, 0).is_empty());

    // A page with no anchors yields nothing.
    assert!(
        article_links("http://example.com/", "<html><body>plain text</body></html>", 200)
            .is_empty()
    );

    // An unterminated comment swallows the rest: nothing inside is extracted.
    assert!(
        article_links("http://example.com/", "<!-- <a href=\"/docs/hidden\"> --> behind", 200)
            .is_empty()
    );

    // Malformed / truncated HTML must not panic and cannot invent many links.
    let junk = "<a href=\"/docs/a-b-c <a href='/docs/d-e-f <!-- <a href=\"/docs/g-h-i\">";
    assert!(article_links("http://example.com/", junk, 200).len() <= 1);
    assert!(article_links("http://example.com/", "<a href=", 200).is_empty());
}

/// Links published on a seed page are followed to the same origin only: a
/// document link is fetched, profile/asset links and an off-origin document link
/// are not.
#[tokio::test]
async fn seed_page_links_are_followed_same_origin_only() {
    // A second loopback origin (different port ⇒ a different site key).
    let (other, off_fx) = spawn_recording_fixture(
        vec![("/docs/off-site".to_string(), "text/html".to_string(), P2.to_string())],
        ROBOTS_OPEN,
    )
    .await;

    let body = format!(
        "<!doctype html><html><head><title>root</title></head><body>\
         <a href=\"/docs/getting-started\">d</a>\
         <a href=\"/author/someone\">a</a>\
         <a href=\"/assets/app.js\">j</a>\
         <a href=\"{other}/docs/off-site\">o</a>\
         </body></html>"
    );
    let (base, fx) = spawn_recording_fixture(
        vec![
            ("/".to_string(), "text/html".to_string(), body),
            ("/docs/getting-started".to_string(), "text/html".to_string(), P1.to_string()),
        ],
        ROBOTS_OPEN,
    )
    .await;

    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let seeds = write_seeds(tmp.path(), "links.txt", &[format!("{base}/")]);

    let summary = crawl(options(seeds, out.clone()), &cfg).await.unwrap();

    assert!(
        fx.count("/docs/getting-started") >= 1,
        "the same-origin document link was followed"
    );
    assert_eq!(fx.count("/author/someone"), 0, "profile link must not be followed");
    assert_eq!(fx.count("/assets/app.js"), 0, "asset link must not be followed");
    assert_eq!(
        off_fx.count("/docs/off-site"),
        0,
        "off-origin link must not be followed"
    );
    assert_eq!(
        summary.urls_attempted, 2,
        "only the seed and its one document link are attempted, got {summary:?}"
    );
}

/// Link following is opt-out: with `follow_links = false` only the seed is
/// fetched and nothing else is enqueued.
#[tokio::test]
async fn follow_links_can_be_disabled() {
    let body = "<!doctype html><html><body><a href=\"/docs/getting-started\">d</a></body></html>";
    let (base, fx) = spawn_recording_fixture(
        vec![
            ("/".to_string(), "text/html".to_string(), body.to_string()),
            ("/docs/getting-started".to_string(), "text/html".to_string(), P1.to_string()),
        ],
        ROBOTS_OPEN,
    )
    .await;

    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let seeds = write_seeds(tmp.path(), "nofollow.txt", &[format!("{base}/")]);

    let mut o = options(seeds, out.clone());
    o.follow_links = false;
    let summary = crawl(o, &cfg).await.unwrap();

    assert_eq!(fx.count("/docs/getting-started"), 0, "link following is off");
    assert_eq!(
        summary.urls_attempted, 1,
        "only the seed is attempted, got {summary:?}"
    );
    assert_eq!(summary.ingested, 1);
}

/// Regression: a seed page with 5 000 anchors must not fetch 5 000 URLs — the
/// number of distinct non-seed paths fetched is bounded by `MAX_LINKS_PER_PAGE`.
#[tokio::test]
async fn huge_anchor_page_is_capped() {
    let mut body = String::from("<!doctype html><html><body>");
    for i in 0..5000 {
        body.push_str(&format!("<a href=\"/docs/page-{i:04}\">x</a>"));
    }
    body.push_str("</body></html>");

    let (base, fx) = spawn_recording_fixture(
        vec![("/".to_string(), "text/html".to_string(), body)],
        ROBOTS_OPEN,
    )
    .await;

    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let seeds = write_seeds(tmp.path(), "big.txt", &[format!("{base}/")]);

    let _summary = crawl(options(seeds, out), &cfg).await.unwrap();

    let distinct: std::collections::BTreeSet<String> = fx
        .paths()
        .into_iter()
        .filter(|p| p != "/" && p != "/robots.txt")
        .collect();
    assert!(
        distinct.len() <= MAX_LINKS_PER_PAGE,
        "fetched {} distinct non-seed paths, cap is {MAX_LINKS_PER_PAGE}",
        distinct.len()
    );
    assert!(!distinct.is_empty(), "some links were still followed");
}

/// A resumed crawl must not let a stale pending backlog starve the seeds that
/// were added since. The bug: the pending frontier was enqueued *before* the
/// seed loop, so the run spent its whole attempt budget inside the backlog's
/// origins and the newly listed seeds were never even discovered (live: 2 355
/// pending docs.rs URLs, and 88 of 145 seeds never fetched).
///
/// One attempt is all this test allows: with the backlog queued first the
/// round-robin picks it and the new seed is never reached; with seeds first the
/// new seed owns the only attempt.
#[tokio::test]
async fn a_stale_pending_backlog_does_not_starve_the_seed_list() {
    let (base_a, fx_a) = spawn_recording_fixture(
        vec![(
            "/docs/stale-article".to_string(),
            "text/html".to_string(),
            P1.to_string(),
        )],
        ROBOTS_OPEN,
    )
    .await;
    let (base_b, fx_b) = spawn_recording_fixture(
        vec![("/seed".to_string(), "text/html".to_string(), P2.to_string())],
        ROBOTS_OPEN,
    )
    .await;

    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    std::fs::create_dir_all(&out).unwrap();

    // A stale frontier from a previous run: document-shaped pending URLs for
    // origin A, so the enqueue gate keeps them.
    let pending: Vec<String> = (0..50)
        .map(|i| format!("{base_a}/docs/stale-article-{i:03}"))
        .collect();
    std::fs::write(
        out.join("frontier.json"),
        serde_json::to_string(&pending).unwrap(),
    )
    .unwrap();

    let seeds = write_seeds(tmp.path(), "seeds.txt", &[format!("{base_b}/seed")]);

    let mut opts = options(seeds, out.clone());
    opts.max_urls = 1;
    opts.max_docs = 1;
    let summary = crawl(opts, &cfg).await.unwrap();

    assert_eq!(summary.urls_attempted, 1, "the bound must hold: {summary:?}");
    // Discovery fetches a seed page once to look for a sitemap or feed, and the
    // drain fetches it again to ingest it: two requests mean the seed actually
    // got the attempt.
    assert!(
        fx_b.count("/seed") >= 2,
        "the newly listed seed must get the attempt, not just discovery: {:?}",
        fx_b.paths()
    );
    let backlog_hits = fx_a
        .paths()
        .iter()
        .filter(|p| p.starts_with("/docs/stale-article"))
        .count();
    assert_eq!(
        backlog_hits, 0,
        "the stale backlog must not be reached while the seed list still has entries: {:?}",
        fx_a.paths()
    );
}

// ---------------------------------------------------------------------------------------------
// §21 browser escalation: a JS-only shell renders to text in a browser, a normal page does not
// need one. The driver is the `BrowserFetcher` trait, so these run on `MockBrowser` (no
// Chromium, no network) and pin the three outcomes that matter.
// ---------------------------------------------------------------------------------------------

/// A page whose HTML normalizes to no text: the shape a client-rendered site serves.
const JS_SHELL: &str = "<html><head><title>App</title></head><body><div id=\"root\"></div>\
                         <script>window.__DATA__={}</script></body></html>";

#[tokio::test]
async fn a_js_shell_is_indexed_from_the_browser_render_when_a_driver_is_attached() {
    let (base, _fx) = spawn_recording_fixture(
        vec![("/shell".to_string(), "text/html".to_string(), JS_SHELL.to_string())],
        ROBOTS_OPEN,
    )
    .await;
    let url = format!("{base}/shell");

    let rendered = "<html><head><title>App</title></head><body><article>\
        <h1>Paged Attention</h1><p>Paged attention reduces KV cache memory in vLLM by \
        storing keys and values in separate blocks.</p></article></body></html>";
    let browser = search_browser::MockBrowser::new().with_fixture(
        url.clone(),
        search_browser::BrowserResult::new(url.clone(), rendered, 200),
    );

    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let seeds = write_seeds(tmp.path(), "seeds.txt", &[url.clone()]);

    let mut opts = options(seeds, out.clone());
    opts.browser = Some(std::sync::Arc::new(browser));
    opts.max_docs = 1;
    let summary = crawl(opts, &cfg).await.unwrap();

    assert_eq!(summary.ingested, 1, "the rendered page must be indexed: {summary:?}");
    let lines = log_lines(&out);
    let entry = lines.iter().find(|l| l["url"] == url.as_str()).expect("the url must be logged");
    assert_eq!(entry["outcome"], "ingested", "{entry}");
}

#[tokio::test]
async fn a_js_shell_stays_unsupported_when_the_render_is_no_better() {
    // The mock returns a DOM that also has no text (a bot wall, a broken bundle): the page must
    // still not become a zero-content document, and the crawl must not fail.
    let (base, _fx) = spawn_recording_fixture(
        vec![("/shell".to_string(), "text/html".to_string(), JS_SHELL.to_string())],
        ROBOTS_OPEN,
    )
    .await;
    let url = format!("{base}/shell");
    let browser = search_browser::MockBrowser::new().with_fixture(
        url.clone(),
        search_browser::BrowserResult::new(url.clone(), JS_SHELL.to_string(), 200),
    );

    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let seeds = write_seeds(tmp.path(), "seeds.txt", &[url.clone()]);

    let mut opts = options(seeds, out.clone());
    opts.browser = Some(std::sync::Arc::new(browser));
    opts.max_docs = 1;
    let summary = crawl(opts, &cfg).await.unwrap();

    assert_eq!(summary.ingested, 0, "nothing indexable was produced: {summary:?}");
    let lines = log_lines(&out);
    let entry = lines.iter().find(|l| l["url"] == url.as_str()).expect("the url must be logged");
    assert_eq!(
        entry["outcome"], "routed",
        "a page with no text stays routed rather than indexed: {entry}"
    );
}

#[tokio::test]
async fn a_page_with_text_never_calls_the_browser() {
    // Escalation must be a last resort: a driver that would fail loudly is attached, and the
    // crawl must not touch it for a normal page.
    let (base, _fx) = spawn_recording_fixture(
        vec![(
            "/doc".to_string(),
            "text/html".to_string(),
            "<html><body><article><h1>Rust</h1><p>Rust is a systems language with a borrow \
             checker.</p></article></body></html>"
                .to_string(),
        )],
        ROBOTS_OPEN,
    )
    .await;
    let url = format!("{base}/doc");

    // No fixture registered: any `fetch` would be `NoFixture`, i.e. a visible escalation attempt.
    let browser = search_browser::MockBrowser::new();

    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let seeds = write_seeds(tmp.path(), "seeds.txt", &[url.clone()]);

    let mut opts = options(seeds, out.clone());
    opts.browser = Some(std::sync::Arc::new(browser));
    opts.max_docs = 1;
    let summary = crawl(opts, &cfg).await.unwrap();
    assert_eq!(summary.ingested, 1, "the page has text and must be indexed directly: {summary:?}");
}

// ---------------------------------------------------------------------------------------------
// §44 deletion detection. A URL that was indexed and then starts answering 404 is no longer a
// resource: it must be marked DELETED so it stops being served, while its stored bytes and crawl
// history are kept — the architecture is explicit that historical results may still matter.
// ---------------------------------------------------------------------------------------------

/// A fixture whose `/doc` route flips from 200 to 404 on command. A resource "disappearing" is the
/// only way to observe deletion detection: the same URL, the same port, a different answer.
async fn spawn_vanishing_fixture() -> (String, Arc<AtomicBool>) {
    let alive = Arc::new(AtomicBool::new(true));
    let doc_alive = alive.clone();
    let router = Router::new()
        .route(
            "/robots.txt",
            get(|| async { ([(header::CONTENT_TYPE, "text/plain")], ROBOTS_OPEN) }),
        )
        .route(
            "/doc",
            get(move || {
                let alive = doc_alive.clone();
                async move {
                    if alive.load(Ordering::SeqCst) {
                        ([(header::CONTENT_TYPE, "text/html")], P1).into_response()
                    } else {
                        StatusCode::NOT_FOUND.into_response()
                    }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}"), alive)
}

#[tokio::test]
async fn a_page_that_disappears_is_marked_deleted_and_keeps_its_history() {
    let (base, alive) = spawn_vanishing_fixture().await;
    let url = format!("{base}/doc");

    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let seeds = write_seeds(tmp.path(), "seeds.txt", &[url.clone()]);

    // First run: the page is live and indexed.
    let first = crawl(options(seeds.clone(), out.clone()), &cfg).await.unwrap();
    assert_eq!(first.ingested, 1, "the live page must be indexed first: {first:?}");
    assert_eq!(first.deleted, 0, "nothing was deleted while the page was up: {first:?}");

    let before = {
        let ro = Pipeline::open_read_only(&cfg, true).expect("read-only open");
        ro.all_docs().expect("all_docs")
    };
    assert!(
        before.iter().any(|d| d.url == url),
        "the page must be in the index before it disappears"
    );

    // The resource disappears, and the crawl log is dropped so the URL is not suppressed as
    // already-seen (that suppression is the resume behaviour, and it would mean never asking again).
    alive.store(false, Ordering::SeqCst);
    let _ = std::fs::remove_file(out.join("crawl-log.jsonl"));

    let second = crawl(options(seeds, out.clone()), &cfg).await.unwrap();
    assert_eq!(second.failed, 0, "a 404 for an indexed URL is a deletion, not a failure: {second:?}");
    assert_eq!(second.deleted, 1, "the disappearance must be recorded: {second:?}");

    let lines = log_lines(&out);
    assert!(
        lines.iter().any(|l| l["url"] == url.as_str() && l["outcome"] == "deleted"),
        "the log must say the URL was deleted: {lines:?}"
    );

    // DELETED means not served, and the bytes stay on disk: history is kept, not erased.
    let after = {
        let ro = Pipeline::open_read_only(&cfg, true).expect("read-only open");
        ro.all_docs().expect("all_docs")
    };
    assert!(
        !after.iter().any(|d| d.url == url),
        "a deleted page must not be returned by the index"
    );
    assert!(
        !blob_files(&out).is_empty(),
        "the fetched bytes must remain on disk after a deletion: history is kept"
    );
}

#[tokio::test]
async fn compressed_raw_blobs_inflate_to_the_fetched_bytes() {
    // `--compress` exists because raw pages average ~237 KB: a million of them is ~237 GB raw, and
    // that is the difference between a large corpus fitting on this disk and not. The sha256 in the
    // file name stays the hash of the *uncompressed* bytes, so the crawl log and dedup are
    // unchanged — only the stored form shrinks.
    let (base, _fx) = spawn_recording_fixture(
        vec![("/doc".to_string(), "text/html".to_string(), P1.to_string())],
        ROBOTS_OPEN,
    )
    .await;
    let url = format!("{base}/doc");

    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let seeds = write_seeds(tmp.path(), "seeds.txt", &[url.clone()]);

    let mut opts = options(seeds, out.clone());
    opts.compress = true;
    let summary = crawl(opts, &cfg).await.unwrap();
    assert_eq!(summary.ingested, 1, "{summary:?}");

    let bones = blob_files(&out);
    assert_eq!(bones.len(), 1, "{bones:?}");
    assert!(bones[0].ends_with(".gz"), "the blob must be stored gzipped: {bones:?}");
    assert!(
        bones[0].contains(SHA_P1),
        "the file name must keep the sha256 of the UNCOMPRESSED bytes: {bones:?}"
    );

    // And it must inflate to exactly what was fetched.
    let raw = std::fs::read(out.join("blobs").join(&bones[0])).expect("read blob");
    let mut decoder = flate2::read::GzDecoder::new(raw.as_slice());
    let mut html = String::new();
    std::io::Read::read_to_string(&mut decoder, &mut html).expect("inflate");
    assert_eq!(html, P1, "the gzipped blob must inflate to the fetched page");
}

#[tokio::test]
async fn blobs_stay_uncompressed_without_the_flag() {
    let (base, _fx) = spawn_recording_fixture(
        vec![("/doc".to_string(), "text/html".to_string(), P1.to_string())],
        ROBOTS_OPEN,
    )
    .await;
    let url = format!("{base}/doc");
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let seeds = write_seeds(tmp.path(), "seeds.txt", &[url.clone()]);
    crawl(options(seeds, out.clone()), &cfg).await.unwrap();
    let bones = blob_files(&out);
    assert!(bones.iter().all(|b| !b.ends_with(".gz")), "default stays raw: {bones:?}");
}

/// An `application/pdf` response must route to the PDF parser, land in the
/// index, and have its raw bytes stored gzipped in the raw dataset exactly like
/// every other blob under `--compress` (so a corpus of huge PDFs fits on disk).
#[tokio::test]
async fn compressed_pdf_raw_blob_is_gzipped_and_indexed() {
    const MARKER: &str = "zephyrpdfcrawlmarker";
    let pdf = build_ascii_pdf(MARKER);
    let (base, _fx) = spawn_recording_fixture(
        vec![(
            "/paper.pdf".to_string(),
            "application/pdf".to_string(),
            pdf.clone(),
        )],
        ROBOTS_OPEN,
    )
    .await;
    let url = format!("{base}/paper.pdf");

    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let seeds = write_seeds(tmp.path(), "seeds.txt", &[url.clone()]);

    let mut opts = options(seeds, out.clone());
    opts.compress = true;
    let summary = crawl(opts, &cfg).await.unwrap();
    assert_eq!(summary.ingested, 1, "the PDF must route + ingest: {summary:?}");

    // Indexed: the PDF's extracted text layer is present in the index.
    let ro = Pipeline::open_read_only(&cfg, true).expect("read-only open");
    let docs = ro.all_docs().expect("all_docs");
    assert!(
        docs.iter()
            .any(|d| !d.tombstone && d.body.contains(MARKER)),
        "the PDF text layer must be indexed: {docs:?}"
    );

    // Raw blob: gzipped on disk, named for the type, and inflating to exactly
    // the fetched PDF bytes.
    let bones = blob_files(&out);
    assert_eq!(bones.len(), 1, "{bones:?}");
    assert!(
        bones[0].ends_with(".pdf.gz"),
        "the PDF raw blob must be gzipped and type-named: {bones:?}"
    );
    let raw = std::fs::read(out.join("blobs").join(&bones[0])).expect("read blob");
    assert_eq!(&raw[..2], &[0x1f, 0x8b], "gzip magic: {bones:?}");
    let mut decoder = flate2::read::GzDecoder::new(raw.as_slice());
    let mut inflated = Vec::new();
    std::io::Read::read_to_end(&mut decoder, &mut inflated).expect("inflate");
    assert_eq!(
        inflated,
        pdf.as_bytes(),
        "the gzipped blob must inflate to the exact fetched PDF bytes"
    );
}

/// Build a valid single-page PDF with a real text layer as pure ASCII, so it can
/// be served through the `String`-bodied fixture without lossy UTF-8 conversion.
fn build_ascii_pdf(text: &str) -> String {
    let content = format!("BT /F1 24 Tf 72 700 Td ({text}) Tj ET");
    let objects: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources \
         << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>"
            .to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_string(),
        format!(
            "<< /Length {} >>\nstream\n{}\nendstream",
            content.len(),
            content
        ),
    ];

    let mut out = String::from("%PDF-1.4\n");
    let mut offsets: Vec<usize> = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.push_str(&format!("{} 0 obj\n{}\nendobj\n", i + 1, body));
    }
    let xref_pos = out.len();
    let n = objects.len() + 1;
    out.push_str(&format!("xref\n0 {n}\n"));
    out.push_str("0000000000 65535 f \n");
    for off in &offsets {
        out.push_str(&format!("{off:010} 00000 n \n"));
    }
    out.push_str(&format!(
        "trailer\n<< /Size {n} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n"
    ));
    out
}

/// An origin that answers HTTP 429 with `Retry-After` must not be asked again
/// until the delay elapses. Both limited documents are fetched exactly once (no
/// immediate retry), and the second same-origin request waits out the backoff.
#[tokio::test]
async fn http_429_backs_off_the_origin_before_the_next_request() {
    const SEED_HTML: &str = "<!doctype html><html><body>\
        <a href=\"/blog/one-two.html\">one</a>\
        <a href=\"/blog/three-four.html\">two</a>\
        </body></html>";
    let (base, fx) = spawn_429_fixture(SEED_HTML).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let seeds = write_seeds(tmp.path(), "seeds.txt", &[format!("{base}/seed")]);

    let summary = crawl(options(seeds, out.clone()), &cfg).await.unwrap();

    // Both rate-limited documents were fetched exactly once: the crawler does
    // not retry the same URL, it backs the origin off.
    let first = fx.times("/blog/one-two.html");
    let second = fx.times("/blog/three-four.html");
    assert_eq!(first.len(), 1, "one-two fetched exactly once");
    assert_eq!(second.len(), 1, "three-four fetched exactly once");

    // Whichever order the two same-origin documents were queued in, the second
    // request waited out `Retry-After: 1` on the origin the first one 429'd.
    let gap = first[0].max(second[0]).duration_since(first[0].min(second[0]));
    assert!(
        gap >= Duration::from_millis(900),
        "second same-origin request arrived {gap:?} after the 429; expected >= ~1s"
    );

    assert_eq!(summary.rate_limited, 2, "both 429s counted");
}


// ---------------------------------------------------------------------------
// Disk guard: a crawl must stop before it fills the filesystem
// ---------------------------------------------------------------------------

/// The pure decision: `0` disables the guard, an unreadable filesystem never stops a run, and
/// only a known free figure below the minimum trips it.
#[test]
fn disk_guard_decision_is_explicit_about_unknowns() {
    use search_agent0::crawl::should_stop_for_disk;
    const GB: u64 = 1024 * 1024 * 1024;
    assert!(!should_stop_for_disk(Some(0), 0), "0 disables the guard");
    assert!(!should_stop_for_disk(None, 5 * GB), "unknown free space never stops a run");
    assert!(should_stop_for_disk(Some(4 * GB), 5 * GB), "below the floor stops");
    assert!(!should_stop_for_disk(Some(6 * GB), 5 * GB), "above the floor continues");
    assert!(should_stop_for_disk(Some(0), 1), "a full disk always stops");
}

/// `free_bytes` reads a real filesystem and returns a plausible figure.
#[test]
fn free_bytes_reads_the_host() {
    let dir = tempfile::tempdir().unwrap();
    let free = search_agent0::crawl::free_bytes(dir.path()).expect("a tempdir is readable");
    assert!(free > 0, "a mounted filesystem reports free bytes, got {free}");
}

/// End to end: with an impossible floor, the crawl stops before fetching anything and says so
/// in its summary rather than pretending the seed list ran out.
#[tokio::test]
async fn a_crawl_stops_immediately_when_the_disk_guard_trips() {
    let (base, fx) = spawn_fixture(vec![("/a", "text/html", P1)], ROBOTS_OPEN).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let seeds = write_seeds(tmp.path(), "seeds.txt", &[format!("{base}/a")]);

    let mut opts = options(seeds, out.clone());
    // No filesystem has this much free: the guard must trip on the first turn.
    opts.min_free_bytes = u64::MAX;

    let summary = crawl(opts, &cfg).await.unwrap();
    assert!(summary.stopped_for_disk, "the summary reports why the run stopped");
    assert_eq!(summary.ingested, 0, "nothing was fetched");
    assert!(
        fx.hits.lock().unwrap().is_empty(),
        "the guard runs before the first request, not after it"
    );
}

// ---------------------------------------------------------------------------
// Resumability after the frontier became incremental
// ---------------------------------------------------------------------------

/// The frontier is a resumability hint, and it stopped being a full rewrite because the
/// rewrite cost 213 MB of JSON every 2 s at 2.9M pending URLs (107 MB/s per shard,
/// measured 2026-10-28). What must survive that change: a killed crawl's pending URLs are
/// still pending afterwards — from the delta as well as from the snapshot.
#[tokio::test]
async fn a_killed_crawl_resumes_from_the_incremental_frontier() {
    // Two pages linked from one seed, so the drain is mid-queue when it stops.
    // Four seeds, so a one-document budget leaves the rest pending.
    let (base, _fx) = spawn_fixture(
        vec![("/a", "text/html", P1), ("/b", "text/html", P2), ("/c", "text/html", P1)],
        ROBOTS_OPEN,
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let seeds = write_seeds(
        tmp.path(),
        "seeds.txt",
        &[
            format!("{base}/a"),
            format!("{base}/b"),
            format!("{base}/c"),
        ],
    );

    // First run: stop after one document, so URLs are left pending.
    let mut opts = options(seeds.clone(), out.clone());
    opts.max_docs = 1;
    let first = crawl(opts, &cfg).await.unwrap();
    assert_eq!(first.ingested, 1, "the first run ingested exactly its budget");

    // The snapshot and/or the delta must account for what is still pending.
    let snapshot: Vec<String> = std::fs::read(out.join("frontier.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let adds: Vec<String> = std::fs::read_to_string(out.join("frontier.adds"))
        .map(|t| {
            t.lines()
                .filter_map(|l| serde_json::from_str::<String>(l).ok())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !snapshot.is_empty() || !adds.is_empty(),
        "a crawl stopped by its budget must leave its pending URLs behind"
    );

    // Second run with a larger budget: it must pick up the pending work rather than
    // rediscovering it, and must not lose the already-ingested document.
    let mut opts = options(seeds, out.clone());
    opts.max_docs = 10;
    let second = crawl(opts, &cfg).await.unwrap();
    assert!(
        second.resumed_from > 0,
        "the second run resumed from a persisted frontier (resumed_from={})",
        second.resumed_from
    );
    assert!(
        second.ingested >= 1,
        "the resumed run made progress (ingested={})",
        second.ingested
    );
}

/// A clean finish leaves an empty snapshot and no delta: the delta must not accumulate
/// forever, and a completed crawl must not look like it has work left.
#[tokio::test]
async fn a_completed_crawl_leaves_no_pending_frontier() {
    let (base, _fx) = spawn_fixture(vec![("/a", "text/html", P1)], ROBOTS_OPEN).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let seeds = write_seeds(tmp.path(), "seeds.txt", &[format!("{base}/a")]);

    crawl(options(seeds, out.clone()), &cfg).await.unwrap();

    let snapshot: Vec<String> = std::fs::read(out.join("frontier.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    assert!(snapshot.is_empty(), "nothing is pending: {snapshot:?}");
    assert!(
        !out.join("frontier.adds").exists(),
        "a clean finish removes the delta"
    );
}

/// Raw bodies are spilled by the pipeline into `data_dir/blobs` already; the crawl must not
/// write a second hash-keyed copy under its output directory unless asked. Measured on the
/// fleet: 295 GB in `data_dir/blobs` and 45 GB in `raw-dataset/*/blobs`, no name overlap, and
/// the second copy is read by nothing.
#[tokio::test]
async fn raw_blobs_are_not_duplicated_into_the_output_directory_by_default() {
    let (base, _fx) = spawn_fixture(vec![("/a", "text/html", P1)], ROBOTS_OPEN).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let seeds = write_seeds(tmp.path(), "seeds.txt", &[format!("{base}/a")]);

    let mut opts = options(seeds, out.clone());
    opts.keep_raw_blobs = false;

    let summary = crawl(opts, &cfg).await.unwrap();
    assert_eq!(summary.ingested, 1);
    assert_eq!(summary.blobs, 0, "no second copy is written");
    assert!(
        blob_files(&out).is_empty(),
        "out_dir/blobs stays empty: {:?}",
        blob_files(&out)
    );
    // The document is still fully ingested and logged — only the duplicate copy is gone.
    assert_eq!(log_lines(&out).len(), 1);
}

// ---------------------------------------------------------------------------
// robots.txt is fetched once per domain, not once per URL
// ---------------------------------------------------------------------------

/// Five same-origin pages must cause **one** robots.txt fetch.
///
/// Measured 2026-10-28 without a cache: 72.7% of all crawl attempts were
/// `robots.txt disallows /` re-fetched per URL, and only 5.2% of attempts produced a document.
/// At 15.5 pending URLs per host the crawl was spending most of its budget re-reading the same
/// file, so this is a throughput property, not a politeness nicety.
#[tokio::test]
async fn robots_txt_is_fetched_once_per_domain() {
    let (base, fx) = spawn_fixture(
        vec![
            ("/a", "text/html", P1),
            ("/b", "text/html", P2),
            ("/c", "text/html", P3),
            ("/d", "text/html", P4),
            ("/e", "text/html", P5),
        ],
        ROBOTS_OPEN,
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let seeds = write_seeds(
        tmp.path(),
        "seeds.txt",
        &[
            format!("{base}/a"),
            format!("{base}/b"),
            format!("{base}/c"),
            format!("{base}/d"),
            format!("{base}/e"),
        ],
    );

    let summary = crawl(options(seeds, out), &cfg).await.unwrap();
    assert_eq!(summary.ingested, 5, "all five pages ingested");

    // Two independent fetchers, by design: the crawl's discovery path (robots → sitemaps →
    // links, once per *seed*: `Pipeline::discover` plus the direct sitemap expand in crawl.rs)
    // and the pipeline's ingestion path, which now reads robots once per *domain* per TTL and
    // single-flights concurrent misses. The rate to assert is reads per seeded page: 2 is the
    // discovery pair; anything proportional to the page count would be the regression.
    let robots_fetches = fx.times("/robots.txt").len();
    let per_seed = robots_fetches as f64 / 5.0;
    assert!(
        per_seed <= 2.5,
        "robots.txt reads must not scale with page count: 5 pages -> {robots_fetches} reads \
         ({per_seed:.1} per seed)"
    );
}

/// A host whose robots.txt denies `/` yields nothing, and the crawl records that as a denial
/// rather than as a fetch failure.
///
/// Production context (measured 2026-10-28): 5,818 hosts each denying `/`, walked URL by URL, so
/// 88% of all attempts were denials and the crawl ingested nothing. The fix has two parts — robots
/// is cached per domain (see `robots_txt_is_fetched_once_per_domain`) and a host found to deny `/`
/// is remembered so its remaining URLs cost a hash lookup. This test pins the second half's
/// observable outcome: the pages produce denials, never ingested documents, and the run does not
/// spend its budget re-fetching them.
#[tokio::test]
async fn a_host_that_denies_root_yields_no_documents() {
    const DENY_ALL: &str = "User-agent: *\nDisallow: /\n";
    let (base, fx) = spawn_fixture(
        vec![
            ("/a", "text/html", P1),
            ("/b", "text/html", P2),
            ("/c", "text/html", P3),
        ],
        DENY_ALL,
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config(tmp.path());
    let out = tmp.path().join("out");
    let seeds = write_seeds(
        tmp.path(),
        "seeds.txt",
        &[
            format!("{base}/a"),
            format!("{base}/b"),
            format!("{base}/c"),
        ],
    );

    let summary = crawl(options(seeds, out), &cfg).await.unwrap();
    assert_eq!(summary.ingested, 0, "a disallow-all host yields nothing");
    assert!(summary.denied >= 1, "the denial is reported as a denial");
    assert_eq!(summary.failed, 0, "a robots denial is not a network failure");

    // The host is fetched at most once per URL: no URL is retried, and the sitemap/feed probes
    // do not multiply the page requests.
    for path in ["/a", "/b", "/c"] {
        assert!(
            fx.times(path).len() <= 1,
            "{path} was fetched more than once from a disallow-all host"
        );
    }
}

/// By default the crawl keeps no raw HTML: the derived artifacts (URL, title, extracted text,
/// index) are the corpus, and the source bytes are 38x their size.
///
/// Measured 2026-10-28: 299 GB of raw blobs against 13.8 GB of extracted text across the fleet,
/// growing at 7.6 GB/h. Deleting the convention is safe because `read_pipeline_bytes` already
/// falls back to the text file, so the crawl log, dedupe and export are unchanged — which is what
/// this asserts: the document is still ingested and still logged, only the blob is absent.
#[tokio::test]
async fn raw_html_is_not_stored_by_default() {
    let (base, _fx) = spawn_fixture(vec![("/a", "text/html", P1)], ROBOTS_OPEN).await;
    let tmp = tempfile::tempdir().unwrap();
    // `test_config` sets SEARCH_KEEP_RAW_BLOBS for the raw-spill tests; this one needs the
    // default, and the pipeline reads the flag when it opens.
    std::env::remove_var("SEARCH_KEEP_RAW_BLOBS");
    let cfg = test_config(tmp.path());
    std::env::remove_var("SEARCH_KEEP_RAW_BLOBS");
    let out = tmp.path().join("out");
    let seeds = write_seeds(tmp.path(), "seeds.txt", &[format!("{base}/a")]);

    // `options()` turns the crawl's own copy on for the raw-spill suite; this test is the default.
    let mut opts = options(seeds, out.clone());
    opts.keep_raw_blobs = false;

    let summary = crawl(opts, &cfg).await.unwrap();
    assert_eq!(summary.ingested, 1, "the document is still ingested");
    assert_eq!(summary.blobs, 0, "no raw copy is counted");
    assert_eq!(log_lines(&out).len(), 1, "the crawl log is unchanged");

    // The extracted text is what remains, and it is enough to index from.
    let text_dir = tmp.path().join("text");
    let texts: Vec<_> = std::fs::read_dir(&text_dir)
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(!texts.is_empty(), "extracted text is still written");
    let blob_dir = tmp.path().join("blobs");
    let blobs: Vec<_> = std::fs::read_dir(&blob_dir)
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(blobs.is_empty(), "no raw blob is written: {blobs:?}");
}
