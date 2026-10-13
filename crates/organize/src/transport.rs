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

/// One model to try: a provider label for reporting, the model id, and the transport reaching it.
pub struct ChainEntry {
    /// Short provider label for reporting (e.g. `openrouter`, `crowbot`). Never a credential.
    pub provider: String,
    /// The model id this entry asks for.
    pub model: String,
    transport: Arc<dyn Transport>,
}

impl ChainEntry {
    /// Build an entry. `provider` is the reporting label derived from the base URL.
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
        transport: Arc<dyn Transport>,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            transport,
        }
    }
}

/// Derive a short, credential-free provider label from a base URL (`https://api.crowbot.sh/v1` →
/// `crowbot`).
pub fn provider_label(base_url: &str) -> String {
    let host = base_url
        .split("://")
        .nth(1)
        .unwrap_or(base_url)
        .split('/')
        .next()
        .unwrap_or(base_url);
    let host = host.trim_start_matches("www.").trim_start_matches("api.");
    host.split('.').next().unwrap_or(host).to_string()
}

/// Tries each entry **in order** and returns the first completion that works.
///
/// The planner's "free models first, and if one fails try another" policy: a model that fails —
/// rate limit, capacity, timeout — moves to the next entry instead of losing the whole plan (which
/// today means zero expansion probes and a visibly worse search). Every attempt is logged (`WARN`
/// naming the provider, model and the provider's own error; `INFO` for the one that answered).
pub struct ChainTransport {
    entries: Vec<ChainEntry>,
}

impl ChainTransport {
    /// Build a chain over `entries`, tried in the order given.
    pub fn new(entries: Vec<ChainEntry>) -> Self {
        Self { entries }
    }
}

#[async_trait]
impl Transport for ChainTransport {
    /// `model` is ignored: each entry answers with **its own** model id (entries may live on
    /// different providers, which do not share model names).
    async fn chat(&self, _model: &str, prompt: &str) -> Result<String, OrganizeError> {
        if self.entries.is_empty() {
            return Err(OrganizeError::Config("the model chain is empty".to_string()));
        }

        let mut tried: Vec<String> = Vec::with_capacity(self.entries.len());
        for entry in &self.entries {
            match entry.transport.chat(&entry.model, prompt).await {
                Ok(body) => {
                    tracing::info!(
                        provider = %entry.provider,
                        model = %entry.model,
                        tried = tried.len() + 1,
                        "planner llm: answered"
                    );
                    return Ok(body);
                }
                Err(err) => {
                    tracing::warn!(
                        provider = %entry.provider,
                        model = %entry.model,
                        error = %err,
                        "planner llm: model failed; trying the next one"
                    );
                    tried.push(format!("{} {} failed ({err})", entry.provider, entry.model));
                }
            }
        }

        Err(OrganizeError::Http(format!(
            "all {} model(s) failed: {}",
            tried.len(),
            tried.join("; ")
        )))
    }
}

/// A transport that retries on a **second provider** when the primary fails.
///
/// The project's primary provider is crowbot; crowbot is the fallback. Only a *failure* falls over
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

    // ---- the model chain (free models first, next one on failure) ----

    #[test]
    fn provider_label_reads_the_host_without_the_scheme_or_path() {
        assert_eq!(provider_label("https://api.crowbot.sh/v1"), "crowbot");
        assert_eq!(provider_label("https://openrouter.ai/api/v1"), "openrouter");
        assert_eq!(provider_label("https://api.crowbot.sh/v1"), "crowbot");
    }

    #[tokio::test]
    async fn a_failing_model_moves_to_the_next_one() {
        let first = Fake::new(true);
        let second = Fake::new(false);
        let chain = ChainTransport::new(vec![
            ChainEntry::new("openrouter", "free-a:free", first.clone()),
            ChainEntry::new("crowbot", "deepseek-v4.1-flash", second.clone()),
        ]);

        let out = chain.chat("ignored", "p").await.expect("the second answers");

        assert_eq!(out, "ok:deepseek-v4.1-flash");
        assert_eq!(first.calls.load(Ordering::SeqCst), 1);
        assert_eq!(second.seen.lock().unwrap().as_slice(), ["deepseek-v4.1-flash"]);
    }

    #[tokio::test]
    async fn a_working_first_model_stops_the_chain() {
        let first = Fake::new(false);
        let second = Fake::new(false);
        let chain = ChainTransport::new(vec![
            ChainEntry::new("openrouter", "free-a:free", first.clone()),
            ChainEntry::new("crowbot", "deepseek-v4.1-flash", second.clone()),
        ]);

        let out = chain.chat("ignored", "p").await.expect("the first answers");

        assert_eq!(out, "ok:free-a:free");
        assert_eq!(
            second.calls.load(Ordering::SeqCst),
            0,
            "a working free model must not spend a call on the fallback"
        );
    }

    #[tokio::test]
    async fn every_model_failing_reports_each_one_by_name() {
        let chain = ChainTransport::new(vec![
            ChainEntry::new("openrouter", "free-a:free", Fake::new(true)),
            ChainEntry::new("openrouter", "free-b:free", Fake::new(true)),
            ChainEntry::new("crowbot", "deepseek-v4.1-flash", Fake::new(true)),
        ]);

        let err = chain.chat("ignored", "p").await.expect_err("all fail");

        let msg = err.to_string();
        assert!(msg.contains("all 3 model(s) failed"), "{msg}");
        for model in ["free-a:free", "free-b:free", "deepseek-v4.1-flash"] {
            assert!(msg.contains(model), "the error must name {model}: {msg}");
        }
    }

    #[tokio::test]
    async fn an_empty_chain_is_a_config_error_not_a_silent_empty_plan() {
        let chain = ChainTransport::new(Vec::new());

        let err = chain.chat("ignored", "p").await.expect_err("no entries");

        assert!(matches!(err, OrganizeError::Config(_)), "{err:?}");
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
            // A User-Agent is load-bearing: the crowbot fallback 403s (Cloudflare 1010) on a request
            // that carries none. See `config::DEFAULT_LLM_USER_AGENT`.
            .user_agent(crate::config::llm_user_agent())
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
/// Reasoning models (id containing `thinking`, `reason`, `r1` or `o1`) spend part of the budget on a
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
            // Explicit, not the default: a relay may stream unless told otherwise, and a streamed
            // body is SSE text, not the JSON this transport parses. The apizio relay streams by
            // default — omitting this returned a body that failed to parse with a JSON error.
            "stream": false,
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
        assert_eq!(token_budget("deepseek-v4.1-flash", 1024), 1024);
    }

    #[test]
    fn reasoning_models_get_room_for_the_hidden_trace() {
        // A synthetic id: the point is the mechanism (the marker substring), not any one vendor's
        // model. None of the configured providers serves a reasoning model, so naming a real id
        // here would pin the test to a name that does not detect.
        assert!(token_budget("some-model-thinking", 1024) >= 8192);
        assert!(token_budget("some-model-r1", 1024) >= 8192);
    }
}
