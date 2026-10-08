//! HTTP fetching with streaming limits (spec §8). OWNER: fetcher agent.
//!
//! The client never follows redirects automatically: every hop is
//! SSRF-validated (with DNS re-resolution) before the next request is sent.
//! Bodies are streamed and aborted mid-stream when they exceed a hard cap.

use std::time::Duration;

use search_core::{AppError, AppResult, Limits};

use crate::canon;
use crate::crawlpolic::content_type_allowed;
use crate::ssrf;
use crate::validators::{ValidatorStore, Validators};

/// Default per-request timeout when the caller does not set one.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Default accepted content types. A source's CrawlPolicy can override this via
/// [`Fetcher::with_allowed_types`]; an empty list accepts everything.
const DEFAULT_ALLOWED_TYPES: &[&str] = &["text/*", "application/xhtml+xml", "application/json"];

pub struct Fetcher {
    client: reqwest::Client,
    limits: Limits,
    user_agent: String,
    timeout: Duration,
    /// Empty means "accept any content type".
    allowed_types: Vec<String>,
    /// TEST-ONLY: when set, loopback/private addresses are not rejected by the
    /// SSRF guard. Default `false` (production-secure).
    allow_private_for_tests: bool,
}

/// The result of one fetch attempt. A fetch does NOT imply a new
/// DocumentVersion: `304` carries no new body.
#[derive(Debug, Clone)]
pub struct FetchOutcome {
    pub final_url: String,
    pub status: u16,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub body: Option<Vec<u8>>,
    pub content_type: Option<String>,
}

/// Outcome of a conditional fetch ([`Fetcher::fetch_conditional`]).
///
/// When the server answers `304 Not Modified` the representation is
/// [`Fetched::Unchanged`]: no body is read and nothing needs re-parsing or
/// re-indexing. A resolved `2xx` is [`Fetched::Modified`] with a fresh body and
/// updated validators. Any other terminal status is [`Fetched::Other`] (no body).
#[derive(Debug, Clone)]
pub enum Fetched {
    /// `304 Not Modified`: the stored representation is still current. The
    /// `validators` carry the (possibly refreshed) validators plus a new
    /// `fetched_at` revalidation timestamp.
    Unchanged {
        final_url: String,
        validators: Validators,
    },
    /// A `2xx` response with a fresh body. `validators` are the new values read
    /// from the response (and already stored), with a fresh `fetched_at`.
    Modified {
        final_url: String,
        status: u16,
        content_type: Option<String>,
        body: Vec<u8>,
        validators: Validators,
    },
    /// Any other terminal status (e.g. `404`, `500`, or a redirect that ran out
    /// of hops). No body is read or stored.
    Other {
        final_url: String,
        status: u16,
        content_type: Option<String>,
    },
}

impl Fetcher {
    /// Build a fetcher with the default text-oriented content-type allow-list
    /// and a default per-request timeout.
    pub fn new(limits: Limits, user_agent: String) -> Self {
        Self::build(limits, user_agent, DEFAULT_TIMEOUT_MS, allowed_from(DEFAULT_ALLOWED_TYPES))
    }

    fn build(limits: Limits, user_agent: String, timeout_ms: u64, allowed_types: Vec<String>) -> Self {
        let timeout = Duration::from_millis(timeout_ms);
        let client = reqwest::Client::builder()
            .user_agent(user_agent.clone())
            // Redirects are handled manually so each hop is SSRF-validated.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            limits,
            user_agent,
            timeout,
            allowed_types,
            allow_private_for_tests: false,
        }
    }

    /// TEST-ONLY escape hatch: allow loopback/private addresses past the SSRF
    /// guard so the fetcher can talk to a local test server. Never enable this
    /// in production code.
    #[doc(hidden)]
    pub fn allow_private_addresses_for_tests(mut self, allow: bool) -> Self {
        self.allow_private_for_tests = allow;
        self
    }

    /// Override the accepted content-type list (comma-separated allow-list
    /// semantics; empty = accept all).
    pub fn with_allowed_types(mut self, allowed: Vec<String>) -> Self {
        self.allowed_types = allowed;
        self
    }

    /// Override the per-request timeout in milliseconds.
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout = Duration::from_millis(timeout_ms);
        self
    }

    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Fetch a URL, following redirects manually (each hop re-validated for
    /// SSRF). Uses conditional GET when validators are supplied.
    pub async fn fetch(
        &self,
        url: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> AppResult<FetchOutcome> {
        let mut current = url::Url::parse(url)
            .map_err(|e| AppError::Network(format!("invalid url {url:?}: {e}")))?;
        let mut redirects: u32 = 0;

        loop {
            // Validate this hop: scheme, literal IP, and every resolved IP.
            self.validate_hop(current.as_str()).await?;

            let mut req = self.client.get(current.clone());
            if let Some(tag) = etag {
                req = req.header(reqwest::header::IF_NONE_MATCH, tag);
            }
            if let Some(lm) = last_modified {
                req = req.header(reqwest::header::IF_MODIFIED_SINCE, lm);
            }

            let resp = req.send().await.map_err(net_err)?;
            let status = resp.status();

            // 304 is final (not a redirect to follow).
            if status.as_u16() == 304 {
                return self.finish(resp, 304, current).await;
            }

            if status.is_redirection() {
                if redirects >= self.limits.max_redirects {
                    return Err(AppError::LimitExceeded(format!(
                        "exceeded max_redirects ({}) fetching {url}",
                        self.limits.max_redirects
                    )));
                }
                let location = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| AppError::Network("redirect response without Location".into()))?;
                current = current
                    .join(location)
                    .map_err(|e| AppError::Network(format!("bad redirect Location {location:?}: {e}")))?;
                redirects += 1;
                continue;
            }

            return self.finish(resp, status.as_u16(), current).await;
        }
    }

    /// Validate one hop. In production this resolves DNS and re-checks every
    /// address; with the test escape hatch it still checks the scheme but skips
    /// the IP/host blocks.
    async fn validate_hop(&self, url: &str) -> AppResult<()> {
        if self.allow_private_for_tests {
            // Still enforce the scheme allow-list; skip the IP/host blocks so a
            // loopback test server is reachable.
            let u = url::Url::parse(url)
                .map_err(|e| AppError::SsrfBlocked(format!("invalid url {url:?}: {e}")))?;
            match u.scheme() {
                "http" | "https" => Ok(()),
                other => Err(AppError::SsrfBlocked(format!(
                    "scheme {other:?} is not allowed (http/https only)"
                ))),
            }
        } else {
            ssrf::validate_url_resolved(url).await.map(|_| ())
        }
    }

    /// Conditional fetch (crawler §18): look up stored validators for the
    /// canonical form of `url`, send `If-None-Match` / `If-Modified-Since`
    /// when known, and interpret the response.
    ///
    /// * `304 Not Modified` → [`Fetched::Unchanged`]: **no body is read** and
    ///   nothing needs re-parsing/re-indexing. Validators are refreshed with a
    ///   new `fetched_at`.
    /// * resolved `2xx` → [`Fetched::Modified`] with the capped body; the new
    ///   `ETag`/`Last-Modified` are stored for the next revalidation.
    /// * anything else → [`Fetched::Other`] (no body).
    ///
    /// The same byte caps, timeout, manual SSRF-validated redirects and
    /// transparent decompression bounds as [`Fetcher::fetch`] apply. Validators
    /// are only sent on the first hop (they belong to the canonical resource,
    /// not to whatever a redirect points at).
    pub async fn fetch_conditional(
        &self,
        store: &dyn ValidatorStore,
        url: &str,
    ) -> AppResult<Fetched> {
        // Validators are keyed by canonical URL (append the observed final URL
        // to the store only after a successful 2xx fetch, see below).
        let key = canon::canonicalize(url).unwrap_or_else(|_| url.to_string());
        let prior = store.get(&key);

        let mut current =
            url::Url::parse(url).map_err(|e| AppError::Network(format!("invalid url {url:?}: {e}")))?;
        let mut redirects: u32 = 0;

        loop {
            self.validate_hop(current.as_str()).await?;

            let mut req = self.client.get(current.clone());
            // Conditional headers only on the first hop.
            if redirects == 0 {
                if let Some(v) = prior.as_ref() {
                    if let Some(tag) = v.etag.as_deref() {
                        req = req.header(reqwest::header::IF_NONE_MATCH, tag);
                    }
                    if let Some(lm) = v.last_modified.as_deref() {
                        req = req.header(reqwest::header::IF_MODIFIED_SINCE, lm);
                    }
                }
            }

            let resp = req.send().await.map_err(net_err)?;
            let status = resp.status();
            let final_url = current.to_string();

            if status.as_u16() == 304 {
                // Merge: 304 responses may repeat or omit validators. Fall back
                // to the previously stored values, then stamp a fresh time.
                let etag = header_string(&resp, reqwest::header::ETAG)
                    .or_else(|| prior.as_ref().and_then(|v| v.etag.clone()));
                let last_modified = header_string(&resp, reqwest::header::LAST_MODIFIED)
                    .or_else(|| prior.as_ref().and_then(|v| v.last_modified.clone()));
                let validators = Validators::new(etag, last_modified);
                store.put(&key, validators.clone());
                return Ok(Fetched::Unchanged {
                    final_url,
                    validators,
                });
            }

            if status.is_redirection() {
                if redirects >= self.limits.max_redirects {
                    return Err(AppError::LimitExceeded(format!(
                        "exceeded max_redirects ({}) fetching {url}",
                        self.limits.max_redirects
                    )));
                }
                let location = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| AppError::Network("redirect response without Location".into()))?;
                current = current
                    .join(location)
                    .map_err(|e| AppError::Network(format!("bad redirect Location {location:?}: {e}")))?;
                redirects += 1;
                continue;
            }

            let content_type = header_string(&resp, reqwest::header::CONTENT_TYPE);

            if !(200..300).contains(&status.as_u16()) {
                return Ok(Fetched::Other {
                    final_url,
                    status: status.as_u16(),
                    content_type,
                });
            }

            // Content-type gate (same local policy override as `fetch`).
            if !self.allowed_types.is_empty() {
                let joined = self.allowed_types.join(",");
                let seen = content_type.clone().unwrap_or_default();
                if !content_type_allowed(&joined, &seen) {
                    return Err(AppError::CrawlPolicyDenied(format!(
                        "content-type {seen:?} is not accepted"
                    )));
                }
            }

            let etag = header_string(&resp, reqwest::header::ETAG);
            let last_modified = header_string(&resp, reqwest::header::LAST_MODIFIED);
            let body = read_body_capped(resp, &self.limits).await?;
            let validators = Validators::new(etag, last_modified);
            store.put(&key, validators.clone());
            return Ok(Fetched::Modified {
                final_url,
                status: status.as_u16(),
                content_type,
                body,
                validators,
            });
        }
    }

    async fn finish(
        &self,
        resp: reqwest::Response,
        status: u16,
        final_url: url::Url,
    ) -> AppResult<FetchOutcome> {
        let etag = header_string(&resp, reqwest::header::ETAG);
        let last_modified = header_string(&resp, reqwest::header::LAST_MODIFIED);
        let content_type = header_string(&resp, reqwest::header::CONTENT_TYPE);

        if status == 304 {
            return Ok(FetchOutcome {
                final_url: final_url.to_string(),
                status,
                etag,
                last_modified,
                body: None,
                content_type,
            });
        }

        if !(200..300).contains(&status) {
            return Ok(FetchOutcome {
                final_url: final_url.to_string(),
                status,
                etag,
                last_modified,
                body: None,
                content_type,
            });
        }

        // Content-type gate (local policy override list).
        if !self.allowed_types.is_empty() {
            let joined = self.allowed_types.join(",");
            let seen = content_type.clone().unwrap_or_default();
            if !content_type_allowed(&joined, &seen) {
                return Err(AppError::CrawlPolicyDenied(format!(
                    "content-type {seen:?} is not accepted"
                )));
            }
        }

        let body = read_body_capped(resp, &self.limits).await?;
        Ok(FetchOutcome {
            final_url: final_url.to_string(),
            status,
            etag,
            last_modified,
            body: Some(body),
            content_type,
        })
    }
}

/// Stream a response body into memory, aborting mid-stream with
/// [`AppError::LimitExceeded`] as soon as either the raw response cap
/// (`max_response_bytes`) or the decompressed cap (`max_decompressed_bytes`) is
/// exceeded. Decompression (gzip/br) is performed transparently by the client;
/// these caps are enforced on the bytes we actually read, never on remote
/// `Content-Length`.
pub async fn read_body_capped(mut resp: reqwest::Response, limits: &Limits) -> AppResult<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await.map_err(net_err)? {
            Some(chunk) => {
                buf.extend_from_slice(&chunk);
                let n = buf.len() as u64;
                if n > limits.max_response_bytes {
                    return Err(AppError::LimitExceeded(format!(
                        "response body exceeds max_response_bytes ({})",
                        limits.max_response_bytes
                    )));
                }
                if n > limits.max_decompressed_bytes {
                    return Err(AppError::LimitExceeded(format!(
                        "response body exceeds max_decompressed_bytes ({})",
                        limits.max_decompressed_bytes
                    )));
                }
            }
            None => break,
        }
    }
    Ok(buf)
}

fn header_string(resp: &reqwest::Response, name: reqwest::header::HeaderName) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

fn net_err(e: reqwest::Error) -> AppError {
    AppError::Network(e.to_string())
}

fn allowed_from(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// Map a robots.txt HTTP status onto the RFC 9309 failure semantics.
pub fn robots_status_for_http(code: u16) -> crate::robots::RobotsStatus {
    crate::robots::RobotsStatus::from_http_status(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_fetcher_builds() {
        let f = Fetcher::new(Limits::default(), "TestBot/0.1".into());
        assert_eq!(f.user_agent(), "TestBot/0.1");
        assert_eq!(f.limits().max_response_bytes, 5 * 1024 * 1024);
    }
}
