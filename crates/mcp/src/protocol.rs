//! The JSON-RPC 2.0 / MCP protocol layer.
//!
//! Requests are newline-delimited JSON on stdin, responses on stdout. No
//! external MCP SDK: the minimal subset ( `initialize`, `tools/list`,
//! `tools/call` ) is implemented here by hand with `serde_json`.
//!
//! Invariants:
//!   * A request (has an `id`) always yields exactly one response.
//!   * A notification (no `id`) yields `None` — no reply.
//!   * Malformed JSON yields a `-32700` error object (never a panic).
//!   * A backend/tool failure yields a normal `tools/call` result with
//!     `isError: true` — never a dropped connection.

use serde_json::{json, Map, Value};

use crate::backend::{Backend, Hit};
use crate::error::{McpError, INVALID_PARAMS};

/// MCP protocol version this server speaks.
pub const PROTOCOL_VERSION: &str = "2024-11-05";
/// `serverInfo.name`.
pub const SERVER_NAME: &str = "search-mcp";
/// `serverInfo.version` (the crate version).
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 100;

/// Handle one newline-delimited line of stdin.
///
/// Returns the response line to write to stdout, or `None` when the line is a
/// notification (no `id`) and therefore must not be answered.
pub fn handle_line(backend: &dyn Backend, line: &str) -> Option<String> {
    // --- parse ----------------------------------------------------------
    let parsed: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return Some(error_response(
                Value::Null,
                McpError::parse(e.to_string()).code(),
                &format!("parse error: {e}"),
            ))
        }
    };
    let obj = match parsed.as_object() {
        Some(o) => o,
        None => {
            return Some(error_response(
                Value::Null,
                crate::error::INVALID_REQUEST,
                "invalid request: payload must be a JSON object",
            ))
        }
    };

    // --- id / notification ----------------------------------------------
    // Absence of the `id` key marks a notification; those are never answered.
    let is_notification = !obj.contains_key("id");
    let id = obj.get("id").cloned().unwrap_or(Value::Null);

    // --- method ----------------------------------------------------------
    let method = match obj.get("method").and_then(Value::as_str) {
        Some(m) => m.to_string(),
        None => {
            if is_notification {
                return None;
            }
            return Some(error_response(
                id,
                crate::error::METHOD_NOT_FOUND,
                "method not found: missing string 'method'",
            ));
        }
    };

    // --- params shape ----------------------------------------------------
    let params = obj.get("params");
    if let Some(p) = params {
        if !p.is_object() {
            if is_notification {
                return None;
            }
            return Some(error_response(
                id,
                INVALID_PARAMS,
                "invalid params: 'params' must be an object",
            ));
        }
    }

    // Notifications are executed (no reply) and otherwise dropped.
    if is_notification {
        return None;
    }

    // --- dispatch --------------------------------------------------------
    match method.as_str() {
        "initialize" => Some(result_response(id, initialize())),
        "tools/list" => Some(result_response(id, json!({ "tools": tool_definitions() }))),
        "tools/call" => Some(tools_call(backend, id, params)),
        other => Some(error_response(
            id,
            crate::error::METHOD_NOT_FOUND,
            &format!("method not found: {other}"),
        )),
    }
}

/// `initialize` result: the negotiated protocol version, the `tools`
/// capability, and the server identity.
fn initialize() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION }
    })
}

/// The three frozen tools, each with a JSON-Schema `inputSchema`.
fn tool_definitions() -> Value {
    json!([
        {
            "name": "search",
            "description": "Search the local index and return ranked results with provenance.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_LIMIT,
                        "default": DEFAULT_LIMIT,
                        "description": "Maximum number of results to return."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        },
        {
            "name": "deep_search",
            "description": "Run a deeper search that also returns the navigation trace the engine walked.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_LIMIT,
                        "default": DEFAULT_LIMIT,
                        "description": "Maximum number of results to return."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        },
        {
            "name": "fetch_doc",
            "description": "Fetch the normalized text of a document previously seen by the crawler.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "doc_id": {
                        "type": "string",
                        "description": "The stable document id returned by a search."
                    }
                },
                "required": ["doc_id"],
                "additionalProperties": false
            }
        }
    ])
}

/// `tools/call` dispatch. Bad params yield a `-32602` error object; a backend
/// failure yields an `isError` content block.
fn tools_call(backend: &dyn Backend, id: Value, params: Option<&Value>) -> String {
    let obj = match params.and_then(Value::as_object) {
        Some(o) => o,
        None => {
            return error_response(
                id,
                INVALID_PARAMS,
                "invalid params: tools/call requires an object 'params'",
            )
        }
    };

    let name = match obj.get("name").and_then(Value::as_str) {
        Some(n) => n,
        None => {
            return error_response(
                id,
                INVALID_PARAMS,
                "invalid params: missing string 'name'",
            )
        }
    };

    let empty = Map::new();
    let args = match obj.get("arguments") {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(m)) => m,
        Some(_) => {
            return error_response(
                id,
                INVALID_PARAMS,
                "invalid params: 'arguments' must be an object",
            )
        }
    };

    match name {
        "search" => {
            let query = match query_arg(args) {
                Ok(q) => q,
                Err(m) => return invalid_params(id, &m),
            };
            let limit = match limit_arg(args) {
                Ok(l) => l,
                Err(m) => return invalid_params(id, &m),
            };
            match backend.search(&query, limit) {
                Ok(hits) => tool_ok(id, hits_json(&hits)),
                Err(e) => tool_error(id, e),
            }
        }
        "deep_search" => {
            let query = match query_arg(args) {
                Ok(q) => q,
                Err(m) => return invalid_params(id, &m),
            };
            let limit = match limit_arg(args) {
                Ok(l) => l,
                Err(m) => return invalid_params(id, &m),
            };
            match backend.deep_search(&query, limit) {
                Ok(d) => tool_ok(
                    id,
                    json!({
                        "hits": d.hits.iter().map(Hit::to_json).collect::<Vec<_>>(),
                        "trace": d.trace,
                    }),
                ),
                Err(e) => tool_error(id, e),
            }
        }
        "fetch_doc" => {
            let doc_id = match args.get("doc_id").and_then(Value::as_str) {
                Some(d) => d,
                None => return invalid_params(id, "missing required string argument 'doc_id'"),
            };
            match backend.fetch_doc(doc_id) {
                Ok(doc) => tool_ok(id, serde_json::to_value(&doc).unwrap_or(Value::Null)),
                Err(e) => tool_error(id, e),
            }
        }
        other => invalid_params(id, &format!("unknown tool '{other}'")),
    }
}

fn query_arg(args: &Map<String, Value>) -> Result<String, String> {
    match args.get("query") {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.clone()),
        Some(Value::String(_)) => Err("'query' must not be empty".to_string()),
        Some(_) => Err("'query' must be a string".to_string()),
        None => Err("missing required string argument 'query'".to_string()),
    }
}

fn limit_arg(args: &Map<String, Value>) -> Result<usize, String> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok(DEFAULT_LIMIT),
        Some(v) => match v.as_u64() {
            Some(n) => Ok((n as usize).clamp(1, MAX_LIMIT)),
            None => Err("'limit' must be a positive integer".to_string()),
        },
    }
}

fn hits_json(hits: &[Hit]) -> Value {
    Value::Array(hits.iter().map(Hit::to_json).collect())
}

// --- response builders --------------------------------------------------

fn result_response(id: Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn error_response(id: Value, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
    .to_string()
}

fn invalid_params(id: Value, detail: &str) -> String {
    error_response(id, INVALID_PARAMS, &format!("invalid params: {detail}"))
}

/// A successful `tools/call`: a single text content block, `isError: false`.
fn tool_ok(id: Value, data: Value) -> String {
    let text = serde_json::to_string_pretty(&data).unwrap_or_else(|_| "null".to_string());
    result_response(
        id,
        json!({
            "content": [ { "type": "text", "text": text } ],
            "isError": false
        }),
    )
}

/// A failed `tools/call`: a single text content block, `isError: true`. This is
/// a *result*, not a JSON-RPC error, so the connection stays usable.
fn tool_error(id: Value, e: McpError) -> String {
    result_response(
        id,
        json!({
            "content": [ { "type": "text", "text": e.to_string() } ],
            "isError": true
        }),
    )
}

/// Read newline-delimited requests from stdin and write responses to stdout
/// until EOF. Closes cleanly; a backend error never terminates the loop.
pub fn serve(backend: Box<dyn Backend>) -> Result<(), McpError> {
    use std::io::{BufRead, Write};

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = line.map_err(|e| McpError::backend(format!("stdin read error: {e}")))?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_line(backend.as_ref(), &line) {
            writeln!(out, "{response}").map_err(|e| McpError::backend(format!("stdout write error: {e}")))?;
            out.flush()
                .map_err(|e| McpError::backend(format!("stdout flush error: {e}")))?;
        }
    }
    Ok(())
}
