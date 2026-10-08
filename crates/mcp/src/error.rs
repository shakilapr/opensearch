//! Error type shared by the MCP protocol layer and backends.
//!
//! [`McpError`] carries both JSON-RPC transport errors (which become a
//! `{"error": {...}}` response) and backend/tool failures (which surface to the
//! client as a `tools/call` result with `isError: true`). See
//! [`McpError::code`] for the JSON-RPC codes.

/// JSON-RPC error code: the payload could not be parsed as JSON.
pub const PARSE_ERROR: i64 = -32700;
/// JSON-RPC error code: the request envelope is malformed.
pub const INVALID_REQUEST: i64 = -32600;
/// JSON-RPC error code: the method does not exist.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC error code: the method's params are missing or wrong.
pub const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC error code: internal error (never emitted for tool failures).
pub const INTERNAL_ERROR: i64 = -32603;

/// Every failure the MCP layer can produce.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpError {
    /// Malformed JSON on the wire (`-32700`).
    #[error("parse error: {0}")]
    Parse(String),
    /// A request whose method is unknown (`-32601`).
    #[error("method not found: {0}")]
    MethodNotFound(String),
    /// A request whose params fail the method's schema (`-32602`).
    #[error("invalid params: {0}")]
    InvalidParams(String),
    /// A backend/tool failure. Returned to the client as an `isError` block,
    /// never as a dropped connection or a panic.
    #[error("{0}")]
    Backend(String),
}

impl McpError {
    /// Build a parse error (`-32700`).
    pub fn parse(msg: impl Into<String>) -> Self {
        Self::Parse(msg.into())
    }

    /// Build a method-not-found error (`-32601`).
    pub fn method_not_found(msg: impl Into<String>) -> Self {
        Self::MethodNotFound(msg.into())
    }

    /// Build an invalid-params error (`-32602`).
    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self::InvalidParams(msg.into())
    }

    /// Build a backend/tool error (surfaced with `isError: true`).
    pub fn backend(msg: impl Into<String>) -> Self {
        Self::Backend(msg.into())
    }

    /// The JSON-RPC error code this variant maps to.
    pub fn code(&self) -> i64 {
        match self {
            Self::Parse(_) => PARSE_ERROR,
            Self::MethodNotFound(_) => METHOD_NOT_FOUND,
            Self::InvalidParams(_) => INVALID_PARAMS,
            Self::Backend(_) => INTERNAL_ERROR,
        }
    }

    /// The human-readable message (without the `kind:` prefix).
    pub fn message(&self) -> String {
        match self {
            Self::Parse(m)
            | Self::MethodNotFound(m)
            | Self::InvalidParams(m)
            | Self::Backend(m) => m.clone(),
        }
    }
}
