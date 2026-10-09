//! Errors surfaced by the answerer.
//!
//! Every variant carries a short, non-secret message. The API key never appears in any error
//! string, and the engine deliberately ignores an [`search_engine::Answerer`] error — a search
//! must never fail because answer generation failed.

use thiserror::Error;

/// Errors produced by the answerer (config, transport, cache and response parsing).
#[derive(Debug, Error)]
pub enum AnswerError {
    /// Cache / filesystem I/O failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A JSON payload failed to (de)serialize.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// The HTTP transport failed, or the provider returned a non-success status.
    #[error("transport error: {0}")]
    Http(String),
    /// The request exceeded the configured timeout.
    #[error("request timed out: {0}")]
    Timeout(String),
    /// The model returned no assistant content (and not because it ran out of tokens).
    #[error("model returned no content")]
    EmptyContent,
    /// The model hit the token ceiling before producing an answer (`finish_reason=length`).
    #[error(
        "model ran out of tokens before producing an answer (finish_reason={0}); \
         raise SEARCH_ANSWER_MAX_TOKENS"
    )]
    Truncated(String),
    /// The supplied configuration was internally inconsistent.
    #[error("invalid configuration: {0}")]
    Config(String),
    /// A required environment variable was absent or empty.
    #[error("missing environment variable {0}")]
    MissingEnv(String),
}
