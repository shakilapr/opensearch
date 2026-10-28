//! `agent0 crawl` — a resumable, multi-document crawl driver.
//!
//! ```text
//! agent0 crawl SEEDSFILE [--max-docs N] [--concurrency C] [--out DIR] [--max-urls N]
//! ```
//!
//! The driver expands a seeds file (one URL per line, `#` comments and blank
//! lines ignored) into a work queue, then drains it through the **existing**
//! single-URL ingest path ([`Pipeline::crawl_one`]) so robots, the SSRF guard,
//! normalization, dedup and indexing behave exactly as `agent0 ingest`.
//!
//! ## What lands on disk (under `--out`, default `raw-dataset/`)
//!
//! * `crawl-log.jsonl` — one JSON line per fetch attempt:
//!   `{url, final_url, sha256, bytes, content_type, fetched_at, outcome, error}`.
//!   `outcome` is one of `ingested|unchanged|denied|routed|failed`. The line
//!   carries **only** these fields — never a header, credential or cookie.
//! * `blobs/<sha256>.<ext>` — the RAW fetched body bytes for every successful
//!   fetch, `ext` derived from the (sniffed) content type, `.bin` when unknown.
//!   Deduped by `sha256`: identical bytes are written once and never rewritten.
//! * `frontier.json` — the not-yet-processed URLs, so a kill/restart resumes
//!   mid-corpus instead of restarting.
//!
//! ## Resumability
//!
//! On start the driver reads any existing `crawl-log.jsonl` and treats every URL
//! already present as done (all five outcomes are terminal in this schema), then
//! loads `frontier.json` for the pending queue. `resumed_from` reports how many
//! previously-logged URLs were recognised. Every log append and frontier write is
//! atomic (`write temp + rename`) so an interrupted run never leaves a partial
//! JSONL line or a truncated frontier.
//!
//! ## Discovery + draining
//!
//! Seeds are discovered in a continuously-refilled window: every unseen seed is
//! expanded with the existing discovery path ([`Pipeline::discover`]: robots.txt
//! `Sitemap:` lines, sitemap indexes, feeds advertised by the seed HTML) **and**
//! by re-fetching the advertised sitemap/feed bodies and re-parsing them with
//! `search_discovery::{parse_sitemap, parse_feed, sitemaps_in_robots,
//! sitemap_index_urls}`. One task per seed means the window touches many origins
//! at once (never two simultaneous requests to the same host).
//!
//! Discovery results are **streamed**: the moment a seed's discovery task
//! finishes, its URL set is enqueued and the seed is drained while the other
//! discovery tasks are still in flight, and the freed window slot is refilled
//! with the next seed. The drain therefore never waits for the slowest chain in
//! a batch. An earlier shape joined the *whole* chunk before draining anything,
//! so one slow origin (or a widened window) left every concurrency slot idle —
//! the box read a fraction of its cores with work queued. Because each seed is a
//! distinct origin and its discovered URLs are same-origin, a seed's drain can
//! overlap another seed's discovery without ever hitting one host twice. Every
//! discovered URL enters the same queue (`frontier.json`) after dedup against the
//! log and the queue.
//!
//! ## Link following
//!
//! Many roots (arxiv `/list/...`, news home pages, blog indexes) publish no
//! sitemap and no feed, so discovery alone contributes almost nothing. After the
//! sitemap work, [`discover_seed_urls`] therefore also fetches the **seed page's**
//! HTML and enqueues the document-shaped links found on it
//! ([`article_links`]), same-origin only. This is **one level deep** and only
//! for pages that were given as seeds or resumed from the frontier — a page that
//! is itself reached by following a link is fetched and indexed by the normal
//! drain but its own links are never expanded (a second level is a separate,
//! budgeted decision). Cap: [`MAX_LINKS_PER_PAGE`] links per seed page; the
//! global `--max-urls`/`--max-docs` bounds still apply. Opt out with
//! `--no-follow-links`.
//!
//! ## The raw-body seam
//!
//! `Pipeline::crawl_one` drives robots/SSRF/normalize/dedup/index but does not
//! hand back the response body. The pipeline already writes the **exact** fetched
//! bytes to its blob store (`cfg.blob_path(<content-hash>)`), so the raw spill
//! reads them from there rather than fetching around the pipeline — no second
//! network client, and robots/SSRF are honoured because the bytes came through
//! `crawl_one`. `crawl_one` also does not surface the response `Content-Type`, so
//! the log's `content_type` is sniffed from those same bytes via
//! `search_router::classify` (an ingested body is only ever HTML or PDF).
//!
//! ## Politeness
//!
//! Concurrency is bounded **across different origins** (`--concurrency`,
//! default 64, or `SEARCH_CRAWL_CONCURRENCY`). Within an origin at most one
//! request is ever in flight, and a
//! minimum interval separates consecutive requests to the same origin. The
//! interval defaults to 1000 ms and is configurable with
//! `SEARCH_CRAWL_ORIGIN_INTERVAL_MS`. This is implemented locally (a per-origin
//! `busy` flag + last-request timestamp); the `search_frontier` per-origin model
//! is not rewired because doing so would require changing another crate.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use search_browser::BrowserFetcher;

use serde::Serialize;
use tokio::task::JoinSet;
use url::Url;

use search_core::config::Config;
use search_core::{AppError, AppResult};
use search_discovery::{parse_feed, parse_sitemap, sitemap_index_urls, sitemaps_in_robots};
use search_fetcher::http::Fetcher;
use search_router::{classify, Route};

use crate::pipeline::{IngestOutcome, Pipeline};

/// Default destination for the raw corpus.
const DEFAULT_OUT: &str = "raw-dataset";
/// Default bound on successfully ingested documents.
const DEFAULT_MAX_DOCS: usize = 10_000;
/// Default bound on in-flight fetches across different origins.
///
/// The crawl is network-I/O bound: at concurrency 8 a 16-core box sat at ~30%
/// CPU and ~66 docs/min, while the implied fetch latency (concurrency /
/// throughput ≈ 29 s) showed the fetches were simply not overlapped. With three
/// processes at the old 128 slots, a 16-core box still sat at ~14% CPU and
/// ~14.6% RAM against an 80% target: the aggregate in-flight bound (3 × 128)
/// was still far below what the box needs to keep every core parsing, deduping
/// and indexing — the mean fetch latency of ~27 s only fell when the global
/// bound rose enough to overlap that whole round-trip. 512 slots per process
/// keeps the core's parse/index work busy without touching the per-origin
/// politeness floor (one request in flight per origin, plus the configured
/// interval), so no host is ever hammered. It is the *global* fetch bound, not
/// a per-host one: with the discovery window feeding many distinct origins,
/// more slots mean more origins resolving, parsing and indexing at once.
const DEFAULT_CONCURRENCY: usize = 512;
/// Default per-origin minimum interval between requests (milliseconds).
const DEFAULT_ORIGIN_INTERVAL_MS: u64 = 1000;

/// Seeds discovered concurrently, as a multiple of `--concurrency`.
///
/// Discovery is a *chain* of network round-trips per seed (robots.txt →
/// sitemaps → feeds → the seed page). A wide window keeps many distinct hosts
/// resolving and parsing at once, which is what fills the CPU between fetches.
/// It is deliberately decoupled from the drain's own `--concurrency` bound: the
/// drain still respects `--concurrency` (one request in flight per origin, plus
/// the configured interval), while discovery runs wider. Each discovery task
/// holds at most one socket, so the window is capped at
/// [`MAX_DISCOVERY_WINDOW`] to stay inside the usual 1024 fd budget alongside
/// the drain's in-flight fetches. Override the absolute window with
/// `SEARCH_CRAWL_DISCOVERY_WINDOW`.
const DISCOVERY_WINDOW_FACTOR: usize = 4;

/// Hard ceiling on the discovery window so a large drain concurrency cannot
/// exhaust the process's file-descriptor budget (discovery tasks each hold one
/// socket alongside the drain's in-flight fetches).
const MAX_DISCOVERY_WINDOW: usize = 1024;

/// Base delay applied to an origin after its first HTTP 429. Consecutive 429s
/// from the same origin double this (`1s`, `2s`, `4s`, ...) up to
/// [`MAX_RATE_LIMIT_BACKOFF_MS`]. A server-supplied `Retry-After` overrides the
/// exponential value entirely.
const BASE_RATE_LIMIT_BACKOFF_MS: u64 = 1_000;
/// Ceiling for the exponential per-origin rate-limit backoff.
const MAX_RATE_LIMIT_BACKOFF_MS: u64 = 300_000;

/// Discovery fan-out bounds (mirror the pipeline's own limits).
const MAX_SITEMAPS: usize = 8;
const MAX_CHILD_SITEMAPS: usize = 16;
const MAX_FEEDS: usize = 8;
const MAX_DISCOVERED: usize = 512;

/// How many incremental frontier flushes pass between full snapshots. The delta is
/// appended to `frontier.adds` in between, so this bounds its growth without paying the
/// 213 MB snapshot cost on every flush.
const PERSIST_FULL_EVERY: u64 = 150;

/// Default free-space floor for the crawl's disk guard, in GiB.
const DEFAULT_MIN_FREE_GB: usize = 5;

/// Maximum number of document links followed from one seed page. A hard ceiling
/// so a link farm cannot spend the whole attempt budget from a single page; the
/// global `max_urls` bound still applies on top.
pub const MAX_LINKS_PER_PAGE: usize = 200;

/// Upper bound on the HTML bytes [`article_links`] scans. A 5 MB page is
/// truncated here so a pathological body cannot make the linear scan expensive;
/// real pages put their navigation in the first bytes well within this.
const MAX_LINK_SCAN_BYTES: usize = 5_000_000;

/// Tunables for one crawl run.
#[derive(Clone)]
pub struct CrawlOptions {
    /// Path to the seeds file (one URL per line, `#` comments ignored).
    pub seeds_file: PathBuf,
    /// Output directory for `crawl-log.jsonl`, `blobs/` and `frontier.json`.
    pub out_dir: PathBuf,
    /// Write a second copy of every raw body under `out_dir/blobs`.
    ///
    /// Off by default. The pipeline already spills the raw bytes to `<data_dir>/blobs/<sha>`
    /// (`Config::blob_path`), so writing them again into the crawl output stores the corpus
    /// twice: measured 2026-10-28 the fleet held **295 GB under `data_dir/blobs` and another
    /// 45 GB under `raw-dataset/*/blobs`, with zero overlap in the names** — the same bytes,
    /// two hash-keyed directories, and the second one is read by nothing (`import` reads the
    /// index, tests read their own temp dirs). Writing it also gzipped every page on the
    /// crawl's own task. `--keep-raw-blobs` restores the old behaviour for a corpus that is
    /// meant to be shipped as a folder of raw pages.
    pub keep_raw_blobs: bool,
    /// Stop after this many *successfully ingested* documents.
    pub max_docs: usize,
    /// Stop after this many total fetch attempts.
    pub max_urls: usize,
    /// Maximum in-flight fetches across different origins.
    pub concurrency: usize,
    /// Follow same-origin document links found on a seed page (one level deep).
    /// Defaults to `true`; disabled with `--no-follow-links`.
    pub follow_links: bool,
    /// Test-only: exempt loopback targets from the SSRF guard.
    pub allow_loopback: bool,
    /// Minimum interval between two requests to the same origin.
    pub origin_interval: Duration,
    /// Explicit per-origin attempt cap. `None` keeps the derived default of
    /// `max(200, max_docs / 10)`; a corpus run whose goal is *many sources*
    /// rather than depth in a few sets a smaller value so the document budget
    /// spreads across more hosts.
    pub origin_cap: Option<usize>,
    /// Gzip each spilled raw blob. Off by default so an existing raw corpus keeps its shape; on for
    /// a large crawl, where uncompressed raw does not fit: pages average ~237 KB, so a million of
    /// them is ~237 GB raw and roughly a seventh of that gzipped.
    pub compress: bool,
    /// §21 escalation driver. `None` in production unless `SEARCH_BROWSER` asks for one;
    /// tests attach a `MockBrowser` here to exercise the path hermetically.
    pub browser: Option<Arc<dyn BrowserFetcher>>,
    /// Stop the run when the filesystem holding the data root has less free space than this.
    /// `0` disables the guard (the default for tests and for a caller that wants the old
    /// behaviour). Production sets it from `SEARCH_MIN_FREE_GB`, default 5 GiB: a crawl that
    /// fills the disk does not just stop, it takes the serving index's answer path with it —
    /// observed twice on this host, once as `ENOSPC` discarding every generated answer.
    pub min_free_bytes: u64,
}

/// Manual `Debug`: the escalation driver is a trait object, which has no `Debug`; its
/// presence is what matters when reading a run's options.
impl std::fmt::Debug for CrawlOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrawlOptions")
            .field("seeds_file", &self.seeds_file)
            .field("out_dir", &self.out_dir)
            .field("max_docs", &self.max_docs)
            .field("max_urls", &self.max_urls)
            .field("concurrency", &self.concurrency)
            .field("follow_links", &self.follow_links)
            .field("allow_loopback", &self.allow_loopback)
            .field("origin_interval", &self.origin_interval)
            .field("origin_cap", &self.origin_cap)
            .field("compress", &self.compress)
            .field("browser", &self.browser.is_some())
            .finish()
    }
}

impl CrawlOptions {
    /// Production defaults: `raw-dataset/`, 10 000 docs, 64 origins in flight.
    pub fn new(seeds_file: impl Into<PathBuf>) -> Self {
        let max_docs = DEFAULT_MAX_DOCS;
        Self {
            seeds_file: seeds_file.into(),
            out_dir: PathBuf::from(DEFAULT_OUT),
            max_docs,
            max_urls: max_docs.saturating_mul(10),
            concurrency: DEFAULT_CONCURRENCY,
            follow_links: true,
            allow_loopback: false,
            origin_interval: Duration::from_millis(DEFAULT_ORIGIN_INTERVAL_MS),
            origin_cap: None,
            compress: false,
            keep_raw_blobs: false,
            browser: None,
            // Off unless the caller asks: a hermetic test must not depend on the host's disk.
            min_free_bytes: 0,
        }
    }
}

/// Free bytes on the filesystem holding `path`, or `None` when it cannot be read.
///
/// `statvfs` rather than shelling out to `df`: the guard runs inside the crawl loop and must be
/// cheap and allocation-light.
#[cfg(unix)]
pub fn free_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    let c = CString::new(path.to_str()?).ok()?;
    // SAFETY: `statvfs` only writes into the struct we pass; the pointer is valid for the call.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut s) };
    if rc != 0 {
        return None;
    }
    Some(s.f_bavail.saturating_mul(s.f_frsize))
}

/// Free bytes on non-Unix hosts: unknown, so the guard never fires.
#[cfg(not(unix))]
pub fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

/// Whether the disk guard should stop the run.
///
/// Pure so it can be tested without filling a filesystem: `min == 0` disables it, an unreadable
/// filesystem (`None`) never stops a run, and only a *known* free figure below the minimum does.
pub fn should_stop_for_disk(free: Option<u64>, min_free_bytes: u64) -> bool {
    match (free, min_free_bytes) {
        (_, 0) => false,
        (Some(f), min) => f < min,
        (None, _) => false,
    }
}

/// Build the escalation driver when `SEARCH_BROWSER` is truthy.
///
/// Off by default: the real driver launches a Chromium process, which a hermetic test run or a
/// text-only deployment must not pay for. `1`/`true`/`yes` enables it, and only a build compiled
/// with the `chromium` feature can satisfy the request — otherwise the crawl logs why and runs
/// without escalation rather than failing.
async fn browser_from_env() -> Option<Arc<dyn BrowserFetcher>> {
    let wanted = std::env::var("SEARCH_BROWSER")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    if !wanted {
        return None;
    }
    #[cfg(feature = "chromium")]
    {
        match search_browser::ChromiumBrowser::launch().await {
            Ok(b) => Some(Arc::new(b)),
            Err(e) => {
                tracing::warn!(error = %e, "SEARCH_BROWSER is set but Chromium could not launch; \
                    crawling without escalation");
                None
            }
        }
    }
    #[cfg(not(feature = "chromium"))]
    {
        tracing::warn!(
            "SEARCH_BROWSER is set but this build has no `chromium` feature; crawling without \
             escalation (rebuild with `--features chromium`)"
        );
        None
    }
}

/// One JSON line printed at the end of a run.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CrawlSummary {
    pub ingested: usize,
    pub unchanged: usize,
    pub denied: usize,
    pub failed: usize,
    /// Documents marked DELETED this run because their URL now answers 404/410 (§44).
    pub deleted: usize,
    /// Attempts that ended in HTTP 429; the origin was backed off afterwards.
    pub rate_limited: usize,
    /// Attempts that failed DNS resolution; counted and skipped, never retried.
    pub dns_failures: usize,
    pub urls_attempted: usize,
    pub blobs: usize,
    pub bytes: u64,
    pub elapsed_ms: u64,
    pub resumed_from: usize,
    /// True when the run stopped because free disk fell below `min_free_bytes`, rather than
    /// because the seed list, the document budget or the URL budget was exhausted. A caller
    /// (the watcher, a supervisor) can restart the run once space is reclaimed.
    pub stopped_for_disk: bool,
}

// ---------------------------------------------------------------------------
// CLI entry point
// ---------------------------------------------------------------------------

/// `agent0 crawl <seedsfile> [--max-docs N] [--concurrency C] [--out DIR] [--max-urls N]`
pub async fn run_crawl(args: &[String]) -> ExitCode {
    let seeds = match args.get(2) {
        Some(s) if !s.trim().is_empty() => s.clone(),
        _ => {
            eprintln!(
                "usage: agent0 crawl <seedsfile> [--max-docs N] [--concurrency C] \
                 [--out DIR] [--max-urls N] [--origin-cap N] [--no-follow-links]"
            );
            return ExitCode::from(2);
        }
    };

    let max_docs = flag_usize(args, "--max-docs").unwrap_or(DEFAULT_MAX_DOCS);
    // `--concurrency` wins; otherwise `SEARCH_CRAWL_CONCURRENCY`; otherwise the
    // default. Always at least 1.
    let concurrency = resolve_concurrency(
        flag_usize(args, "--concurrency"),
        std::env::var("SEARCH_CRAWL_CONCURRENCY").ok(),
    );
    let out = flag_str(args, "--out").unwrap_or_else(|| DEFAULT_OUT.to_string());
    let max_urls = flag_usize(args, "--max-urls").unwrap_or_else(|| max_docs.saturating_mul(10));
    // Link following is on by default; `--no-follow-links` turns it off.
    let follow_links = !args.iter().any(|a| a == "--no-follow-links");
    // Breadth knob: the derived cap (max(200, max_docs/10)) suits a depth-first
    // run; a corpus run that wants hundreds of sources spreads the same budget
    // further with a smaller cap.
    let origin_cap = flag_usize(args, "--origin-cap").or_else(|| {
        std::env::var("SEARCH_CRAWL_ORIGIN_CAP")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
    });
    let interval_ms = std::env::var("SEARCH_CRAWL_ORIGIN_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_ORIGIN_INTERVAL_MS);
    let keep_raw_blobs = args.iter().any(|a| a == "--keep-raw-blobs");
    let compress = args.iter().any(|a| a == "--compress")
        || matches!(
            std::env::var("SEARCH_CRAWL_COMPRESS").unwrap_or_default().trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        );

    let opts = CrawlOptions {
        seeds_file: PathBuf::from(seeds),
        out_dir: PathBuf::from(out),
        max_docs,
        max_urls,
        concurrency,
        follow_links,
        allow_loopback: false,
        origin_interval: Duration::from_millis(interval_ms),
        origin_cap,
        compress,
        keep_raw_blobs,
        browser: browser_from_env().await,
        min_free_bytes: resolve_min_free_bytes(flag_usize(args, "--min-free-gb")),
    };

    let cfg = Config::from_env();
    match crawl(opts, &cfg).await {
        Ok(summary) => {
            println!("{}", serde_json::to_string(&summary).unwrap());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("crawl failed: {e}");
            ExitCode::from(1)
        }
    }
}

/// The disk guard's threshold in bytes.
///
/// Precedence: `--min-free-gb`, then `SEARCH_MIN_FREE_GB`, then 5 GiB. `0` disables the guard.
/// A non-numeric value is ignored rather than fatal — a bad setting must not stop a crawl that
/// would otherwise run.
fn resolve_min_free_bytes(flag_gb: Option<usize>) -> u64 {
    let gb = flag_gb
        .or_else(|| {
            std::env::var("SEARCH_MIN_FREE_GB")
                .ok()
                .and_then(|v| v.trim().parse::<usize>().ok())
        })
        .unwrap_or(DEFAULT_MIN_FREE_GB);
    (gb as u64).saturating_mul(1024 * 1024 * 1024)
}

fn flag_usize(args: &[String], flag: &str) -> Option<usize> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<usize>().ok())
}

fn flag_str(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .filter(|v| !v.trim().is_empty())
        .cloned()
}

/// Resolve the fleet-wide in-flight fetch bound. Precedence: `--concurrency`
/// flag, then `SEARCH_CRAWL_CONCURRENCY`, then [`DEFAULT_CONCURRENCY`]. A
/// non-numeric or absent env value is ignored; the result is never below 1.
fn resolve_concurrency(flag: Option<usize>, env: Option<String>) -> usize {
    flag.or_else(|| env.and_then(|v| v.trim().parse::<usize>().ok()))
        .unwrap_or(DEFAULT_CONCURRENCY)
        .max(1)
}

/// Seeds to discover in parallel for one chunk.
///
/// `SEARCH_CRAWL_DISCOVERY_WINDOW` sets the absolute window; otherwise the
/// fetch concurrency is widened by [`DISCOVERY_WINDOW_FACTOR`]. Never below 1,
/// so a crawl always makes progress.
fn discovery_window(concurrency: usize, env: Option<String>) -> usize {
    env.and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| {
            concurrency
                .max(1)
                .saturating_mul(DISCOVERY_WINDOW_FACTOR)
                .min(MAX_DISCOVERY_WINDOW)
        })
        .max(1)
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

/// Run one crawl to completion, returning the summary. Library entry point used
/// directly by tests (which pass a hermetic `Config` + loopback fixture).
pub async fn crawl(opts: CrawlOptions, cfg: &Config) -> AppResult<CrawlSummary> {
    let started = Instant::now();

    std::fs::create_dir_all(&opts.out_dir)?;
    let blobs_dir = opts.out_dir.join("blobs");
    std::fs::create_dir_all(&blobs_dir)?;
    let log_path = opts.out_dir.join("crawl-log.jsonl");
    let frontier_path = opts.out_dir.join("frontier.json");

    // Resume: every URL already in the log counts as done.
    let seen = read_log_urls(&log_path)?;
    let resumed_from = seen.len();

    let mut pipeline = Pipeline::open_with(cfg, opts.allow_loopback)?;
    if let Some(browser) = opts.browser.clone() {
        tracing::info!("browser escalation enabled for this crawl");
        pipeline.set_browser(browser);
    }
    let pipeline = Arc::new(pipeline);
    // Bulk crawl: buffer index docs and commit once per batch instead of once
    // per page. A per-document Tantivy commit + manifest scan measured ~2 s, so
    // the crawl was commit-bound and the box idle; the tail is flushed below.
    pipeline.enable_overlay_batching();

    // The fetcher used ONLY for discovery bodies (robots/sitemaps/feeds). It is
    // the same `Fetcher` type the pipeline holds; the pipeline does not expose
    // its instance, so one is built here. `fetch` re-validates SSRF on every
    // hop; the loopback escape is enabled only for the hermetic test path.
    let fetcher = Arc::new(
        Fetcher::new(cfg.limits.clone(), cfg.user_agent.clone())
            .with_allowed_types(vec![
                "text/*".to_string(),
                "application/xml".to_string(),
                "application/rss+xml".to_string(),
                "application/atom+xml".to_string(),
                "application/xhtml+xml".to_string(),
                "application/json".to_string(),
            ])
            .allow_private_addresses_for_tests(opts.allow_loopback),
    );

    let mut drain = Drainer::new(
        pipeline.clone(),
        cfg.clone(),
        opts.clone(),
        seen,
        log_path,
        blobs_dir,
        frontier_path,
    );

    let seeds = parse_seeds(&opts.seeds_file)?;

    // Discovery and draining run together, streamed seed by seed.
    //
    // The old shape discovered a whole chunk and only drained it once every task
    // in the chunk had finished, so the drain sat idle behind the slowest of
    // hundreds of discovery chains (and widening the window made that tail
    // longer, not shorter). Here each seed's discovery result is enqueued the
    // moment its task completes and the freed window slot is immediately refilled
    // with the next seed, so the network-bound discovery and the CPU-bound drain
    // overlap continuously and the concurrency slots stay full.
    //
    // This runs BEFORE the resumed backlog on purpose. The pending frontier
    // left by a previous run can be dominated by one host's sitemap (a live
    // run had 2 355 pending docs.rs URLs against 1 203 from everything else);
    // draining it first parked the whole run inside a single origin and the
    // seeds listed below were never even discovered. Seeds first makes a
    // resumed crawl breadth-first; the stale backlog goes last, and the
    // document bound normally stops the run before reaching it.
    let window = discovery_window(
        opts.concurrency,
        std::env::var("SEARCH_CRAWL_DISCOVERY_WINDOW").ok(),
    );
    let follow_links = opts.follow_links;
    let mut next_seed = 0usize;
    let mut discovery: JoinSet<(String, Vec<String>)> = JoinSet::new();
    let mut drain_set: JoinSet<PerUrl> = JoinSet::new();

    // Prime the window; the tasks discover while this task reaps and drains.
    fill_discovery(
        &mut discovery,
        &seeds,
        &mut next_seed,
        window,
        &pipeline,
        &fetcher,
        follow_links,
        &drain.seen,
    );

    // Disk-guard bookkeeping: a counter so `statvfs` runs every 32 turns, and the flag the
    // summary reports so a supervisor can tell "budget exhausted" from "stopped to save the disk".
    let mut loop_ticks: u64 = 0;
    let mut stopped_for_disk = false;

    loop {
        // Stream every completed discovery task: enqueue its URLs and seed, then
        // refill the window so discovery never ramps down between batches.
        while let Some(res) = discovery.try_join_next() {
            if let Ok((seed, urls)) = res {
                for u in urls {
                    drain.enqueue(u);
                }
                drain.enqueue_seed(seed);
            }
        }
        fill_discovery(
            &mut discovery,
            &seeds,
            &mut next_seed,
            window,
            &pipeline,
            &fetcher,
            follow_links,
            &drain.seen,
        );

        // Reap finished fetches and dispatch new ones (never blocks).
        drain.pump(&mut drain_set);

        // Disk guard: a crawl that fills the filesystem takes the serving index's write path
        // (and, on this host, the answer cache) down with it. Checked every 32 turns of the
        // loop (the first turn is checked so a crawl started on a full disk stops at once) —
        // `statvfs` is cheap, but not free enough to call per dispatch.
        loop_ticks = loop_ticks.wrapping_add(1);
        if opts.min_free_bytes > 0 && loop_ticks % 32 == 1 {
            let probe = if cfg.data_dir.exists() { &cfg.data_dir } else { &opts.out_dir };
            if should_stop_for_disk(free_bytes(probe), opts.min_free_bytes) {
                tracing::warn!(
                    path = %probe.display(),
                    min_free_bytes = opts.min_free_bytes,
                    "stopping the crawl: free disk is below the configured minimum"
                );
                stopped_for_disk = true;
                break;
            }
        }

        // The seed list is exhausted, discovery has quiesced and everything it
        // produced has drained.
        if next_seed >= seeds.len()
            && discovery.is_empty()
            && drain.queues_empty()
            && drain_set.is_empty()
        {
            break;
        }
        if drain.bound_reached() {
            break;
        }

        // Block until the next event so neither discovery nor the drain can idle
        // the task while the other has work.
        if !discovery.is_empty() {
            tokio::select! {
                Some(res) = discovery.join_next() => {
                    if let Ok((seed, urls)) = res {
                        for u in urls {
                            drain.enqueue(u);
                        }
                        drain.enqueue_seed(seed);
                    }
                }
                Some(res) = drain_set.join_next(), if !drain_set.is_empty() => {
                    drain.handle(res);
                    drain.persist();
                }
            }
        } else if !drain_set.is_empty() {
            if let Some(res) = drain_set.join_next().await {
                drain.handle(res);
                drain.persist();
            }
        } else {
            // Nothing in flight: queued URLs are waiting out the per-origin
            // interval or a 429 backoff, or the remaining seeds were all seen.
            // Nap briefly rather than spin; the next pass reaps or dispatches.
            let d = drain.next_ready_delay();
            let nap = if d.is_zero() {
                Duration::from_millis(5)
            } else {
                d.min(Duration::from_millis(250))
            };
            tokio::time::sleep(nap).await;
        }
    }

    // Reap any fetch still in flight when a bound ended the run, so its log line
    // is written rather than dropped with the task set.
    while let Some(res) = drain_set.join_next().await {
        drain.handle(res);
    }

    // The pending frontier from a previous run, drained last: everything in it
    // is already queued, and the bound is usually reached first.
    for u in read_frontier(&drain.frontier_path) {
        drain.enqueue(u);
    }
    // The delta written since the last full snapshot. A clean exit leaves no delta (see
    // `Drainer::final_persist`); after a kill it holds exactly the URLs enqueued since
    // the last snapshot, which is what makes the 213 MB rewrite unnecessary.
    for u in read_frontier_adds(&adds_path(&drain.frontier_path)) {
        drain.enqueue(u);
    }

    // Final drain for anything still queued (no-op when the bound was met).
    if !drain.bound_reached() {
        drain.run().await;
    }

    // Commit + publish the final partial batch so the last documents are
    // searchable and survive a resume (see `Pipeline::enable_overlay_batching`).
    pipeline.flush_overlay_batch()?;

    // Leave an accurate frontier: a crawl that stopped because it hit `--max-docs` has
    // thousands of URLs still pending, and clearing the file would make the next run
    // rediscover them from scratch. A run that genuinely emptied its queue writes []
    // because `remaining` is empty.
    drain.final_persist();

    Ok(drain.summary(resumed_from, started.elapsed(), stopped_for_disk))
}

// ---------------------------------------------------------------------------
// Scheduler + drain loop
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Counters {
    ingested: usize,
    unchanged: usize,
    denied: usize,
    failed: usize,
    /// §44: documents tombstoned because their URL answered 404/410 after being indexed.
    deleted: usize,
    /// HTTP 429 responses seen (each one triggers a per-origin backoff).
    rate_limited: usize,
    /// DNS resolution failures (permanent for this run; skipped, not retried).
    dns_failures: usize,
    blobs: usize,
    bytes: u64,
}

/// One dispatched URL's result, ready to be logged.
struct PerUrl {
    url: String,
    origin: String,
    final_url: String,
    outcome: &'static str,
    error: Option<String>,
    sha256: Option<String>,
    bytes: u64,
    content_type: Option<String>,
    /// Raw fetched bytes for the spill (present for ingested/unchanged).
    raw: Option<Vec<u8>>,
    /// The origin answered HTTP 429: back it off before dispatching to it again.
    rate_limited: bool,
    /// `Retry-After` seconds from the 429, when the server supplied one.
    retry_after: Option<u64>,
    /// The failure was a DNS resolution error (counted, not retried).
    dns_failure: bool,
}

/// Owns the work queue, the per-origin politeness state and the counters.
/// Concurrency is across origins; at most one in-flight request per origin.
struct Drainer {
    pipeline: Arc<Pipeline>,
    cfg: Config,
    opts: CrawlOptions,
    log_path: PathBuf,
    blobs_dir: PathBuf,
    frontier_path: PathBuf,
    /// URLs already logged (previous runs) — never re-attempted.
    seen: HashSet<String>,
    queues: HashMap<String, VecDeque<String>>,
    order: Vec<String>,
    busy: HashSet<String>,
    last: HashMap<String, Instant>,
    rr: usize,
    in_flight: usize,
    /// Attempts charged per origin; a host at `origin_cap()` is retired so no
    /// single sitemap can consume the whole budget (see `dispatch`).
    origin_attempts: HashMap<String, usize>,
    /// Per-origin wall-clock before which no request may be dispatched, set
    /// after an HTTP 429. This only ever *adds* delay on top of
    /// `opts.origin_interval`; it never shortens the configured floor.
    backoff_until: HashMap<String, Instant>,
    /// Consecutive 429s per origin, driving the exponential backoff. Cleared by
    /// any healthy (`ingested`/`unchanged`) response from that origin.
    backoff_level: HashMap<String, u32>,
    attempts: usize,
    ingested: usize,
    /// Every URL enqueued-but-not-yet-logged (queued + in-flight); persisted to
    /// `frontier.json` so a kill loses nothing.
    remaining: BTreeSet<String>,
    /// Last time the pending frontier was written. The frontier is rewritten in
    /// full, so doing it after every completion is O(N) per document and O(N²)
    /// over a run; it is only a resumability hint (the log is the source of
    /// truth for done URLs), so it is throttled.
    last_persist: Instant,
    /// `remaining` URLs added since the last full snapshot. Resumability only needs
    /// "what is still pending", so the steady state is an append of the *new* URLs to
    /// `frontier.adds` rather than a rewrite of the whole 200 MB set; the snapshot is
    /// written rarely (see [`PERSIST_FULL_EVERY`]) and at exit.
    pending_adds: Vec<String>,
    /// Full snapshots written so far; drives the periodic compaction of `pending_adds`.
    full_snapshots: u64,
    counters: Counters,
}

impl Drainer {
    #[allow(clippy::too_many_arguments)]
    fn new(
        pipeline: Arc<Pipeline>,
        cfg: Config,
        opts: CrawlOptions,
        seen: HashSet<String>,
        log_path: PathBuf,
        blobs_dir: PathBuf,
        frontier_path: PathBuf,
    ) -> Self {
        Self {
            pipeline,
            cfg,
            opts,
            log_path,
            blobs_dir,
            frontier_path,
            seen,
            queues: HashMap::new(),
            order: Vec::new(),
            busy: HashSet::new(),
            last: HashMap::new(),
            rr: 0,
            in_flight: 0,
            origin_attempts: HashMap::new(),
            backoff_until: HashMap::new(),
            backoff_level: HashMap::new(),
            attempts: 0,
            ingested: 0,
            remaining: BTreeSet::new(),
            last_persist: Instant::now(),
            pending_adds: Vec::new(),
            full_snapshots: 0,
            counters: Counters::default(),
        }
    }

    fn enqueue(&mut self, url: String) {
        if !url.starts_with("http") {
            return;
        }
        // A profile/utility/asset URL can never be indexed: rejecting it here
        // keeps the attempt budget for documents (see `is_low_value`).
        if is_low_value(&url) {
            return;
        }
        if self.seen.contains(&url) || !self.remaining.insert(url.clone()) {
            return;
        }
        // Resumability: this URL is pending and must survive a kill. Recorded as a delta
        // rather than by rewriting the whole snapshot (see `Drainer::persist`).
        self.pending_adds.push(url.clone());
        let o = origin_of(&url);
        if !self.queues.contains_key(&o) {
            self.order.push(o.clone());
        }
        let q = self.queues.entry(o).or_default();
        // Ranked documents drain before generic pages within an origin.
        if url_rank(&url) > 0 {
            q.push_front(url);
        } else {
            q.push_back(url);
        }
    }

    /// Enqueue an operator-provided seed. Seeds bypass the low-value gate: the
    /// user asked for this URL explicitly.
    fn enqueue_seed(&mut self, url: String) {
        if !url.starts_with("http") || self.seen.contains(&url) || !self.remaining.insert(url.clone())
        {
            return;
        }
        self.pending_adds.push(url.clone());
        let o = origin_of(&url);
        if !self.queues.contains_key(&o) {
            self.order.push(o.clone());
        }
        let q = self.queues.entry(o).or_default();
        if url_rank(&url) > 0 {
            q.push_front(url);
        } else {
            q.push_back(url);
        }
    }

    /// Per-origin attempt cap. The derived default is one tenth of the document
    /// goal, never below 200, which keeps the corpus broad instead of deep in
    /// one host; `opts.origin_cap` overrides it so a breadth-first run can
    /// spread a 10 000-document budget across hundreds of hosts.
    fn origin_cap(&self) -> usize {
        self.opts
            .origin_cap
            .unwrap_or_else(|| std::cmp::max(200, self.opts.max_docs / 10))
    }

    fn bound_reached(&self) -> bool {
        self.attempts >= self.opts.max_urls || self.ingested >= self.opts.max_docs
    }


    fn queues_empty(&self) -> bool {
        self.queues.values().all(|q| q.is_empty())
    }

    /// Time until the earliest backed-off origin becomes dispatchable again.
    /// Zero when nothing is in rate-limit backoff.
    fn next_ready_delay(&self) -> Duration {
        let now = Instant::now();
        let mut soonest: Option<Duration> = None;
        for until in self.backoff_until.values() {
            if *until > now {
                let d = *until - now;
                soonest = Some(soonest.map_or(d, |s| s.min(d)));
            }
        }
        soonest.unwrap_or(Duration::ZERO)
    }

    fn persist(&mut self) {
        // The frontier is a resumability hint, not a correctness boundary: every
        // terminal URL is already in the log, and an un-logged URL that falls out
        // of a stale frontier is simply rediscovered.
        //
        // Measured 2026-10-28: at 2.9M pending URLs the snapshot is 213 MB of JSON,
        // and serialising it takes 0.7 s. Retried every 2 s that is 107 MB/s of JSON
        // generation per shard — 427 MB/s across four shards, on the crawl's own event
        // loop, which is why the box sat at 8% CPU and 5,300 attempts/hour. The
        // snapshot is now the exception: the steady state appends only the URLs added
        // since the last one, which is proportional to *new work*, not to queue depth.
        let now = Instant::now();
        if now.saturating_duration_since(self.last_persist) < Duration::from_secs(2) {
            return;
        }
        self.last_persist = now;
        if self.pending_adds.is_empty() {
            return;
        }
        // Every `PERSIST_FULL_EVERY`-th flush (and, via `final_persist`, at exit) writes
        // the whole snapshot and truncates the delta, so the delta cannot grow without
        // bound over a long run.
        self.full_snapshots = self.full_snapshots.wrapping_add(1);
        if self.full_snapshots % PERSIST_FULL_EVERY == 0 {
            persist_frontier(&self.frontier_path, self.remaining.iter().cloned());
            self.pending_adds.clear();
            let _ = std::fs::remove_file(adds_path(&self.frontier_path));
        } else {
            append_frontier_adds(&adds_path(&self.frontier_path), &self.pending_adds);
            self.pending_adds.clear();
        }
    }

    /// Write the full snapshot unconditionally. Called once as the crawl exits, so a
    /// clean shutdown leaves a complete `frontier.json` and no delta behind.
    fn final_persist(&mut self) {
        persist_frontier(&self.frontier_path, self.remaining.iter().cloned());
        // The snapshot now contains every pending URL, so the delta is redundant. A
        // failure to remove it is harmless: the resume reader unions both.
        let _ = std::fs::remove_file(adds_path(&self.frontier_path));
        self.pending_adds.clear();
    }

    fn summary(&self, resumed_from: usize, elapsed: Duration, stopped_for_disk: bool) -> CrawlSummary {
        CrawlSummary {
            ingested: self.counters.ingested,
            unchanged: self.counters.unchanged,
            denied: self.counters.denied,
            failed: self.counters.failed,
            deleted: self.counters.deleted,
            rate_limited: self.counters.rate_limited,
            dns_failures: self.counters.dns_failures,
            urls_attempted: self.attempts,
            blobs: self.counters.blobs,
            bytes: self.counters.bytes,
            elapsed_ms: elapsed.as_millis() as u64,
            resumed_from,
            stopped_for_disk,
        }
    }

    /// One non-blocking scheduler pass: reap every completed fetch, then
    /// dispatch as many new ones as the concurrency + per-origin politeness
    /// allow. Returns how many completions were handled. Never awaits, so a
    /// caller driving discovery and draining together can interleave the two.
    fn pump(&mut self, joinset: &mut JoinSet<PerUrl>) -> usize {
        let mut handled = 0;
        while let Some(res) = joinset.try_join_next() {
            self.handle(res);
            self.persist();
            handled += 1;
        }
        if !self.bound_reached() {
            self.dispatch(joinset);
        }
        handled
    }

    /// Drain the queue until it is empty or a bound is reached.
    async fn run(&mut self) {
        let mut joinset: JoinSet<PerUrl> = JoinSet::new();
        loop {
            self.pump(&mut joinset);
            if joinset.is_empty() {
                if self.queues_empty() || self.bound_reached() {
                    break;
                }
                // Nothing dispatchable right now: the per-origin interval has
                // not elapsed, or every eligible origin is in 429 backoff.
                // Sleep until the earliest backoff lifts (bounded) rather
                // than spinning.
                let d = self.next_ready_delay();
                let nap = if d.is_zero() {
                    Duration::from_millis(5)
                } else {
                    d.min(Duration::from_millis(250))
                };
                tokio::time::sleep(nap).await;
                continue;
            }
            if let Some(res) = joinset.join_next().await {
                self.handle(res);
                self.persist();
            }
        }
        while let Some(res) = joinset.join_next().await {
            self.handle(res);
        }
    }

    /// Dispatch as many eligible URLs as the concurrency + per-origin politeness
    /// allow. Returns how many tasks were spawned. Never blocks.
    fn dispatch(&mut self, joinset: &mut JoinSet<PerUrl>) -> usize {
        let interval = self.opts.origin_interval;
        let concurrency = self.opts.concurrency;
        let mut spawned = 0;
        loop {
            if self.in_flight >= concurrency
                || self.attempts >= self.opts.max_urls
                || self.ingested >= self.opts.max_docs
                || self.order.is_empty()
            {
                break;
            }
            let now = Instant::now();
            let n = self.order.len();
            let mut chosen: Option<String> = None;
            for k in 0..n {
                let o = &self.order[(self.rr + k) % n];
                if self.busy.contains(o) {
                    continue;
                }
                match self.queues.get(o) {
                    Some(q) if !q.is_empty() => {}
                    _ => continue,
                }
                // Fairness: a single host (a giant sitemap) must not absorb the
                // whole attempt budget. Once an origin reaches its cap its
                // queue is dropped, so the remaining sources get the budget —
                // the Stage-1 corpus wants many sources, not one deep host.
                if self.origin_attempts.get(o).copied().unwrap_or(0) >= self.origin_cap() {
                    if let Some(q) = self.queues.get_mut(o) {
                        for u in q.drain(..) {
                            self.remaining.remove(&u);
                        }
                    }
                    continue;
                }
                if let Some(t) = self.last.get(o) {
                    if now.saturating_duration_since(*t) < interval {
                        continue;
                    }
                }
                // Rate-limit backoff: an origin that answered 429 is skipped
                // until its delay elapses. The slot then goes to whichever other
                // origin the round-robin reaches — that is the point.
                if let Some(until) = self.backoff_until.get(o) {
                    if now < *until {
                        continue;
                    }
                }
                chosen = Some(o.clone());
                self.rr = (self.rr + k + 1) % n;
                break;
            }
            let Some(o) = chosen else { break };

            let url = self.queues.get_mut(&o).unwrap().pop_front().unwrap();
            self.busy.insert(o.clone());
            self.last.insert(o.clone(), now);
            *self.origin_attempts.entry(o.clone()).or_insert(0) += 1;
            self.in_flight += 1;
            self.attempts += 1;
            spawned += 1;

            let pl: Arc<Pipeline> = self.pipeline.clone();
            let c = self.cfg.clone();
            joinset.spawn(async move { attempt(pl, c, url, o).await });
        }
        spawned
    }

    /// Apply one completed task: counters, blob spill, log append.
    fn handle(&mut self, res: Result<PerUrl, tokio::task::JoinError>) {
        let per = match res {
            Ok(p) => p,
            Err(e) => {
                // A panicked task: count it as a failure; no URL to log.
                self.in_flight = self.in_flight.saturating_sub(1);
                self.counters.failed += 1;
                eprintln!("crawl task panicked: {e}");
                return;
            }
        };

        self.in_flight = self.in_flight.saturating_sub(1);
        self.busy.remove(&per.origin);
        self.remaining.remove(&per.url);
        // Mark as done so a later frontier re-read never re-queues it.
        self.seen.insert(per.url.clone());

        match per.outcome {
            "ingested" => {
                self.counters.ingested += 1;
                self.ingested += 1;
            }
            "unchanged" => self.counters.unchanged += 1,
            "denied" => self.counters.denied += 1,
            "failed" => self.counters.failed += 1,
            "deleted" => self.counters.deleted += 1,
            _ => {}
        }

        // Per-origin rate-limit backoff. An origin that answered 429 must not be
        // asked again until its delay elapses; because dispatch round-robins,
        // the freed slot moves to another origin instead of hammering this one.
        // A healthy response clears the origin's backoff entirely. This never
        // shortens `opts.origin_interval`, so the configured politeness floor is
        // preserved (backoff only adds delay).
        if per.rate_limited {
            let level = self.backoff_level.entry(per.origin.clone()).or_insert(0);
            *level = level.saturating_add(1);
            let delay = rate_limit_backoff(per.retry_after, *level);
            self.backoff_until
                .insert(per.origin.clone(), Instant::now() + delay);
            self.counters.rate_limited += 1;
            tracing::warn!(
                target: "agent0::crawl",
                origin = %per.origin,
                delay_ms = delay.as_millis() as u64,
                level = *level,
                "HTTP 429: backing off this origin"
            );
        } else if per.outcome == "ingested" || per.outcome == "unchanged" {
            self.backoff_level.remove(&per.origin);
            self.backoff_until.remove(&per.origin);
        }
        if per.dns_failure {
            self.counters.dns_failures += 1;
            tracing::debug!(
                target: "agent0::crawl",
                url = %per.url,
                "DNS resolution failed; counted and skipped"
            );
        }

        // Raw spill: dedupe by sha256. With `--compress` the blob is gzipped: raw pages average
        // ~237 KB, so a million of them is ~237 GB uncompressed — which is the difference between a
        // large corpus fitting on this disk and not. The sha256 in the name stays the hash of the
        // *uncompressed* bytes, so dedup and the crawl log mean the same thing either way.
        // Off by default: the pipeline already spilled these bytes to `data_dir/blobs`
        // (see `CrawlOptions::keep_raw_blobs`). Writing them again doubled the corpus on disk
        // and ran a gzip on the crawl loop for a directory nothing reads.
        if self.opts.keep_raw_blobs {
        if let (Some(raw), Some(sha)) = (&per.raw, &per.sha256) {
            let ext = ext_for(per.content_type.as_deref());
            let name = if self.opts.compress {
                format!("{sha}.{ext}.gz")
            } else {
                format!("{sha}.{ext}")
            };
            let path = self.blobs_dir.join(&name);
            if !path.exists() {
                let payload: Vec<u8> = if self.opts.compress {
                    match gzip_bytes(raw) {
                        Ok(gz) => gz,
                        Err(e) => {
                            eprintln!("failed to gzip blob {sha}: {e}; storing it uncompressed");
                            raw.clone()
                        }
                    }
                } else {
                    raw.clone()
                };
                if let Err(e) = write_atomic(&path, &payload) {
                    eprintln!("failed to write blob {sha}: {e}");
                } else {
                    self.counters.blobs += 1;
                    self.counters.bytes += payload.len() as u64;
                }
            }
        }
        }

        let line = serde_json::json!({
            "url": per.url,
            "final_url": per.final_url,
            "sha256": per.sha256,
            "bytes": per.bytes,
            "content_type": per.content_type,
            "fetched_at": search_core::time::now_unix_s(),
            "outcome": per.outcome,
            "error": per.error,
        });
        if let Err(e) = append_log_line(&self.log_path, &line.to_string()) {
            eprintln!("failed to append crawl log line: {e}");
        }
    }
}

/// Run one URL through the existing ingest path and package the result.
async fn attempt(pipeline: Arc<Pipeline>, cfg: Config, url: String, origin: String) -> PerUrl {
    match pipeline.crawl_one(&url).await {
        Ok(outcome) => {
            let (name, content_id, error) = match &outcome {
                IngestOutcome::Ingested { content_id, .. } => {
                    ("ingested", Some(content_id.clone()), None)
                }
                IngestOutcome::Unchanged { content_id, .. } => {
                    ("unchanged", Some(content_id.clone()), None)
                }
                IngestOutcome::Denied { reason, .. } => ("denied", None, Some(reason.clone())),
                IngestOutcome::Routed { .. } => ("routed", None, None),
            };
            let final_url = outcome_url(&outcome).unwrap_or_else(|| url.clone());

            let mut per = PerUrl {
                url: url.clone(),
                origin,
                final_url,
                outcome: name,
                error,
                sha256: None,
                bytes: 0,
                content_type: None,
                raw: None,
                rate_limited: false,
                retry_after: None,
                dns_failure: false,
            };

            if let Some(cid) = content_id {
                if let Some(raw) = read_pipeline_bytes(&cfg, &cid) {
                    let sha = sha256_hex(&raw);
                    let ct = sniff_media_type(&per.final_url, &raw);
                    per.bytes = raw.len() as u64;
                    per.content_type = Some(ct.to_string());
                    per.sha256 = Some(sha);
                    per.raw = Some(raw);
                }
            }
            per
        }
        Err(e) => {
            // §44 deletion detection. A 404/410 for a URL that is already indexed means the
            // resource is gone: mark the document DELETED so it stops being served, keeping its
            // stored bytes and history (the architecture is explicit that historical results may
            // still matter, so this is not a hard delete). A 404 for a URL never indexed stays an
            // ordinary failed attempt.
            let not_found = matches!(e, search_core::error::AppError::NotFound(_));
            let deleted = not_found
                && pipeline.mark_deleted(&url).unwrap_or_else(|err| {
                    eprintln!("deletion detection: could not mark {url} deleted: {err}");
                    false
                });
            // A 429 is not a failure of the resource; it is the origin asking us
            // to slow down. Capture its `Retry-After` so the scheduler can honour
            // it, and count DNS failures (permanent for this run) rather than
            // letting them churn.
            let (rate_limited, retry_after) = match &e {
                search_core::error::AppError::RateLimited {
                    retry_after_secs, ..
                } => (true, *retry_after_secs),
                _ => (false, None),
            };
            let dns_failure = !rate_limited && is_dns_failure(&e.to_string());
            PerUrl {
                url: url.clone(),
                origin,
                final_url: url,
                outcome: if deleted { "deleted" } else { "failed" },
                error: Some(e.to_string()),
                sha256: None,
                bytes: 0,
                content_type: None,
                raw: None,
                rate_limited,
                retry_after,
                dns_failure,
            }
        }
    }
}

fn outcome_url(o: &IngestOutcome) -> Option<String> {
    match o {
        IngestOutcome::Ingested { url, .. }
        | IngestOutcome::Unchanged { url, .. }
        | IngestOutcome::Denied { url, .. }
        | IngestOutcome::Routed { url, .. } => Some(url.clone()),
    }
}

/// Read the exact raw bytes the pipeline persisted for a content id.
///
/// Seam: `crawl_one` -> `write_content_files` writes the fetched body to
/// `cfg.blob_path(<content-hash>)`; the content id is `cnt_<hex>`.
///
/// Falls back to the normalized text when the blob is gone: a document that is
/// still in the index (an `Unchanged` re-fetch, or an earlier run that only
/// kept text) must still be represented in the raw dataset, otherwise the
/// dataset silently under-covers the index.
pub fn read_pipeline_bytes(cfg: &Config, content_id: &str) -> Option<Vec<u8>> {
    let hex = content_id.strip_prefix("cnt_").unwrap_or(content_id);
    if hex.len() < 2 {
        return None;
    }
    if let Ok(raw) = std::fs::read(cfg.blob_path(hex)) {
        return Some(raw);
    }
    std::fs::read(cfg.text_path(hex)).ok()
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Top up the discovery window: start discovery for unseen seeds until
/// `window` tasks are in flight (or the seed list is exhausted).
///
/// Each task owns a distinct origin, so the window discovers many hosts
/// concurrently without ever making two simultaneous requests to the same host.
/// A task returns `(seed, urls)`; the caller enqueues both the moment the task
/// completes, so discovery and draining overlap continuously instead of the
/// drain waiting for a whole batch.
#[allow(clippy::too_many_arguments)]
fn fill_discovery(
    discovery: &mut JoinSet<(String, Vec<String>)>,
    seeds: &[String],
    next: &mut usize,
    window: usize,
    pipeline: &Arc<Pipeline>,
    fetcher: &Arc<Fetcher>,
    follow_links: bool,
    seen: &HashSet<String>,
) {
    while *next < seeds.len() && discovery.len() < window {
        let seed = seeds[*next].clone();
        *next += 1;
        if seen.contains(&seed) {
            continue;
        }
        let pl = pipeline.clone();
        let ft = fetcher.clone();
        let seed_for_task = seed.clone();
        discovery.spawn(async move {
            let urls = discover_seed_urls(&pl, &ft, &seed_for_task, follow_links).await;
            (seed_for_task, urls)
        });
    }
}

/// Expand one seed and return every URL it discovered (the seed itself is not
/// included; the caller enqueues it). Runs the existing pipeline discovery path
/// plus a direct sitemap/feed re-parse, and follows the seed page's own
/// same-origin document links one level deep.
async fn discover_seed_urls(
    pipeline: &Pipeline,
    fetcher: &Fetcher,
    seed: &str,
    follow_links: bool,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    // (1) Existing discovery path: robots -> sitemaps -> feeds, persisted and
    // pushed to the metadata frontier. Errors are non-fatal.
    if let Err(e) = pipeline.discover(seed).await {
        tracing::warn!(target: "agent0::crawl", seed = %seed, error = %e, "discovery pass failed (continuing)");
    }

    // (2) Direct sitemap expansion from the origin's robots.txt.
    let origin = origin_of(seed);
    if !origin.is_empty() {
        let robots_url = format!("{origin}/robots.txt");
        if let Some(bytes) = fetch_body(fetcher, &robots_url).await {
            let text = String::from_utf8_lossy(&bytes).to_string();
            let sitemaps: Vec<String> = sitemaps_in_robots(&text)
                .into_iter()
                .map(|u| resolve_against(&origin, &u))
                .filter(|u| u.starts_with("http"))
                .take(MAX_SITEMAPS)
                .collect();
            for sm in sitemaps {
                out.extend(expand_sitemap(fetcher, &origin, &sm).await);
            }
        }
    }

    // (2b) Follow document links published on the seed page itself.
    //
    // One level only: discovery runs for seeds (and frontier entries), never for
    // pages reached by following a link, so a followed page is fetched and
    // indexed by the normal drain but its own links are never expanded.
    //
    // `Pipeline::discover` already fetched this page for feed discovery but does
    // not surface the body (pipeline.rs is out of scope), so the seed HTML is
    // fetched once more here with the module's discovery `Fetcher`.
    if follow_links {
        if let Some(bytes) = fetch_body(fetcher, seed).await {
            let html = String::from_utf8_lossy(&bytes).to_string();
            let seed_site = site_key(seed);
            for link in article_links(seed, &html, MAX_LINKS_PER_PAGE) {
                // Same-origin only, with `www.`/scheme normalisation (see
                // `site_key`).
                if site_key(&link) == seed_site {
                    out.push(link);
                }
            }
        }
    }

    // (3) Feeds the pipeline persisted for this seed's domain, re-parsed.
    for feed_url in metadata_urls(pipeline, "SELECT url FROM feed LIMIT ?1", MAX_FEEDS as i64) {
        if let Some(bytes) = fetch_body(fetcher, &feed_url).await {
            let entries = parse_feed(&bytes, &feed_url).unwrap_or_default();
            for r in entries.into_iter().take(MAX_DISCOVERED) {
                out.push(r.url);
            }
        }
    }

    // (4) Everything the pipeline pushed into the frontier.
    for u in metadata_urls(
        pipeline,
        "SELECT url FROM frontier_entry LIMIT ?1",
        MAX_DISCOVERED as i64,
    ) {
        out.push(u);
    }

    out
}

/// Fetch a sitemap; if it is an index, follow exactly one level of children.
/// Returns every resource URL found.
async fn expand_sitemap(fetcher: &Fetcher, origin: &str, sm_url: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Some(bytes) = fetch_body(fetcher, sm_url).await else {
        return out;
    };
    let children = sitemap_index_urls(&bytes).unwrap_or_default();
    if !children.is_empty() {
        for child in children.into_iter().take(MAX_CHILD_SITEMAPS) {
            let child = resolve_against(origin, &child);
            if !child.starts_with("http") {
                continue;
            }
            let Some(cb) = fetch_body(fetcher, &child).await else {
                continue;
            };
            let resources = parse_sitemap(&cb, &child).unwrap_or_default();
            for r in resources.into_iter().take(MAX_DISCOVERED) {
                out.push(r.url);
            }
        }
    } else {
        let resources = parse_sitemap(&bytes, sm_url).unwrap_or_default();
        for r in resources.into_iter().take(MAX_DISCOVERED) {
            out.push(r.url);
        }
    }
    out
}

/// Read a string column from the pipeline's metadata DB (read-only).
fn metadata_urls(pipeline: &Pipeline, sql: &str, limit: i64) -> Vec<String> {
    pipeline
        .with_meta_conn(|conn| {
            let mut stmt = conn.prepare(sql).map_err(search_metadata::db_err)?;
            let rows = stmt
                .query_map(rusqlite::params![limit], |r| r.get::<_, String>(0))
                .map_err(search_metadata::db_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(search_metadata::db_err)?;
            Ok(rows)
        })
        .unwrap_or_default()
}

/// Fetch a discovery body (2xx only). Respects SSRF (validated per hop by the
/// fetcher); returns raw bytes.
async fn fetch_body(fetcher: &Fetcher, url: &str) -> Option<Vec<u8>> {
    match fetcher.fetch(url, None, None).await {
        Ok(o) if (200..300).contains(&o.status) => o.body,
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Outlink extraction (link following)
// ---------------------------------------------------------------------------

/// Extract document-shaped outlinks from an HTML page.
///
/// Signature: `pub fn article_links(base: &str, html: &str, cap: usize) -> Vec<String>`.
///
/// Every `href=` occurrence is found by a hand-written scan (both `"` and `'`
/// quoting, attribute name matched case-insensitively, `data-href`/`xhref`
/// ignored), its value is resolved against `base` with [`resolve_against`], and
/// the result is kept only when ALL hold:
///   * the resolved URL is absolute `http`/`https` (`mailto:`, `javascript:` or
///     a leftover relative value is dropped);
///   * any `#fragment` is stripped;
///   * [`is_low_value`] is `false` (no profiles, utility pages, static assets);
///   * [`url_rank`] `> 0` (document-shaped: `/docs/`, `/blog/`, dated slugs…).
/// Kept links are deduped preserving first-seen order and truncated to `cap`.
///
/// This function is **origin-agnostic** — same-origin enforcement happens at the
/// call site ([`discover_seed_urls`]).
///
/// Safety on malformed input: a single left-to-right pass over at most
/// [`MAX_LINK_SCAN_BYTES`] bytes (a 5 MB body is truncated there, so the scan is
/// O(n) and cannot blow up); every index is bounds-checked, so truncated tags,
/// stray quotes and unterminated comments/`<script>` never panic. `href` inside
/// an HTML comment or a `<script>` body is ignored. HTML entities in an href
/// (`&amp;`) are NOT decoded (no dependency is available); such a URL usually
/// still parses.
pub fn article_links(base: &str, html: &str, cap: usize) -> Vec<String> {
    if cap == 0 {
        return Vec::new();
    }
    let bytes = html.as_bytes();
    let end = bytes.len().min(MAX_LINK_SCAN_BYTES);
    let src = &bytes[..end];
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut i = 0usize;
    while i < src.len() {
        if out.len() >= cap {
            break;
        }
        // Ignore everything inside a comment or a script body.
        if starts_with_ci(src, i, b"<!--") {
            i = skip_until(src, i + 4, b"-->");
            continue;
        }
        if starts_with_ci(src, i, b"<script") {
            i = skip_until(src, i + 7, b"</script");
            continue;
        }
        // A whole attribute name: the preceding byte must not be part of one.
        if starts_with_ci(src, i, b"href") && (i == 0 || !is_name_byte(src[i - 1])) {
            if let Some((raw, next)) = read_href(src, i + 4) {
                i = next;
                push_link(&mut out, &mut seen, base, &raw, cap);
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Parse `href`'s value starting just after the attribute name. Returns the raw
/// (undecoded) value and the index just past it. Bounds-checked throughout.
fn read_href(src: &[u8], mut j: usize) -> Option<(String, usize)> {
    let n = src.len();
    while j < n && matches!(src[j], b' ' | b'\t' | b'\n' | b'\r') {
        j += 1;
    }
    if j >= n || src[j] != b'=' {
        return None;
    }
    j += 1;
    while j < n && matches!(src[j], b' ' | b'\t' | b'\n' | b'\r') {
        j += 1;
    }
    if j >= n {
        return None;
    }
    let quote = src[j];
    if quote == b'"' || quote == b'\'' {
        let start = j + 1;
        let mut k = start;
        while k < n && src[k] != quote {
            k += 1;
        }
        let value = String::from_utf8_lossy(&src[start..k]).into_owned();
        let next = if k < n { k + 1 } else { k };
        Some((value, next))
    } else {
        let start = j;
        let mut k = start;
        while k < n && !matches!(src[k], b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'<') {
            k += 1;
        }
        Some((String::from_utf8_lossy(&src[start..k]).into_owned(), k))
    }
}

/// Resolve, filter and dedupe one raw href; push it when it survives.
fn push_link(
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
    base: &str,
    raw: &str,
    cap: usize,
) {
    if out.len() >= cap {
        return;
    }
    let raw = raw.trim();
    if raw.is_empty() {
        return;
    }
    let resolved = resolve_against(base, raw);
    let Ok(mut u) = Url::parse(&resolved) else {
        return;
    };
    if u.scheme() != "http" && u.scheme() != "https" {
        return;
    }
    u.set_fragment(None);
    let candidate = u.to_string();
    if is_low_value(&candidate) || url_rank(&candidate) <= 0 {
        return;
    }
    if seen.insert(candidate.clone()) {
        out.push(candidate);
    }
}

/// `true` when `hay[at..]` starts with `needle` compared ASCII-case-insensitively.
fn starts_with_ci(hay: &[u8], at: usize, needle: &[u8]) -> bool {
    if at + needle.len() > hay.len() {
        return false;
    }
    hay[at..at + needle.len()]
        .iter()
        .zip(needle)
        .all(|(h, n)| h.to_ascii_lowercase() == *n)
}

/// Index just past the first occurrence of `needle` at or after `from`, or the
/// end of `hay` when it is absent.
fn skip_until(hay: &[u8], from: usize, needle: &[u8]) -> usize {
    let mut i = from;
    while i < hay.len() {
        if starts_with_ci(hay, i, needle) {
            return i + needle.len();
        }
        i += 1;
    }
    hay.len()
}

/// A byte that can be part of an attribute name (`-`/`:`/`_` or alphanumeric).
fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b':' | b'_')
}

/// Normalised site key for the same-origin check: the lowercased host with a
/// leading `www.` removed, plus an explicit port when one is present (the `url`
/// crate already strips default `:80`/`:443`). The scheme is deliberately NOT
/// part of the key, so `http://host/` and `https://host/` — and `www.host` vs
/// `host` — count as the same origin when following links.
fn site_key(url: &str) -> Option<String> {
    let u = Url::parse(url).ok()?;
    let host = u.host_str()?.to_ascii_lowercase();
    let bare = host.strip_prefix("www.").unwrap_or(&host);
    match u.port() {
        Some(p) => Some(format!("{bare}:{p}")),
        None => Some(bare.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_seeds(path: &Path) -> AppResult<Vec<String>> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        AppError::Invalid(format!("failed to read seeds file {}: {e}", path.display()))
    })?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect())
}

fn read_log_urls(path: &Path) -> AppResult<HashSet<String>> {
    let mut set = HashSet::new();
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(u) = v.get("url").and_then(|x| x.as_str()) {
                    set.insert(u.to_string());
                }
            }
        }
    }
    Ok(set)
}

fn read_frontier(path: &Path) -> Vec<String> {
    let bytes = std::fs::read(path).unwrap_or_default();
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// The delta file that sits beside `frontier.json`.
fn adds_path(frontier: &Path) -> std::path::PathBuf {
    frontier.with_extension("adds")
}

/// Append newly-enqueued URLs to the delta file, one JSON array element per line.
///
/// Line-delimited rather than one array: an interrupted append leaves at most one
/// unparseable trailing line, which the reader skips, exactly like the crawl log.
fn append_frontier_adds(path: &Path, urls: &[String]) {
    if urls.is_empty() {
        return;
    }
    let mut buf = String::with_capacity(urls.len() * 64);
    for u in urls {
        match serde_json::to_string(u) {
            Ok(s) => {
                buf.push_str(&s);
                buf.push('\n');
            }
            Err(_) => continue,
        }
    }
    match std::fs::OpenOptions::new().create(true).append(true).open(path) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(buf.as_bytes()) {
                eprintln!("failed to append frontier adds: {e}");
            }
        }
        Err(e) => eprintln!("failed to open frontier adds: {e}"),
    }
}

/// Every URL the delta file holds. A missing file is empty; a malformed trailing line is
/// skipped (see [`append_frontier_adds`]).
fn read_frontier_adds(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<String>(l).ok())
        .collect()
}

fn persist_frontier(path: &Path, urls: impl Iterator<Item = String>) {
    let v: Vec<String> = urls.collect();
    if let Ok(bytes) = serde_json::to_vec(&v) {
        if let Err(e) = write_atomic(path, &bytes) {
            eprintln!("failed to persist frontier: {e}");
        }
    }
}

fn append_log_line(path: &Path, line: &str) -> std::io::Result<()> {
    // A true append, not a read-whole-file + rewrite. The previous shape read the
    // entire log, appended one line and rewrote it atomically, which made the k-th
    // document O(k): a 250K-document shard re-wrote ~20 MB per line, hundreds of
    // gigabytes of needless I/O over the run, all of it counted as idle CPU
    // (iowait) while the box looked unused. The log is append-only JSONL and the
    // resume reader already ignores an unparseable trailing line, so a plain
    // O_APPEND write of one whole line is both correct and O(1).
    let mut buf = Vec::with_capacity(line.len() + 1);
    buf.extend_from_slice(line.as_bytes());
    buf.push(b'\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(&buf)
}

/// Write `bytes` to `path` atomically (temp file + rename).
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Path segments that never carry an indexable document: account/utility pages
/// and static assets. Big hosts advertise these in their sitemaps (a
/// semanticscholar sitemap is thousands of `/author/<name>/<id>` profile URLs),
/// so without a gate the attempt budget is spent on pages that can never be
/// indexed — a 10k-document run then yields a handful of documents.
const LOW_VALUE_SEGMENTS: &[&str] = &[
    "/author/", "/authors/", "/profile", "/user/", "/users/", "/people/", "/members/",
    "/login", "/signin", "/sign-in", "/signup", "/sign-up", "/register", "/account",
    "/cart", "/checkout", "/password", "/settings", "/preferences", "/notifications",
    "/search", "/help", "/faq", "/support", "/legal/", "/privacy", "/terms", "/cookies",
    "/sitemap", "/robots.txt", "/feed/", "/rss/", "/atom/", "/tag/", "/tags/", "/label/",
    "/print/", "/amp/", "/amp/", "/cdn-cgi/", "/topic/", "/topics/", "/browse/",
    // Generated index pages: a docs.rs "list of all items in this crate" or a
    // release index is a machine listing with almost no prose — it indexes as a
    // document but supports no question.
    "/all.html", "/releases/", "/versions/",
];

/// File suffixes that are never an indexable document in this crawler.
const LOW_VALUE_SUFFIXES: &[&str] = &[
    ".css", ".js", ".mjs", ".json", ".xml", ".txt", ".csv", ".png", ".jpg", ".jpeg",
    ".gif", ".svg", ".ico", ".webp", ".woff", ".woff2", ".ttf", ".eot", ".zip", ".gz",
    ".tar", ".rar", ".7z", ".mp4", ".webm", ".mp3", ".wav", ".exe", ".dmg", ".apk",
];

/// Path prefixes that ARE documents: research papers, docs, blog posts, wikis.
/// Used only to order the queue (ranked first), never to exclude.
const CONTENT_SEGMENTS: &[&str] = &[
    "/abs/", "/pdf/", "/paper/", "/papers/", "/doi/", "/docs/", "/doc/", "/documentation",
    "/blog/", "/posts/", "/post/", "/news/", "/article", "/wiki/", "/learn/", "/guide",
    "/tutorial", "/reference/", "/manual", "/handbook", "/notes/", "/research/",
    "/publication", "/report",
];

/// `true` when a URL cannot be a document (a profile, a utility endpoint or a
/// static asset) and must not consume an attempt.
pub fn is_low_value(url: &str) -> bool {
    let path = match Url::parse(url) {
        Ok(u) => u.path().to_ascii_lowercase(),
        Err(_) => return true,
    };
    if LOW_VALUE_SUFFIXES.iter().any(|s| path.ends_with(s)) {
        return true;
    }
    LOW_VALUE_SEGMENTS.iter().any(|s| path.contains(s))
}

/// Content-shape rank: documented content first, everything else after it.
/// Deterministic and content-free (no body is fetched to compute it).
pub fn url_rank(url: &str) -> i32 {
    let path = match Url::parse(url) {
        Ok(u) => u.path().to_ascii_lowercase(),
        Err(_) => return -1,
    };
    let mut rank = 0;
    if CONTENT_SEGMENTS.iter().any(|s| path.contains(s)) {
        rank += 2;
    }
    // A dated or wordy slug path is usually a real article.
    if path.rsplit('/').next().map(|s| s.matches('-').count()).unwrap_or(0) >= 2 {
        rank += 1;
    }
    if path.ends_with(".html") || path.ends_with(".htm") {
        rank += 1;
    }
    // A bare query string (no path) is a search/filter view, not a document.
    if path.trim_matches('/').is_empty() {
        rank -= 1;
    }
    rank
}

/// Origin (`scheme://host[:port]`) of a URL, else the raw string for an
/// unparseable URL so it still gets its own scheduler bucket.
fn origin_of(raw: &str) -> String {
    match Url::parse(raw) {
        Ok(u) => match (u.host_str(), u.port()) {
            (Some(host), Some(port)) => format!("{}://{}:{}", u.scheme(), host, port),
            (Some(host), None) => format!("{}://{}", u.scheme(), host),
            (None, _) => raw.to_string(),
        },
        Err(_) => raw.to_string(),
    }
}

/// Delay before the next request to an origin that just answered HTTP 429.
///
/// A server-supplied `Retry-After` (seconds) wins outright, capped at
/// [`MAX_RATE_LIMIT_BACKOFF_MS`]. Otherwise the delay doubles per consecutive
/// 429: `level` is 1 for the first 429 (`base`), 2 for the second (`2·base`),
/// and so on, capped at the ceiling. It always adds delay on top of
/// `opts.origin_interval` — it can only slow a host down, never speed it up —
/// except when a server explicitly asks for `Retry-After: 0`.
fn rate_limit_backoff(retry_after: Option<u64>, level: u32) -> Duration {
    if let Some(secs) = retry_after {
        return Duration::from_secs(secs).min(Duration::from_millis(MAX_RATE_LIMIT_BACKOFF_MS));
    }
    let shift = level.saturating_sub(1).min(16);
    let ms = BASE_RATE_LIMIT_BACKOFF_MS.saturating_mul(1u64 << shift);
    Duration::from_millis(ms.min(MAX_RATE_LIMIT_BACKOFF_MS))
}

/// Whether an error message is a DNS resolution failure. Such a failure is
/// permanent for the lifetime of a run, so the URL is counted and skipped
/// instead of being retried; the scheduler never re-queues it in this run.
fn is_dns_failure(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("dns error")
        || m.contains("failed to lookup address")
        || m.contains("name or service not known")
        || m.contains("nodename nor servname")
        || m.contains("no such host")
        || m.contains("temporary failure in name resolution")
}

fn resolve_against(origin: &str, url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        return url.to_string();
    }
    match Url::parse(origin).and_then(|b| b.join(url)) {
        Ok(u) => u.to_string(),
        Err(_) => url.to_string(),
    }
}

/// Best-effort media type for the raw bytes `crawl_one` ingested. The router
/// cannot see a plain-HTML body without a declared type, so an HTML-looking body
/// is reported as `text/html` (a `crawl_one` ingest is only ever HTML or PDF).
fn sniff_media_type(url: &str, raw: &[u8]) -> &'static str {
    let sniff = &raw[..raw.len().min(4096)];
    match classify(None, url, sniff).route {
        Route::Pdf => "application/pdf",
        _route if looks_like_html(sniff) => "text/html",
        route => media_type(&route),
    }
}

fn looks_like_html(sniff: &[u8]) -> bool {
    let mut s = sniff;
    while let Some((&b, rest)) = s.split_first() {
        if b.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    if s.first() == Some(&b'<') {
        return true;
    }
    let window = &sniff[..sniff.len().min(1024)];
    let lower: Vec<u8> = window.iter().map(|b| b.to_ascii_lowercase()).collect();
    contains_bytes(&lower, b"<html") || contains_bytes(&lower, b"<!doctype html")
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Map a sniffed router route onto a media type for the log + blob extension.
fn media_type(route: &Route) -> &'static str {
    match route {
        Route::Html => "text/html",
        Route::Pdf => "application/pdf",
        Route::Feed => "application/rss+xml",
        Route::Json => "application/json",
        Route::Xml => "application/xml",
        Route::PlainText => "text/plain",
        Route::Image => "image/unknown",
        Route::Archive => "application/octet-stream",
        Route::Other(_) => "application/octet-stream",
    }
}

/// Blob file extension for a content type (`.bin` when unknown).
/// Gzip `bytes` for the raw spill (`--compress`).
fn gzip_bytes(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes)?;
    encoder.finish()
}

fn ext_for(content_type: Option<&str>) -> &'static str {
    match content_type.unwrap_or("") {
        "application/pdf" => "pdf",
        "text/html" => "html",
        "application/json" => "json",
        "application/xml" => "xml",
        "application/rss+xml" | "application/atom+xml" => "xml",
        "text/plain" => "txt",
        "image/unknown" => "img",
        _ => "bin",
    }
}

// ---------------------------------------------------------------------------
// SHA-256 (self-contained: the agent0 crate does not depend on `sha2`)
// ---------------------------------------------------------------------------

const K256: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

fn sha256_hex(data: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K256[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = String::with_capacity(64);
    for word in h {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_empty_and_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn extension_mapping() {
        assert_eq!(ext_for(Some("application/pdf")), "pdf");
        assert_eq!(ext_for(Some("text/html")), "html");
        assert_eq!(ext_for(None), "bin");
    }

    #[test]
    fn site_key_normalises_www_and_scheme_but_keeps_port() {
        // `https://www.` and `http://` forms of the same host are one site.
        assert_eq!(
            site_key("https://www.example.com/docs/x").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            site_key("http://example.com/docs/x").as_deref(),
            Some("example.com")
        );
        // A distinct host or an explicit port is a different site.
        assert_eq!(site_key("https://other.com/x").as_deref(), Some("other.com"));
        assert_eq!(
            site_key("https://example.com:8443/x").as_deref(),
            Some("example.com:8443")
        );
        // Unparseable input has no site key.
        assert!(site_key("not a url").is_none());
    }

    #[test]
    fn rate_limit_backoff_honours_retry_after_and_caps() {
        // `Retry-After` wins outright over the exponential value.
        assert_eq!(rate_limit_backoff(Some(3), 1), Duration::from_secs(3));
        // ... but is capped at the ceiling.
        assert_eq!(
            rate_limit_backoff(Some(u64::MAX), 1),
            Duration::from_millis(MAX_RATE_LIMIT_BACKOFF_MS)
        );
        // Without a header the delay doubles per consecutive 429: 1s, 2s, 4s.
        assert_eq!(
            rate_limit_backoff(None, 1),
            Duration::from_millis(BASE_RATE_LIMIT_BACKOFF_MS)
        );
        assert_eq!(
            rate_limit_backoff(None, 2),
            Duration::from_millis(2 * BASE_RATE_LIMIT_BACKOFF_MS)
        );
        assert_eq!(
            rate_limit_backoff(None, 3),
            Duration::from_millis(4 * BASE_RATE_LIMIT_BACKOFF_MS)
        );
        // ... and is capped.
        assert_eq!(
            rate_limit_backoff(None, 99),
            Duration::from_millis(MAX_RATE_LIMIT_BACKOFF_MS)
        );
    }

    #[test]
    fn dns_failure_detection() {
        assert!(is_dns_failure(
            "network error: error trying to connect: dns error: failed to lookup address information"
        ));
        assert!(is_dns_failure("dns error"));
        assert!(is_dns_failure("Name or service not known"));
        assert!(!is_dns_failure("network error: connection refused"));
        assert!(!is_dns_failure("rate limited (HTTP 429) for https://x/"));
    }

    #[test]
    fn concurrency_resolution_precedence_and_clamp() {
        // Flag wins over the environment.
        assert_eq!(resolve_concurrency(Some(3), Some("128".into())), 3);
        // Environment is used when no flag is present.
        assert_eq!(resolve_concurrency(None, Some("128".into())), 128);
        // Whitespace around the env value is tolerated.
        assert_eq!(resolve_concurrency(None, Some("  128 ".into())), 128);
        // Absent/invalid env falls back to the raised default.
        assert_eq!(resolve_concurrency(None, None), DEFAULT_CONCURRENCY);
        assert_eq!(resolve_concurrency(None, Some("nope".into())), DEFAULT_CONCURRENCY);
        // A zero bound is clamped to 1, never a wedged crawl.
        assert_eq!(resolve_concurrency(Some(0), None), 1);
    }

    #[test]
    fn discovery_window_widens_concurrency_and_honours_override() {
        // The window is the fetch concurrency widened by the factor, so a chunk
        // parks many more hosts in flight than the drain's own bound.
        assert_eq!(discovery_window(32, None), 32 * DISCOVERY_WINDOW_FACTOR);
        assert_eq!(discovery_window(64, None), 64 * DISCOVERY_WINDOW_FACTOR);
        // A zero concurrency is clamped to 1 before widening: never zero work.
        assert_eq!(discovery_window(0, None), DISCOVERY_WINDOW_FACTOR);
        // An absolute override wins, whitespace tolerated.
        assert_eq!(discovery_window(8, Some("256".into())), 256);
        assert_eq!(discovery_window(8, Some("  512 ".into())), 512);
        // A zero/invalid/non-positive override falls back to the widened default.
        assert_eq!(discovery_window(8, Some("0".into())), 8 * DISCOVERY_WINDOW_FACTOR);
        assert_eq!(discovery_window(8, Some("nope".into())), 8 * DISCOVERY_WINDOW_FACTOR);
        // The window is capped so a large drain bound cannot exhaust the fd
        // budget with one socket per discovery task.
        assert_eq!(discovery_window(4096, None), MAX_DISCOVERY_WINDOW);
    }

    /// The log must be appended in place (O(1)) and a torn final write must not
    /// hide the lines before it, or a crash mid-run would lose the resume state.
    #[test]
    fn log_append_is_incremental_and_tolerates_a_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crawl-log.jsonl");
        append_log_line(&path, r#"{"url":"https://a/","outcome":"ingested"}"#).unwrap();
        append_log_line(&path, r#"{"url":"https://b/","outcome":"ingested"}"#).unwrap();
        // A crash mid-write leaves a partial, unparseable final line.
        {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(b"{\"url\":\"https://c/\"").unwrap();
        }
        let seen = read_log_urls(&path).unwrap();
        assert!(seen.contains("https://a/"), "first good line survives");
        assert!(seen.contains("https://b/"), "second good line survives");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().lines().count(),
            3,
            "each append grows the file by one line instead of rewriting it"
        );
    }
}
