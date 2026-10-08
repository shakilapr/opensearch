# deploy/

Service surfaces for the search engine: two systemd units for the Rust agent and
the nginx front end for the UI + HTTP API.

## Layout

| File                     | What it runs                                             |
|--------------------------|----------------------------------------------------------|
| `search-agent.service`   | `agent0 serve` — the crawl/index pipeline + HTTP API     |
| `search-api.service`     | `agent0 serve` — the HTTP API tier (`/search`, `/health`)|
| `nginx.conf`             | static UI (`/var/www/search`) + reverse proxy to `127.0.0.1:8080` |

The MCP server (`search-mcp`) is a **stdio** program launched by an MCP client,
not a daemon, so it has no unit here — see [`../docs/MCP.md`](../docs/MCP.md).

> **Modular-monolith note.** Today `agent0 serve` is *both* the agent and the
> HTTP API (there is no separate daemon binary). `search-agent.service` owns
> `/var/lib/search` and serves the API; `search-api.service` is the same binary
> for an API-only node that reads the shared index. **Enable exactly one per
> host** — enabling both on one host double-binds `127.0.0.1:8080`.

## Build & install

```bash
# 1. Build the workspace (release).
cargo build --release

# 2. Install the binary and create the service user + data dir.
sudo useradd --system --home /var/lib/search --shell /usr/sbin/nologin search
sudo install -d -o search -g search /opt/search/bin /var/lib/search
sudo install -m 755 target/release/agent0 /opt/search/bin/agent0

# 3. Install the unit and start it.
sudo install -m 644 deploy/search-agent.service /etc/systemd/system/search-agent.service
sudo systemctl daemon-reload
sudo systemctl enable --now search-agent.service

# 4. Verify.
curl -fsS http://127.0.0.1:8080/health
```

The data root must stay outside any synced path: the WAL SQLite database and
Tantivy's `MmapDirectory` corrupt if copied mid-write. Keep it at
`/var/lib/search` (the unit already sets `SEARCH_DATA_DIR`).

## UI

```bash
cd ui
# Production builds are same-origin: the API is reached through this nginx.
VITE_API_BASE= npm run build
sudo mkdir -p /var/www/search
npm run deploy            # build + copy dist/* to /var/www/search
```

## nginx

```bash
sudo install -m 644 deploy/nginx.conf /etc/nginx/sites-available/search
sudo ln -sf /etc/nginx/sites-available/search /etc/nginx/sites-enabled/search
sudo nginx -t && sudo systemctl reload nginx
```

## Environment variables

| Variable           | Unit default              | Meaning                                  |
|--------------------|---------------------------|------------------------------------------|
| `SEARCH_DATA_DIR`  | `/var/lib/search`         | runtime state root (SQLite/Tantivy/blobs)|
| `SEARCH_BIND`      | `127.0.0.1:8080`          | HTTP API bind address                    |
| `RUST_LOG`         | `info`                    | tracing filter                           |
| `SEARCH_API_BASE`  | `http://127.0.0.1:8080`   | base URL the `search-mcp` loopback uses  |
