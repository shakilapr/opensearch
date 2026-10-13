//! `search-answer` — optional, opt-in query-time LLM answer generation.
//!
//! Implements [`search_engine::Answerer`] on top of an OpenAI-chat-compatible provider, reusing
//! the patterns proven in `search-organize`: env-driven config, an on-disk response cache keyed
//! by request (atomic writes), a bounded-concurrency fan-out, and an injectable [`Transport`] so
//! tests are hermetic (no network, no credential, no sleeps).
//!
//! Design guarantees:
//!
//! * **Opt-in.** [`from_env`] returns `None` unless `SEARCH_ANSWER_ENABLED` is truthy *and* an
//!   API key (`SEARCH_LLM_API_KEY`, else `KITANI_API_KEY`) is set. The caller then passes no
//!   answerer and search behaves exactly as before.
//! * **Never blocks or fails a search.** `answer` returns a short `Err` on any transport/timeout/
//!   content failure; the engine ignores it.
//! * **Bounded.** The prompt has hard character caps; the HTTP request has a bounded timeout; the
//!   in-flight fan-out is bounded by a semaphore; there are no retries.

#![forbid(unsafe_code)]

use std::sync::Arc;

mod cache;
mod client;
mod config;
mod error;
mod prompt;
mod transport;

pub use cache::{cache_key_hash, entry_path, read_cache, write_cache_temp_only};
pub use client::{CacheStats, LlmAnswerer};
pub use config::{
    api_key, disabled_reason, fallback_api_key, fallback_base_url, fallback_model, is_enabled,
    llm_user_agent, model_chain, AnswerConfig, Disabled, DEFAULT_BASE_URL, DEFAULT_CONCURRENCY,
    DEFAULT_FALLBACK_BASE_URL, DEFAULT_FALLBACK_MODEL, DEFAULT_LLM_USER_AGENT, DEFAULT_MAX_TOKENS,
    DEFAULT_MODEL, DEFAULT_TIMEOUT_MS, REASONING_MIN_TOKENS, REASONING_TIMEOUT_MS,
};
pub use error::AnswerError;
pub use prompt::{
    build_prompt, parse_chat_response, MAX_CHARS_PER_PASSAGE, MAX_EVIDENCE_RECORDS, MAX_PASSAGES,
    MAX_PROMPT_CHARS, PROMPT_VERSION,
};
pub use transport::{ChainEntry, ChainTransport, FallbackTransport, Transport};

/// Build the answerer from the environment, or `None` when it is disabled.
///
/// The transport is a **chain**: the models in `SEARCH_LLM_MODELS` (free ones first) on the
/// primary provider, then the fallback provider last. A model that fails — rate limit, capacity,
/// timeout — moves to the next entry instead of failing the search, and every attempt is recorded
/// so the response can report which models were tried, which failed and which answered.
///
/// Returns `None` when `SEARCH_ANSWER_ENABLED` is unset/0, when no API key
/// (`SEARCH_LLM_API_KEY` / `OPENROUTER_API_KEY` / `KITANI_API_KEY`) is set, or if the HTTP client
/// cannot be built — never panics, never blocks, never reads a credential from a file. Use
/// [`disabled_reason`] for the single explanatory log line.
pub fn from_env() -> Option<LlmAnswerer> {
    let cfg = AnswerConfig::from_env().ok()?;
    let key = api_key()?;

    // Free models first, on the primary provider, then the fallback provider as the last resort.
    let primary_label = transport::provider_label(&cfg.base_url);
    let listed = config::model_chain();
    let models = if listed.is_empty() {
        vec![cfg.model.clone()]
    } else {
        listed
    };

    let mut entries: Vec<ChainEntry> = Vec::with_capacity(models.len() + 1);
    for model in models {
        let http = transport::HttpTransport::new(&cfg, key.clone()).ok()?;
        entries.push(ChainEntry::new(
            primary_label.clone(),
            model,
            Arc::new(http),
        ));
    }
    if let Some(fallback_key) = config::fallback_api_key() {
        let mut fb = cfg.clone();
        fb.base_url = config::fallback_base_url();
        let model = config::fallback_model();
        if let Ok(http) = transport::HttpTransport::new(&fb, fallback_key) {
            entries.push(ChainEntry::new(
                transport::provider_label(&fb.base_url),
                model,
                Arc::new(http),
            ));
        }
    }

    let chain = Arc::new(ChainTransport::new(entries));
    let transport: Arc<dyn Transport> = chain.clone();
    LlmAnswerer::with_transport_and_chain(cfg, transport, Some(chain)).ok()
}

/// Test/injection variant of [`from_env`]: identical env gating, with a caller-supplied
/// transport. Returns `None` under exactly the same conditions as [`from_env`].
pub fn from_env_with_transport(transport: Arc<dyn Transport>) -> Option<LlmAnswerer> {
    let cfg = AnswerConfig::from_env().ok()?;
    LlmAnswerer::with_transport(cfg, transport).ok()
}
