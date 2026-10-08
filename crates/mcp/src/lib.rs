//! `search-mcp` — a minimal MCP server (JSON-RPC 2.0 over stdio) exposing the
//! local search engine as `search`, `deep_search`, and `fetch_doc` tools.
//!
//! Interfaces are frozen in `docs/STAGE7_SPEC.md`. The crate is deliberately
//! free of any external MCP SDK: the protocol subset is implemented by hand on
//! top of `serde_json`, and the server is generic over [`Backend`] so it is
//! fully testable without a live engine.
//!
//! ```text
//! initialize    -> protocol version + `tools` capability
//! tools/list    -> search / deep_search / fetch_doc with inputSchema
//! tools/call    -> dispatch by name (tool errors are isError blocks)
//! ```
//!
//! The `search-mcp` binary wires [`Backend`] to the local HTTP search-api
//! ([`http::HttpBackend`]); tests wire it to a fake backend.

#![forbid(unsafe_code)]

pub mod backend;
pub mod error;
pub mod http;
pub mod protocol;

pub use backend::{Backend, DeepResult, DocText, Hit};
pub use error::McpError;
pub use http::HttpBackend;
pub use protocol::{handle_line, serve, PROTOCOL_VERSION, SERVER_NAME, SERVER_VERSION};
