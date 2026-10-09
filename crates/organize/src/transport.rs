//! The injectable LLM transport.
//!
//! Production uses [`HttpTransport`] (OpenAI-chat-compatible `POST {base}/chat/completions`);
//! tests supply their own [`Transport`] so the whole crate is hermetic — no network, no
//! credential, no sleeps.

use std::time::Duration;

use async_trait::async_trait;

use crate::error::OrganizeError;
use crate::config::LlmConfig;

/// A single chat completion. Injected so tests can drive the organizer with canned responses.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Ask `model` to complete `prompt`, returning the assistant message content.
    async fn chat(&self, model: &str, prompt: &str) -> Result<String, OrganizeError>;
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
