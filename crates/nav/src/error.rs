//! Navigation errors.

use thiserror::Error;

/// Failures surfaced by a [`crate::SearchExecutor`] or a [`crate::ModelClient`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum NavError {
    /// The executor could not carry out the action (index/network/parse failure).
    #[error("action execution failed: {0}")]
    Execution(String),
    /// The injected model client failed (transport, decoding, timeout).
    #[error("model client failed: {0}")]
    Model(String),
    /// The action was malformed or referenced a missing target.
    #[error("invalid action: {0}")]
    InvalidAction(String),
    /// A resource budget was exceeded while dispatching.
    #[error("budget exhausted")]
    Budget,
}

impl NavError {
    /// Convenience builder for execution failures.
    pub fn execution(msg: impl Into<String>) -> Self {
        NavError::Execution(msg.into())
    }
    /// Convenience builder for model failures.
    pub fn model(msg: impl Into<String>) -> Self {
        NavError::Model(msg.into())
    }
}
