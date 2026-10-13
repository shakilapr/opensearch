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
/// The project's primary provider is crowbot; crowbot is the fallback. Only a *failure* falls over
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

/// One model to try on the search path: a provider label for reporting, the model id, and the
/// transport that reaches it.
pub struct ChainEntry {
    /// Short provider label for reporting (e.g. `openrouter`, `crowbot`). Never a credential.
    pub provider: String,
    /// The model id this entry asks for.
    pub model: String,
    transport: Arc<dyn Transport>,
}

impl ChainEntry {
    /// Build an entry. `provider` is the reporting label derived from the base URL.
    pub fn new(provider: impl Into<String>, model: impl Into<String>, transport: Arc<dyn Transport>) -> Self {
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
/// This is the "if the free model fails, try another free model" policy made visible: every
/// attempt is logged (`WARN` naming the provider, model and the provider's own error; `INFO` for
/// the one that answered) and recorded, so the caller can report which models were tried, which
/// failed and which was used. A chain of one entry behaves exactly like a single transport.
pub struct ChainTransport {
    entries: Vec<ChainEntry>,
    attempts: std::sync::Mutex<Vec<search_core::LlmAttempt>>,
}

impl ChainTransport {
    /// Build a chain over `entries`, tried in the order given.
    pub fn new(entries: Vec<ChainEntry>) -> Self {
        Self {
            entries,
            attempts: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// The attempts of the most recent [`chat`](Transport::chat) call, in order.
    pub fn last_attempts(&self) -> Vec<search_core::LlmAttempt> {
        self.attempts
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    fn record(&self, attempts: Vec<search_core::LlmAttempt>) {
        if let Ok(mut guard) = self.attempts.lock() {
            *guard = attempts;
        }
    }
}

#[async_trait]
impl Transport for ChainTransport {
    /// `model` is ignored: each entry answers with **its own** model id (the entries may live on
    /// different providers, which do not share model names).
    async fn chat(&self, _model: &str, prompt: &str) -> Result<String, AnswerError> {
        // A chain with no entries cannot answer; make the misconfiguration explicit rather than
        // returning an empty body.
        if self.entries.is_empty() {
            return Err(AnswerError::Config("the model chain is empty".to_string()));
        }

        let mut attempts: Vec<search_core::LlmAttempt> = Vec::with_capacity(self.entries.len());
        for entry in &self.entries {
            match entry.transport.chat(&entry.model, prompt).await {
                Ok(body) => {
                    tracing::info!(
                        provider = %entry.provider,
                        model = %entry.model,
                        tried = attempts.len() + 1,
                        "llm: answered"
                    );
                    attempts.push(search_core::LlmAttempt::ok(&entry.provider, &entry.model));
                    self.record(attempts);
                    return Ok(body);
                }
                Err(err) => {
                    tracing::warn!(
                        provider = %entry.provider,
                        model = %entry.model,
                        error = %err,
                        "llm: model failed; trying the next one"
                    );
                    attempts.push(search_core::LlmAttempt::failed(
                        &entry.provider,
                        &entry.model,
                        err.to_string(),
                    ));
                }
            }
        }

        self.record(attempts.clone());
        let tried = attempts
            .iter()
            .map(|a| a.summary())
            .collect::<Vec<_>>()
            .join("; ");
        Err(AnswerError::Http(format!(
            "all {} model(s) failed: {tried}",
            attempts.len()
        )))
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
            // Load-bearing: crowbot (the fallback) 403s with no User-Agent (Cloudflare 1010).
            .user_agent(crate::config::llm_user_agent())
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

    // ---- the model chain (free models first, next one on failure) ----

    #[test]
    fn provider_label_reads_the_host_without_the_scheme_or_path() {
        assert_eq!(provider_label("https://api.crowbot.sh/v1"), "crowbot");
        assert_eq!(provider_label("https://openrouter.ai/api/v1"), "openrouter");
        assert_eq!(provider_label("https://api.crowbot.sh/v1"), "crowbot");
    }

    #[tokio::test]
    async fn a_failing_free_model_moves_to_the_next_one() {
        let first = Fake::new(true);
        let second = Fake::new(false);
        let chain = ChainTransport::new(vec![
            ChainEntry::new("openrouter", "free-a:free", first.clone()),
            ChainEntry::new("openrouter", "free-b:free", second.clone()),
        ]);

        let out = chain.chat("ignored", "p").await.expect("the second answers");

        assert_eq!(out, "ok:free-b:free");
        assert_eq!(first.calls.load(Ordering::SeqCst), 1);
        assert_eq!(second.seen.lock().unwrap().as_slice(), ["free-b:free"]);
        let attempts = chain.last_attempts();
        assert_eq!(attempts.len(), 2);
        assert!(!attempts[0].ok, "the first model failed");
        assert_eq!(attempts[0].model, "free-a:free");
        assert_eq!(attempts[0].error.as_deref(), Some("transport error: primary is down"));
        assert!(attempts[1].ok, "the second model answered");
        assert_eq!(attempts[1].model, "free-b:free");
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
            "a working free model must not spend a call on the paid fallback"
        );
        assert_eq!(chain.last_attempts().len(), 1);
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
        let attempts = chain.last_attempts();
        assert_eq!(attempts.len(), 3);
        assert!(attempts.iter().all(|a| !a.ok));
    }

    #[tokio::test]
    async fn an_empty_chain_is_a_config_error_not_a_silent_empty_answer() {
        let chain = ChainTransport::new(Vec::new());

        let err = chain.chat("ignored", "p").await.expect_err("no entries");

        assert!(matches!(err, AnswerError::Config(_)), "{err:?}");
        assert!(chain.last_attempts().is_empty());
    }
}
