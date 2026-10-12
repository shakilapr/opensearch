//! The injectable LLM transport.
//!
//! Production uses [`HttpTransport`] (OpenAI-chat-compatible `POST {base}/chat/completions`);
//! tests supply their own [`Transport`] so the whole crate is hermetic — no network, no
//! credential, no sleeps.
//!
//! NOTE: a transport returns the **raw response body** (JSON text). The client parses it with
//! [`crate::prompt::parse_chat_response`], so the `finish_reason`/empty-content handling is
//! exercised by a fake transport in tests rather than hiding inside the HTTP layer.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::config::AnswerConfig;
use crate::error::AnswerError;

/// A single chat completion. Injected so tests can drive the answerer with canned bodies.
#[async_trait]
pub trait Transport: Send + Sync {
    /// POST `prompt` to `model` and return the raw response body (JSON text).
    async fn chat(&self, model: &str, prompt: &str) -> Result<String, AnswerError>;
}

/// A transport that retries on a **second provider** when the primary fails.
///
/// The project's primary provider is kitani; cun.ai is the fallback. Only a *failure* falls over
/// — a successful primary call is returned untouched, so the fallback costs nothing in the normal
/// case. The fallback is asked with its **own** model id (the vendors do not share model names).
/// With no fallback credential configured the primary is the only transport, so a single-provider
/// deployment behaves exactly as before.
///
/// A decorator over two `Transport`s rather than HTTP-specific, so the fail-over policy is
/// exercised by hermetic fakes (no network, no credential).
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
    async fn chat(&self, model: &str, prompt: &str) -> Result<String, AnswerError> {
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
                        AnswerError::Http(format!(
                            "primary provider failed ({primary_err}); \
                             fallback provider failed ({fallback_err})"
                        ))
                    })
            }
        }
    }
}

/// Real HTTP transport: `POST {base_url}/chat/completions` with a bearer token.
///
/// The API key is held only in memory (moved in at construction) and is never logged or embedded
/// in an error message.
pub(crate) struct HttpTransport {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    max_tokens: u32,
}

impl HttpTransport {
    pub(crate) fn new(cfg: &AnswerConfig, api_key: String) -> Result<Self, AnswerError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(cfg.timeout_ms.max(1)))
            .build()
            .map_err(|e| AnswerError::Http(e.to_string()))?;
        Ok(Self {
            client,
            base_url: cfg.base_url.clone(),
            api_key,
            max_tokens: cfg.max_tokens,
        })
    }
}

/// Token budget for `model`. Reasoning models spend part of the budget on a hidden trace before
/// the answer; with a small budget they finish with `finish_reason=length` and null content.
fn token_budget(model: &str, configured: u32) -> u32 {
    let m = model.to_ascii_lowercase();
    if m.contains("thinking") || m.contains("reason") || m.contains("r1") || m.contains("o1") {
        configured.max(8192)
    } else {
        configured
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn chat(&self, model: &str, prompt: &str) -> Result<String, AnswerError> {
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
            .map_err(|e| {
                if e.is_timeout() {
                    AnswerError::Timeout(format!("request to {url} timed out: {e}"))
                } else {
                    AnswerError::Http(format!("request to {url} failed: {e}"))
                }
            })?;

        let status = resp.status();
        if !status.is_success() {
            return Err(AnswerError::Http(format!(
                "chat completions returned HTTP {status}"
            )));
        }

        resp.text()
            .await
            .map_err(|e| AnswerError::Http(format!("failed to read response body: {e}")))
    }
}

#[cfg(test)]
mod fallback_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Fails (or succeeds) and records the model ids it was asked with.
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
        async fn chat(&self, model: &str, _prompt: &str) -> Result<String, AnswerError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().unwrap().push(model.to_string());
            if self.fail {
                Err(AnswerError::Http("primary is down".into()))
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
