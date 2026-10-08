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
use chromiumoxide::cdp::browser_protocol::network::{
    EventRequestWillBeSent, ResourceType, SetBlockedUrlsParams,
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
}

impl Default for ChromiumConfig {
    fn default() -> Self {
        Self {
            headless: true,
            executable: None,
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
    /// Launch with defaults (headless, discovered executable).
    pub async fn launch() -> Result<Self, BrowserError> {
        Self::launch_with(ChromiumConfig::default()).await
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
        let cfg = builder
            .build()
            .map_err(|e| BrowserError::Navigation(format!("browser config: {e}")))?;

        let (browser, mut handler) = Browser::launch(cfg)
            .await
            .map_err(|e| BrowserError::Navigation(format!("launch: {e}")))?;

        // Drive the CDP handler until the browser closes.
        let handle = tokio::spawn(async move {
            while let Some(ev) = handler.next().await {
                if ev.is_err() {
                    break;
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
        let work = async {
            // Short-lived isolated context (§21).
            let context = self
                .browser
                .start_incognito_context()
                .await
                .map_err(|e| BrowserError::Navigation(format!("incognito: {e}")))?;
            let page = context
                .new_page("about:blank")
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
                let _ = page.execute(SetBlockedUrlsParams::new(blocked)).await;
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
                    Ok(Some(Ok(ev))) => {
                        let is_data = matches!(
                            ev.type_,
                            Some(ResourceType::Xhr) | Some(ResourceType::Fetch)
                        );
                        if is_data {
                            endpoints.push(ev.request.url.clone());
                        }
                    }
                    _ => break,
                }
            }

            let _ = context.close().await;

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
