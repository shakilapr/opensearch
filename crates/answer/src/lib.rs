//! `search-answer` — optional, opt-in query-time LLM answer generation.
//!
//! Implements [`search_engine::Answerer`] on top of an OpenAI-chat-compatible provider, reusing
//! the patterns proven in `search-organize`: env-driven config, an on-disk response cache keyed
//! by request (atomic writes), a bounded-concurrency fan-out, and an injectable [`Transport`] so
//! tests are hermetic (no network, no credential, no sleeps).
//!
//! Design guarantees:
//!
//! * **Opt-in.** [`from_env`] returns `None` unless `SEARCH_ANSWER_ENABLED` is truthy *and*
//!   `KITANI_API_KEY` is set. The caller then passes no answerer and search behaves exactly as
//!   before.
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
    api_key, disabled_reason, is_enabled, AnswerConfig, Disabled, DEFAULT_BASE_URL,
    DEFAULT_CONCURRENCY, DEFAULT_MAX_TOKENS, DEFAULT_MODEL, DEFAULT_TIMEOUT_MS,
    REASONING_MIN_TOKENS, REASONING_TIMEOUT_MS,
};
pub use error::AnswerError;
pub use prompt::{
    build_prompt, parse_chat_response, MAX_CHARS_PER_PASSAGE, MAX_EVIDENCE_RECORDS, MAX_PASSAGES,
    MAX_PROMPT_CHARS, PROMPT_VERSION,
};
pub use transport::Transport;

/// Build the answerer from the environment, or `None` when it is disabled.
///
/// Returns `None` when `SEARCH_ANSWER_ENABLED` is unset/0, when `KITANI_API_KEY` is absent, or if
/// the HTTP client cannot be built — never panics, never blocks, never reads a credential from a
/// file. Use [`disabled_reason`] for the single explanatory log line.
pub fn from_env() -> Option<LlmAnswerer> {
    let cfg = AnswerConfig::from_env().ok()?;
    let key = api_key()?;
    let transport = transport::HttpTransport::new(&cfg, key).ok()?;
    LlmAnswerer::with_transport(cfg, Arc::new(transport)).ok()
}

/// Test/injection variant of [`from_env`]: identical env gating, with a caller-supplied
/// transport. Returns `None` under exactly the same conditions as [`from_env`].
pub fn from_env_with_transport(transport: Arc<dyn Transport>) -> Option<LlmAnswerer> {
    let cfg = AnswerConfig::from_env().ok()?;
    LlmAnswerer::with_transport(cfg, transport).ok()
}
