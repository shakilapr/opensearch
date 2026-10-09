//! Errors surfaced by the organizer.
//!
//! Every variant is explicit: nothing here fabricates data, and every failure path either
//! escalates or returns an error. The API key is never embedded in an error message.

use thiserror::Error;

/// Errors produced by the organizer (config, transport, cache and parsing).
#[derive(Debug, Error)]
pub enum OrganizeError {
    /// Cache / filesystem I/O failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A cache entry failed to (de)serialize.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// The model output could not be parsed/validated into an organized record.
    #[error("parse error: {0}")]
    Parse(String),
    /// The HTTP transport failed, or the provider returned a non-success status.
    #[error("http error: {0}")]
    Http(String),
    /// A required environment variable was absent or empty.
    #[error("missing environment variable {0}")]
    MissingEnv(String),
    /// The supplied configuration was internally inconsistent.
    #[error("invalid configuration: {0}")]
    Config(String),
    /// The page could not be turned into a non-empty record.
    #[error("invalid page: {0}")]
    InvalidPage(String),
    /// A spawned worker task panicked or was cancelled.
    #[error("task failed: {0}")]
    Join(String),
}
