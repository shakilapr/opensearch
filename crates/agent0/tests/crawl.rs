//! Hermetic integration tests for the `agent0 crawl` driver.
//!
//! Everything runs against a single loopback axum fixture (ephemeral port, no
//! public internet) and `tempfile` data/out dirs. The fixture records the path
//! and arrival `Instant` of every request so the politeness test can measure the
//! per-origin interval.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    http::{header, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};

use search_agent0::crawl::{crawl, CrawlOptions};
use search_core::config::Config;

// Fixture bodies. Their SHA-256 values are asserted verbatim in the log.
const P1: &str = "<!doctype html><html><head><title>One</title></head><body><p>pageone alpha</p></body></html>";
const P2: &str = "<!doctype html><html><head><title>Two</title></head><body><p>pagetwo beta</p></body></html>";
const SAME: &str = "<!doctype html><html><head><title>Same</title></head><body><p>identical body</p></body></html>";
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn test_config(data_dir: &Path) -> Config {
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
