//! The injectable LLM transport.
//!
//! Production uses [`HttpTransport`] (OpenAI-chat-compatible `POST {base}/chat/completions`);
//! tests supply their own [`Transport`] so the whole crate is hermetic — no network, no
//! credential, no sleeps.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::config::LlmConfig;
use crate::error::OrganizeError;

/// A single chat completion. Injected so tests can drive the organizer with canned responses.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Ask `model` to complete `prompt`, returning the assistant message content.
    async fn chat(&self, model: &str, prompt: &str) -> Result<String, OrganizeError>;
}

/// A transport that retries on a **second provider** when the primary fails.
///
/// The project's primary provider is kitani; cun.ai is the fallback. Only a *failure* falls over
/// — a successful primary call is returned untouched, so the fallback costs nothing in the normal
/// case. The fallback is asked with its **own** model id (the vendors do not share model names).
/// Observed: an unset fallback credential disables this entirely, so a single-provider deployment
/// behaves exactly as before.
///
/// The transport is deliberately a decorator over two `Transport`s rather than HTTP-specific, so
/// the fail-over policy is exercised by hermetic fakes (no network, no credential).
pub struct FallbackTransport {
    primary: Arc<dyn Transport>,
    fallback: Arc<dyn Transport>,
    fallback_model: String,
}

impl FallbackTransport {
    /// Wrap two transports. `fallback_model` is what the fallback is asked with.
    pub fn new(
        primary: Arc<dyn Transport>,
        fallback: Arc<dyn Transport>,
        fallback_model: impl Into<String>,
    ) -> Self {
        Self {
            primary,
            fallback,
            fallback_model: fallback_model.into(),
        }
    }
}

#[async_trait]
impl Transport for FallbackTransport {
    async fn chat(&self, model: &str, prompt: &str) -> Result<String, OrganizeError> {
        match self.primary.chat(model, prompt).await {
            Ok(body) => Ok(body),
            Err(primary_err) => {
                tracing::warn!(
                    error = %primary_err,
                    fallback_model = %self.fallback_model,
                    "primary LLM provider failed; retrying on the fallback provider"
                );
                self.fallback
                    .chat(&self.fallback_model, prompt)
                    .await
                    .map_err(|fallback_err| {
                        OrganizeError::Http(format!(
                            "primary provider failed ({primary_err}); \
                             fallback provider failed ({fallback_err})"
                        ))
                    })
            }
        }
    }
}

#[cfg(test)]
mod fallback_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Fails (or succeeds) and records what it was asked.
    struct Fake {
        fail: bool,
        seen: Mutex<Vec<String>>,
        calls: AtomicUsize,
    }

    impl Fake {
        fn new(fail: bool) -> Arc<Self> {
            Arc::new(Self {
                fail,
                seen: Mutex::new(Vec::new()),
                calls: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl Transport for Fake {
        async fn chat(&self, model: &str, _prompt: &str) -> Result<String, OrganizeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().unwrap().push(model.to_string());
            if self.fail {
                Err(OrganizeError::Http("primary is down".into()))
            } else {
                Ok(format!("ok:{model}"))
            }
        }
    }

    #[tokio::test]
    async fn a_successful_primary_never_calls_the_fallback() {
        let primary = Fake::new(false);
        let fallback = Fake::new(false);
        let t = FallbackTransport::new(primary.clone(), fallback.clone(), "fallback-model");

        let out = t.chat("primary-model", "p").await.expect("primary succeeds");

        assert_eq!(out, "ok:primary-model");
        assert_eq!(primary.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fallback.calls.load(Ordering::SeqCst),
            0,
            "the fallback must not be touched when the primary works"
        );
    }

    #[tokio::test]
    async fn a_failed_primary_falls_over_to_the_fallback_model() {
        let primary = Fake::new(true);
        let fallback = Fake::new(false);
        let t = FallbackTransport::new(primary.clone(), fallback.clone(), "fallback-model");

        let out = t.chat("primary-model", "p").await.expect("fallback succeeds");

        assert_eq!(out, "ok:fallback-model");
        assert_eq!(primary.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fallback.seen.lock().unwrap().as_slice(),
            ["fallback-model"],
            "the fallback is asked with its own model id, not the primary's"
        );
    }

    #[tokio::test]
    async fn both_failing_reports_both_causes() {
        let t = FallbackTransport::new(Fake::new(true), Fake::new(true), "fallback-model");

        let err = t.chat("primary-model", "p").await.expect_err("both fail");

        let msg = err.to_string();
        assert!(msg.contains("primary provider failed"), "{msg}");
        assert!(msg.contains("fallback provider failed"), "{msg}");
    }
}
/// Real HTTP transport: `POST {base_url}/chat/completions` with a bearer token.
///
/// The API key is held only in memory (cloned from [`LlmConfig`] at construction) and is never
/// logged or embedded in error messages.
pub(crate) struct HttpTransport {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    max_tokens: u32,
}

impl HttpTransport {
    pub(crate) fn new(cfg: &LlmConfig, api_key: String) -> Result<Self, OrganizeError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_s.max(1)))
            .build()
            .map_err(|e| OrganizeError::Http(e.to_string()))?;
        Ok(Self {
            client,
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key,
            max_tokens: 1024,
        })
    }
}

/// Token budget for `model`.
///
/// Reasoning models (e.g. `deepseek/deepseek-v4.1-flash-thinking`) spend part of the budget on a
/// hidden reasoning trace *before* the answer. With a small budget they finish with
/// `finish_reason=length` and `content: null` — observed live. Give them room for both.
fn token_budget(model: &str, default: u32) -> u32 {
    let m = model.to_ascii_lowercase();
    if m.contains("thinking") || m.contains("reason") || m.contains("r1") || m.contains("o1") {
        default.max(8192)
    } else {
        default
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn chat(&self, model: &str, prompt: &str) -> Result<String, OrganizeError> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = serde_json::json!({
            "model": model,
            "messages": [{ "role": "user", "content": prompt }],
            "max_tokens": token_budget(model, self.max_tokens),
        });
        // The workspace's reqwest is built without the `json` feature, so encode by hand.
        let body_bytes = serde_json::to_vec(&body)?;

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body_bytes)
            .send()
            .await
            .map_err(|e| OrganizeError::Http(format!("request to {url} failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(OrganizeError::Http(format!(
                "chat completions returned HTTP {status}"
            )));
        }

        let text = resp
            .text()
            .await
            .map_err(|e| OrganizeError::Http(format!("failed to read response body: {e}")))?;
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| OrganizeError::Http(format!("invalid JSON response: {e}")))?;

        let choice = value.get("choices").and_then(|c| c.get(0));
        let content = choice
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .filter(|s| !s.trim().is_empty());
        match content {
            Some(s) => Ok(s.to_string()),
            None => {
                // Diagnose rather than guess: report finish_reason and the shape we got.
                let finish = choice
                    .and_then(|c| c.get("finish_reason"))
                    .and_then(|f| f.as_str())
                    .unwrap_or("?");
                let reasoning_len = choice
                    .and_then(|c| c.get("message"))
                    .and_then(|m| m.get("reasoning"))
                    .and_then(|r| r.as_str())
                    .map(str::len)
                    .unwrap_or(0);
                Err(OrganizeError::Http(format!(
                    "no assistant content (finish_reason={finish}, reasoning_chars={reasoning_len}); \
                     body: {}",
                    snippet(&text, 400)
                )))
            }
        }
    }
}

/// First `n` characters of `s`, for error diagnostics (never contains the API key).
fn snippet(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::token_budget;

    #[test]
    fn plain_models_keep_the_default_budget() {
        assert_eq!(token_budget("deepseek/deepseek-v4.1-flash", 1024), 1024);
    }

    #[test]
    fn reasoning_models_get_room_for_the_hidden_trace() {
        assert!(token_budget("deepseek/deepseek-v4.1-flash-thinking", 1024) >= 8192);
    }
}
