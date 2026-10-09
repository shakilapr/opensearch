//! The injectable LLM transport.
//!
//! Production uses [`HttpTransport`] (OpenAI-chat-compatible `POST {base}/chat/completions`);
//! tests supply their own [`Transport`] so the whole crate is hermetic — no network, no
//! credential, no sleeps.
//!
//! NOTE: a transport returns the **raw response body** (JSON text). The client parses it with
//! [`crate::prompt::parse_chat_response`], so the `finish_reason`/empty-content handling is
//! exercised by a fake transport in tests rather than hiding inside the HTTP layer.

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
