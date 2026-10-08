//! Browser escalation (plan Stage 1, crawler §21–22).
//!
//! A browser fetcher renders JS-heavy pages and reports the XHR/fetch endpoints it saw so
//! future fetches can hit the JSON directly. Default build ships a hermetic mock; the real
//! Chromium driver sits behind `feature = "chromium"`. Interfaces frozen in
//! `docs/STAGE1_SPEC.md`.
//!
//! ## Layout
//! * [`BrowserLimits`] — per-fetch budget (§21).
//! * [`BrowserResult`] — rendered DOM + observed JSON endpoints (§22).
//! * [`BrowserFetcher`] — the escalation interface.
//! * [`MockBrowser`] — hermetic, fixture-backed default used by tests.
//! * `chromium::ChromiumBrowser` — real driver, `feature = "chromium"` only.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;

/// Default browser time budget, in milliseconds.
pub const DEFAULT_MAX_MS: u64 = 15_000;
/// Default cap on rendered DOM size, in bytes (8 MiB).
pub const DEFAULT_MAX_DOM_BYTES: usize = 8 * 1024 * 1024;
/// Block media (images/video/fonts) by default.
pub const DEFAULT_BLOCK_MEDIA: bool = true;
/// Default redirect chain cap.
pub const DEFAULT_MAX_REDIRECTS: u8 = 5;

/// Resource limits for a single browser fetch (§21). `Default` is the frozen
/// production baseline: 15 s, 8 MiB, media blocked, 5 redirects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserLimits {
    pub max_ms: u64,
    pub max_dom_bytes: usize,
    pub block_media: bool,
    pub max_redirects: u8,
}

impl Default for BrowserLimits {
    fn default() -> Self {
        Self {
            max_ms: DEFAULT_MAX_MS,
            max_dom_bytes: DEFAULT_MAX_DOM_BYTES,
            block_media: DEFAULT_BLOCK_MEDIA,
            max_redirects: DEFAULT_MAX_REDIRECTS,
        }
    }
}

/// Outcome of a browser render. `json_endpoints` are absolute URLs of XHR/fetch
/// calls observed during rendering, deduped (first-seen order preserved).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BrowserResult {
    pub final_url: String,
    pub html: String,
    pub status: u16,
    pub json_endpoints: Vec<String>,
    pub elapsed_ms: u64,
}

impl BrowserResult {
    /// Convenience constructor: an HTML-only result with no observed endpoints.
    pub fn new(final_url: impl Into<String>, html: impl Into<String>, status: u16) -> Self {
        Self {
            final_url: final_url.into(),
            html: html.into(),
            status,
            json_endpoints: Vec::new(),
            elapsed_ms: 0,
        }
    }
}

/// Failure modes of a browser fetch.
#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    /// The render exceeded the wall-clock budget (carries the limit in ms).
    #[error("browser navigation exceeded {0} ms")]
    Timeout(u64),
    /// The rendered DOM exceeded the byte budget (carries the observed size).
    #[error("rendered DOM exceeded {0} bytes")]
    DomTooLarge(usize),
    /// Navigation/render failed for a transport or protocol reason.
    #[error("navigation failed: {0}")]
    Navigation(String),
    /// No fixture was registered for the requested URL (mock driver).
    #[error("no fixture registered for {0}")]
    NoFixture(String),
    /// The request was refused before navigation (bad scheme, blocked host, policy).
    #[error("navigation blocked: {0}")]
    Blocked(String),
}

/// The browser escalation interface. Implementors must be `Send + Sync` because a
/// single driver (one persistent Chromium) is shared across the fetch queue.
#[async_trait]
pub trait BrowserFetcher: Send + Sync {
    async fn fetch(&self, url: &str, limits: BrowserLimits) -> Result<BrowserResult, BrowserError>;
}

/// Keep only absolute URLs (`scheme://…`), drop relative/schemeless entries, and
/// remove duplicates while preserving first-seen order.
///
/// `https://a/x` and `https://a/x` collapse to one; `/api/x` and `api/x` are dropped.
pub fn dedup_endpoints(endpoints: &[String]) -> Vec<String> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for e in endpoints {
        if !is_absolute_url(e) {
            continue;
        }
        if seen.insert(e.as_str()) {
            out.push(e.clone());
        }
    }
    out
}

/// Scheme-bearing absolute URL check with no external parser.
///
/// Accepts `scheme://host…` where `scheme` starts with an ASCII letter and contains
/// only letters/digits/`+`/`-`/`.`, and a non-empty remainder. Rejects `//host`,
/// `/path`, `api/x`, and `mailto:x` (no `//`).
fn is_absolute_url(s: &str) -> bool {
    let Some((scheme, rest)) = s.split_once("://") else {
        return false;
    };
    if rest.is_empty() || scheme.is_empty() {
        return false;
    }
    let mut chars = scheme.chars();
    let first = chars.next().unwrap();
    first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Only http/https URLs are navigable by the browser driver.
fn is_navigable_scheme(s: &str) -> bool {
    let lower = s.trim_start().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Hermetic, in-memory [`BrowserFetcher`] used by the default build and tests.
///
/// Fixtures are registered per URL. `fetch` honours the limits deterministically:
/// * a non-http(s) URL is [`BrowserError::Blocked`];
/// * an unregistered URL is [`BrowserError::NoFixture`];
/// * a fixture whose `html` exceeds `max_dom_bytes` is [`BrowserError::DomTooLarge`];
/// * a fixture whose `elapsed_ms` exceeds `max_ms` is [`BrowserError::Timeout`].
///
/// Registered `json_endpoints` are deduped (absolute-only, first-seen order) on read.
#[derive(Debug, Default, Clone)]
pub struct MockBrowser {
    fixtures: HashMap<String, BrowserResult>,
}

impl MockBrowser {
    pub fn new() -> Self {
        Self {
            fixtures: HashMap::new(),
        }
    }

    /// Register (or replace) the fixture served for `url`.
    pub fn register(&mut self, url: impl Into<String>, result: BrowserResult) {
        self.fixtures.insert(url.into(), result);
    }

    /// Builder-style variant of [`MockBrowser::register`].
    pub fn with_fixture(mut self, url: impl Into<String>, result: BrowserResult) -> Self {
        self.register(url, result);
        self
    }

    /// Number of registered fixtures.
    pub fn len(&self) -> usize {
        self.fixtures.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fixtures.is_empty()
    }
}

#[async_trait]
impl BrowserFetcher for MockBrowser {
    async fn fetch(&self, url: &str, limits: BrowserLimits) -> Result<BrowserResult, BrowserError> {
        if !is_navigable_scheme(url) {
            return Err(BrowserError::Blocked(format!("unsupported url scheme: {url}")));
        }
        let mut result = self
            .fixtures
            .get(url)
            .cloned()
            .ok_or_else(|| BrowserError::NoFixture(url.to_string()))?;

        if result.html.len() > limits.max_dom_bytes {
            return Err(BrowserError::DomTooLarge(result.html.len()));
        }
        if result.elapsed_ms > limits.max_ms {
            return Err(BrowserError::Timeout(limits.max_ms));
        }

        result.json_endpoints = dedup_endpoints(&result.json_endpoints);
        Ok(result)
    }
}

#[cfg(feature = "chromium")]
pub mod chromium;

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(url: &str, html: &str) -> BrowserResult {
        BrowserResult::new(url, html, 200)
    }

    #[tokio::test]
    async fn mock_returns_registered_fixture() {
        let mut mock = MockBrowser::new();
        mock.register(
            "https://example.com/js",
            BrowserResult::new("https://example.com/js", "<html>ok</html>", 200),
        );
        let got = mock
            .fetch("https://example.com/js", BrowserLimits::default())
            .await
            .expect("fixture should be returned");
        assert_eq!(got.final_url, "https://example.com/js");
        assert_eq!(got.html, "<html>ok</html>");
        assert_eq!(got.status, 200);
    }

    #[tokio::test]
    async fn mock_with_fixture_builder_and_len() {
        let mock = MockBrowser::new()
            .with_fixture("https://a.test/", fixture("https://a.test/", "x"))
            .with_fixture("https://b.test/", fixture("https://b.test/", "y"));
        assert_eq!(mock.len(), 2);
        assert!(!mock.is_empty());
        assert!(mock
            .fetch("https://b.test/", BrowserLimits::default())
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn mock_missing_fixture_is_no_fixture() {
        let mock = MockBrowser::new();
        let err = mock
            .fetch("https://nope.test/", BrowserLimits::default())
            .await
            .unwrap_err();
        match err {
            BrowserError::NoFixture(u) => assert_eq!(u, "https://nope.test/"),
            other => panic!("expected NoFixture, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_dom_too_large_when_html_exceeds_limit() {
        let mut mock = MockBrowser::new();
        let big = "a".repeat(64);
        mock.register("https://big.test/", fixture("https://big.test/", &big));
        let limits = BrowserLimits {
            max_dom_bytes: 16,
            ..BrowserLimits::default()
        };
        let err = mock.fetch("https://big.test/", limits).await.unwrap_err();
        match err {
            BrowserError::DomTooLarge(n) => assert_eq!(n, 64),
            other => panic!("expected DomTooLarge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_timeout_when_elapsed_exceeds_budget() {
        let mut mock = MockBrowser::new();
        let mut f = fixture("https://slow.test/", "ok");
        f.elapsed_ms = 9_000;
        mock.register("https://slow.test/", f);
        let limits = BrowserLimits {
            max_ms: 1_000,
            ..BrowserLimits::default()
        };
        let err = mock.fetch("https://slow.test/", limits).await.unwrap_err();
        match err {
            BrowserError::Timeout(ms) => assert_eq!(ms, 1_000),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_blocks_non_http_scheme() {
        let mock = MockBrowser::new();
        assert!(matches!(
            mock.fetch("file:///etc/passwd", BrowserLimits::default())
                .await
                .unwrap_err(),
            BrowserError::Blocked(_)
        ));
    }

    #[tokio::test]
    async fn mock_dedupes_json_endpoints_preserving_order() {
        let mut mock = MockBrowser::new();
        let mut f = fixture("https://spa.test/", "<html></html>");
        f.json_endpoints = vec![
            "https://spa.test/api/a".into(),
            "/relative".into(),
            "https://spa.test/api/a".into(),
            "https://spa.test/api/b".into(),
            "api/c".into(),
        ];
        mock.register("https://spa.test/", f);
        let got = mock
            .fetch("https://spa.test/", BrowserLimits::default())
            .await
            .unwrap();
        assert_eq!(
            got.json_endpoints,
            vec!["https://spa.test/api/a", "https://spa.test/api/b"]
        );
    }

    #[test]
    fn default_limits_are_frozen_values() {
        let l = BrowserLimits::default();
        assert_eq!(l.max_ms, 15_000);
        assert_eq!(l.max_dom_bytes, 8 * 1024 * 1024);
        assert!(l.block_media);
        assert_eq!(l.max_redirects, 5);
    }

    #[test]
    fn dedup_endpoints_keeps_absolute_drops_relative_and_dedupes() {
        let input = vec![
            "https://x.test/1".to_string(),
            "/api/2".to_string(),
            "https://x.test/1".to_string(),
            "http://y.test/3".to_string(),
            "api/4".to_string(),
            "//proto-relative/5".to_string(),
            "https://x.test/2".to_string(),
        ];
        let out = dedup_endpoints(&input);
        assert_eq!(
            out,
            vec![
                "https://x.test/1",
                "http://y.test/3",
                "https://x.test/2",
            ]
        );
    }

    #[test]
    fn is_absolute_url_accepts_schemes_rejects_relative() {
        assert!(is_absolute_url("https://a/b"));
        assert!(is_absolute_url("wss://a/b"));
        assert!(is_absolute_url("custom+scheme://a"));
        assert!(!is_absolute_url("/a/b"));
        assert!(!is_absolute_url("api/x"));
        assert!(!is_absolute_url("//host/x"));
        assert!(!is_absolute_url("mailto:a@b"));
        assert!(!is_absolute_url("1http://x"));
        assert!(!is_absolute_url("https://"));
    }
}
