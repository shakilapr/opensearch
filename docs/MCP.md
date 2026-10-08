# MCP server (`search-mcp`)

`search-mcp` exposes the local search engine to MCP clients (Hermes, Claude
Desktop, etc.) as three tools, over **JSON-RPC 2.0 on stdio** — newline-delimited
JSON requests on stdin, responses on stdout. No external MCP SDK: the protocol
subset is implemented by hand.

## Registering with a client

Point the client at the built binary. Most clients take a `command` + `args`
(or `command` + `env`) block:

```json
{
  "mcpServers": {
    "search": {
      "command": "/opt/search/bin/search-mcp",
      "args": [],
      "env": { "SEARCH_API_BASE": "http://127.0.0.1:8080" }
    }
  }
}
```

For a source checkout, point straight at the debug build:

```json
{
  "mcpServers": {
    "search": {
      "command": "cargo",
      "args": ["run", "-q", "-p", "search-mcp", "--bin", "search-mcp"]
    }
  }
}
```

The binary is a thin loopback client of the HTTP search-api. Build it with
`cargo build --release` (`target/release/search-mcp`) and make sure
`search-agent.service` is running so `/search` is reachable.

Environment:

| Variable           | Default                   | Meaning                       |
|--------------------|---------------------------|-------------------------------|
| `SEARCH_API_BASE`  | `http://127.0.0.1:8080`   | search-api base URL           |

## Tools

| Tool          | Input                | Output                                            |
|---------------|----------------------|---------------------------------------------------|
| `search`      | `{ query, limit? }`  | ranked results with `url`/`title`/`snippet`/`provenance` |
| `deep_search` | `{ query, limit? }`  | a navigation `trace` plus results                 |
| `fetch_doc`   | `{ doc_id }`         | the normalized document `text`                    |

Each tool declares a JSON-Schema `inputSchema` via `tools/list`.

## Protocol

Methods implemented: `initialize`, `tools/list`, `tools/call`. Protocol version
`2024-11-05`. Notifications (messages without an `id`, e.g.
`notifications/initialized`) get no reply.

Errors:

| Situation                                   | Behaviour                                   |
|---------------------------------------------|---------------------------------------------|
| malformed JSON                               | JSON-RPC error `-32700`                     |
| unknown method                               | JSON-RPC error `-32601`                     |
| bad params / unknown tool / missing argument | JSON-RPC error `-32602`                     |
| backend or tool failure                      | a `tools/call` **result** with `isError: true` and a text block — never a crash or dropped connection |

## Example exchange

Request — `initialize`:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","clientInfo":{"name":"demo","version":"0"}}}
```

Response:

```json
{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"search-mcp","version":"0.1.0"}}}
```

Request — `tools/list` (truncated):

```json
{"jsonrpc":"2.0","id":2,"method":"tools/list"}
```

Response:

```json
{"jsonrpc":"2.0","id":2,"result":{"tools":[
  {"name":"search","description":"Search the local index and return ranked results with provenance.","inputSchema":{"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":100,"default":10}},"required":["query"],"additionalProperties":false}},
  {"name":"deep_search","description":"Run a deeper search that also returns the navigation trace the engine walked.","inputSchema":{"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":100,"default":10}},"required":["query"],"additionalProperties":false}},
  {"name":"fetch_doc","description":"Fetch the normalized text of a document previously seen by the crawler.","inputSchema":{"type":"object","properties":{"doc_id":{"type":"string"}},"required":["doc_id"],"additionalProperties":false}}
]}}
```

Request — `tools/call` for `search`:

```json
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search","arguments":{"query":"rust async runtime","limit":2}}}
```

Response (the `text` field is a pretty-printed JSON array of hits):

```json
{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"[\n  {\n    \"doc_id\": \"...\",\n    \"url\": \"https://...\",\n    \"title\": \"...\",\n    \"snippet\": \"...\",\n    \"provenance\": \"snapshot\"\n  }\n]"}],"isError":false}}
```

A failing tool returns the same envelope with an error message and
`"isError": true`:

```json
{"jsonrpc":"2.0","id":4,"result":{"content":[{"type":"text","text":"search-api unreachable at http://127.0.0.1:8080/search: connection refused"}],"isError":true}}
```

A malformed line returns a JSON-RPC error (id `null`):

```json
{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error: expected value at line 1 column 1"}}
```

## Testing

The protocol layer is fully testable without a live engine:

```bash
cargo test -p search-mcp
```

The suite drives `handle_line` against an in-process fake backend (no network,
no real index) and covers `initialize`, `tools/list`, each `tools/call`
round-trip, the `-32700`/`-32601`/`-32602` error codes, the `isError` tool-error
path, and notifications returning no reply.
