//! Error taxonomy. One enum so every crate maps failures onto the same set of
//! classifications the crawler doc requires (DNS/conn/TLS/timeout/429/403/404/410/5xx/parse/...).

use thiserror::Error;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("database error: {0}")]
    Db(String),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("index error: {0}")]
    Index(String),

    #[error("network error: {0}")]
    Network(String),

    #[error("blocked by SSRF guard: {0}")]
    SsrfBlocked(String),

    #[error("robots disallows: {0}")]
    RobotsDisallowed(String),

    #[error("crawl policy denies: {0}")]
    CrawlPolicyDenied(String),

    #[error("resource limit exceeded: {0}")]
    LimitExceeded(String),

    #[error("lease fencing failed: {0}")]
    LeaseFenced(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid input: {0}")]
    Invalid(String),

    #[error("deadline exceeded")]
    Deadline,

    #[error("internal error: {0}")]
    Internal(String),
}

impl AppError {
    /// Whether a failure is retryable at the frontier level.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            AppError::Network(_) | AppError::Io(_) | AppError::Internal(_)
        )
    }
}

// Note: `From<rusqlite::Error>` and `From<tantivy::TantivyError>` live in the
// crates that depend on those libraries (metadata, indexer), not in core, so
// that core stays free of storage dependencies.
