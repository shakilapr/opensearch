# Stage 7 Spec — Service surfaces

Frozen interfaces. New crate `crates/mcp` (package `search-mcp`, a binary) + `deploy/` files.
The HTTP API (`crates/api`) is extended by the Stage-4 integration work, not here.

## MCP server (`crates/mcp`)

A **JSON-RPC 2.0 over stdio** MCP server (no external MCP SDK — implement the minimal
protocol). Requests are newline-delimited JSON on stdin, responses on stdout. It must
implement:

- `initialize` → `{ protocolVersion, capabilities: { tools: {} }, serverInfo }`
- `tools/list` → the tools below with JSON-Schema `inputSchema`
- `tools/call` → dispatch by name; a tool error returns `isError: true` with a text content
  block, never a crash

Tools (each maps to a backend trait so the server is testable without a live engine):

| tool          | input                     | output                                   |
|---------------|---------------------------|------------------------------------------|
| `search`      | `{ query, limit? }`       | results with url/title/snippet/provenance|
| `deep_search` | `{ query, limit? }`       | navigation trace + results               |
| `fetch_doc`   | `{ doc_id }`              | normalized document text                 |

```rust
pub trait Backend: Send + Sync {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>, McpError>;
    fn deep_search(&self, query: &str, limit: usize) -> Result<DeepResult, McpError>;
    fn fetch_doc(&self, doc_id: &str) -> Result<DocText, McpError>;
}
pub struct Hit { pub doc_id: String, pub url: String, pub title: String, pub snippet: String, pub provenance: String }
pub struct DocText { pub doc_id: String, pub text: String }
pub struct DeepResult { pub hits: Vec<Hit>, pub trace: Vec<String> }
pub fn serve(backend: Box<dyn Backend>) -> Result<(), McpError>;   // stdio loop
pub fn handle_line(backend: &dyn Backend, line: &str) -> Option<String>;  // one request -> one response
```

`handle_line` returning `None` means "notification, no reply" (e.g. `notifications/initialized`).
Malformed JSON → a JSON-RPC error object with code `-32700` (parse) / `-32601` (method not
found) / `-32602` (invalid params).

## deploy/

- `deploy/search-agent.service` — systemd unit for `agent0 serve` (User, WorkingDirectory,
  `Restart=on-failure`, `Environment=SEARCH_DATA_DIR=/var/lib/search`).
- `deploy/search-api.service` — systemd unit for the HTTP API.
- `deploy/nginx.conf` — reverse proxy to `127.0.0.1:8080` with sane timeouts + gzip.
- `deploy/README.md` — install/run steps.
- `docs/specs/MCP.md` — how to register the MCP server with a client (command + args), tool list,
  and example JSON-RPC exchange.
- `ui/README.md` — add the `VITE_API_BASE` / build-and-serve note if not already covered.

## Exit gate

`initialize`, `tools/list`, and each `tools/call` round-trip through `handle_line` against a
fake backend (hermetic, no network); parse errors and unknown methods return the right
JSON-RPC error codes. Deploy files exist and reference the real binary names.
