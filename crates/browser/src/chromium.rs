//! Real Chromium driver — `feature = "chromium"` (crawler §21–22).
//!
//! **UNVERIFIED IN THIS ENVIRONMENT.** `chromiumoxide` 0.7 is not downloaded here and
//! this module was never compiled or run; it is a thin, best-effort driver written to the
//! documented 0.7 API and intended to be validated on a machine with Chromium available.
//! It must never be required for the default build — the workspace builds and tests with
//! this module absent (`default` features).
//!
//! Design (per §21): one persistent [`Browser`] is launched once and shared; each fetch
//! opens a **short-lived incognito context** (isolated session, closed immediately after),
//! so no per-page browser process is spawned and no state is retained between fetches.
//! Network interception (§22) records XHR/fetch request URLs and exposes them deduped as
//! [`BrowserResult::json_endpoints`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chromiumoxide::cdp::browser_protocol::target::{
    CreateBrowserContextParams, CreateTargetParams,
};
use chromiumoxide::cdp::browser_protocol::network::{
    EventRequestWillBeSent, ResourceType, SetBlockedUrLsParams,
};
use chromiumoxide::{Browser, BrowserConfig};
use futures::StreamExt;

use crate::{BrowserError, BrowserFetcher, BrowserLimits, BrowserResult, dedup_endpoints};

/// Launch configuration for the persistent Chromium handle.
#[derive(Debug, Clone)]
pub struct ChromiumConfig {
    /// Run headless (default `true`).
    pub headless: bool,
    /// Optional explicit Chromium/Chrome executable path.
    pub executable: Option<std::path::PathBuf>,
    /// Chromium profile directory. `None` uses a private per-process directory.
    ///
    /// Chromium takes an exclusive lock on its profile. With the crate default (one fixed
    /// directory) a second launch — another crawl, a test, a leftover browser from an earlier
    /// run — blocks until the first one exits, which presents as `launch()` hanging forever.
    /// A per-process directory removes the collision; set this explicitly only to share a
    /// profile deliberately.
    pub user_data_dir: Option<std::path::PathBuf>,
    /// Disable Chromium's sandbox (`--no-sandbox`).
    ///
    /// **A real security trade-off, so it is explicit rather than automatic**: with the sandbox
    /// off, a renderer that exploits a bug is not contained. It is needed only where Chromium
    /// refuses to start otherwise — notably as root (`Running as root without --no-sandbox is
    /// not supported`), which is how a container usually runs. Enable with
    /// `SEARCH_CHROMIUM_NO_SANDBOX=1`; when it is off and Chromium is running as root, the driver
    /// fails with Chromium's own message rather than silently weakening the sandbox.
    pub no_sandbox: bool,
}

impl Default for ChromiumConfig {
    fn default() -> Self {
        Self {
            headless: true,
            executable: None,
            user_data_dir: None,
            no_sandbox: false,
        }
    }
}

/// A persistent Chromium process that serves many short-lived isolated contexts.
///
/// The handler task keeps the CDP connection alive; dropping this struct closes the
/// browser. `Arc<Browser>` must be retained for the incognito-context API.
pub struct ChromiumBrowser {
    browser: Arc<Browser>,
    _handler: tokio::task::JoinHandle<()>,
}

impl ChromiumBrowser {
    /// Launch with defaults: headless, and the executable from `SEARCH_CHROMIUM` when set.
    ///
    /// Discovery alone is not enough on a host whose Chromium lives outside `PATH` (this one
    /// does), and a crawl that silently cannot escalate is worse than one told where to look.
    pub async fn launch() -> Result<Self, BrowserError> {
        let mut config = ChromiumConfig::default();
        if let Ok(path) = std::env::var("SEARCH_CHROMIUM") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                config.executable = Some(std::path::PathBuf::from(trimmed));
            }
        }
        if let Ok(v) = std::env::var("SEARCH_CHROMIUM_NO_SANDBOX") {
            if matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes") {
                tracing::warn!(
                    "SEARCH_CHROMIUM_NO_SANDBOX is set: Chromium runs without its sandbox. Use \
                     this only in a trusted container (it is required to run Chromium as root)."
                );
                config.no_sandbox = true;
            }
        }
        Self::launch_with(config).await
    }

    /// Launch with an explicit configuration.
    pub async fn launch_with(config: ChromiumConfig) -> Result<Self, BrowserError> {
        let mut builder = BrowserConfig::builder();
        if !config.headless {
            builder = builder.with_head(); // `with_head` = headful window; omitted ⇒ headless.
        }
        if let Some(exe) = config.executable {
            builder = builder.chrome_executable(exe);
        }
        // A private profile per process: see `ChromiumConfig::user_data_dir`.
        let profile = config.user_data_dir.unwrap_or_else(|| {
            std::env::temp_dir().join(format!("search-browser-profile-{}", std::process::id()))
        });
        builder = builder.user_data_dir(profile);
        if config.no_sandbox {
            // `--disable-dev-shm-usage` is the companion flag containers need: /dev/shm is small
            // there, and a full one crashes the renderer mid-navigation.
            builder = builder.no_sandbox().args(["--disable-dev-shm-usage"]);
        }
        let cfg = builder
            .build()
            .map_err(|e| BrowserError::Navigation(format!("browser config: {e}")))?;

        let (browser, mut handler) = Browser::launch(cfg)
            .await
            .map_err(|e| BrowserError::Navigation(format!("launch: {e}")))?;

        // Drive the CDP handler until the browser closes.
        //
        // An individual protocol error must NOT end this loop: the handler is what completes
        // every pending request, so exiting on the first `Err` cancels them all — the symptom is
        // `goto: oneshot canceled` on a navigation that Chromium would otherwise have served.
        // Only the stream ending (`None`, i.e. the browser went away) stops it.
        let handle = tokio::spawn(async move {
            while let Some(ev) = handler.next().await {
                if let Err(e) = ev {
                    tracing::debug!(error = %e, "cdp event error (continuing)");
                }
            }
        });

        Ok(Self {
            browser: Arc::new(browser),
            _handler: handle,
        })
    }
}

#[async_trait]
impl BrowserFetcher for ChromiumBrowser {
    async fn fetch(&self, url: &str, limits: BrowserLimits) -> Result<BrowserResult, BrowserError> {
        let start = Instant::now();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(BrowserError::Blocked(format!("unsupported url scheme: {url}")));
        }

        let budget = Duration::from_millis(limits.max_ms);
        // One persistent browser, one **short-lived isolated context per fetch** (§21): the
        // context is created, used and disposed here, so no cookies, storage or service worker
        // survives between pages. `create_browser_context` is `&self`, so the shared
        // `Arc<Browser>` is enough — `start_incognito_context` would need `&mut Browser` and
        // would put the incognito flag on the browser itself rather than per fetch.
        let work = async {
            let context_id = self
                .browser
                .create_browser_context(CreateBrowserContextParams::default())
                .await
                .map_err(|e| BrowserError::Navigation(format!("browser context: {e}")))?;
            let mut target = CreateTargetParams::new("about:blank");
            target.browser_context_id = Some(context_id.clone());
            let page = self
                .browser
                .new_page(target)
                .await
                .map_err(|e| BrowserError::Navigation(format!("new_page: {e}")))?;

            // Observe XHR/fetch requests (§22).
            let mut xhr = page
                .event_listener::<EventRequestWillBeSent>()
                .await
                .map_err(|e| BrowserError::Navigation(format!("listener: {e}")))?;

            // Block media when requested (keeps renders fast and the DOM small).
            if limits.block_media {
                let blocked = vec![
                    "*.png".to_string(),
                    "*.jpg".to_string(),
                    "*.jpeg".to_string(),
                    "*.gif".to_string(),
                    "*.webp".to_string(),
                    "*.mp4".to_string(),
                    "*.webm".to_string(),
                    "*.woff".to_string(),
                    "*.woff2".to_string(),
                ];
                let _ = page.execute(SetBlockedUrLsParams::new(blocked)).await;
            }

            page.goto(url)
                .await
                .map_err(|e| BrowserError::Navigation(format!("goto: {e}")))?;

            let html = page
                .content()
                .await
                .map_err(|e| BrowserError::Navigation(format!("content: {e}")))?;
            let final_url = page.url().await.ok().flatten().unwrap_or_else(|| url.to_string());
            let status = 200u16; // chromiumoxide exposes no direct status for the main doc here.

            // Drain XHR/fetch event URLs without blocking on the live stream.
            let mut endpoints: Vec<String> = Vec::new();
            loop {
                match tokio::time::timeout(Duration::from_millis(25), xhr.next()).await {
                    Ok(Some(ev)) => {
                        // The CDP field is generated as a raw identifier (`r#type`), not `type_`.
                        let is_data = matches!(
                            ev.r#type,
                            Some(ResourceType::Xhr) | Some(ResourceType::Fetch)
                        );
                        if is_data {
                            endpoints.push(ev.request.url.clone());
                        }
                    }
                    _ => break,
                }
            }

            // Dispose the whole context: it takes the page, its cookies and its storage with it.
            let _ = self.browser.dispose_browser_context(context_id).await;

            Ok::<_, BrowserError>(BrowserResult {
                final_url,
                html,
                status,
                json_endpoints: dedup_endpoints(&endpoints),
                elapsed_ms: start.elapsed().as_millis() as u64,
            })
        };

        let mut result = tokio::time::timeout(budget, work)
            .await
            .map_err(|_| BrowserError::Timeout(limits.max_ms))??;

        if result.html.len() > limits.max_dom_bytes {
            return Err(BrowserError::DomTooLarge(result.html.len()));
        }
        result.elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(result)
    }
}
