# Operations runbook

How to run, back up, and troubleshoot the search engine on the single box. Service files
live in `deploy/` ([`deploy/README.md`](../deploy/README.md)); this is the day-2 view.

Component-to-binary reality check: there is **one daemon binary**, `agent0`, run as
`agent0 serve` — it owns the crawl/index pipeline *and* the HTTP API. `search-mcp` is a
stdio client launched by an MCP client, not a daemon. `bench` is a batch job. There is no
separate `search-api` daemon yet (modular-monolith note in `deploy/README.md`).

## Start / stop

Two systemd units exist; **enable exactly one per host** because both bind
`SEARCH_BIND=127.0.0.1:8080`:

| unit | runs | role |
| :--- | :--- | :--- |
| `search-agent.service` | `agent0 serve` | owns `/var/lib/search`; crawl/index + API |
| `search-api.service` | `agent0 serve` | API-only node that reads a shared index |

```sh
sudo systemctl start  search-agent.service
sudo systemctl status search-agent.service
sudo systemctl stop   search-agent.service

# readiness check (does not depend on logs):
curl -fsS http://127.0.0.1:8080/health
```

`/health` returns `{"status":"ok","generation":N,"index_manifest_id":"...","uptime_seconds":S,"version":"0.1.0"}`.
`generation == 0` means nothing is published yet (see *Unpublished index* below).

> **The bind port must be free.** `agent0 serve` exits with
> `configuration error: failed to bind 127.0.0.1:8080: Address already in use` if anything else
> holds the port (on a dev box that is often `code-server`, which also defaults to 8080). Check
> with `ss -ltn | grep 8080` and override with `SEARCH_BIND=127.0.0.1:<free-port>` if needed —
> a failed bind is a startup error, not a silent fallback, and `/health` will then answer from
> whatever else owns the port (which is why a stray `401 Unauthorized` on `/health` means you are
> talking to a different service entirely).

Install/upgrade steps are in `deploy/README.md`. `agent0` reads its config from the
environment (`crates/core::Config::from_env`):

| variable | default | meaning |
| :--- | :--- | :--- |
| `SEARCH_DATA_DIR` | `/var/lib/search` | runtime state root |
| `SEARCH_BIND` | `127.0.0.1:8080` | HTTP API bind |
| `SEARCH_USER_AGENT` | `AgentNativeSearchBot/0.1 (+https://github.com/agent-native-search)` | crawl UA |
| `RUST_LOG` | `info` | tracing filter (see *Logs*) |
| `SEARCH_MODEL_CACHE` | `<workspace>/models/fastembed` | where the real embedding weights live (see *Real embedding model*) |
| `SEARCH_READ_ONLY` | `auto` | `auto` takes the index writer and falls back to a **read-only** open when it is locked (a crawl is running); `1` always read-only, `0` always the writer |
| `SEARCH_CORS_ORIGIN` | unset | comma-separated origins allowed to call the API from a browser (see *Browser clients and CORS*) |
| `SEARCH_PLANNER_ENABLED` | unset | opt-in §6/§8 LLM query planning (`search-planner`); needs `KITANI_API_KEY`. See `PLANNER.md` |

### LLM gateway credentials

Every component that calls the OpenAI-chat-compatible provider (`search-organize`, `search-answer`)
accepts a vendor-neutral override. These take precedence over the vendor-specific variables, which
keep working unchanged as fallbacks — so a gateway other than the default one needs only this set:

| variable | overrides | meaning |
| :--- | :--- | :--- |
| `SEARCH_LLM_BASE_URL` | `KITANI_BASE_URL` (organize), `SEARCH_ANSWER_BASE_URL` (answer) | OpenAI-chat-compatible endpoint, no trailing `/chat/completions`. |
| `SEARCH_LLM_API_KEY` | `KITANI_API_KEY` | Credential. |
| `SEARCH_LLM_MODEL` | `KITANI_MODEL` (organize), `SEARCH_ANSWER_MODEL` (answer) | Primary model id. |

Precedence is resolved **per variable**, so setting only `SEARCH_LLM_API_KEY` keeps the
vendor-specific base URL/model. The escalation model (`KITANI_ESCALATE_MODEL`, `search-organize`)
has no neutral override and is unchanged.

The credential is read **from the environment only** — never from a file, never logged, never put
in an error message. When neither key variable is set: `search-organize` (`LlmConfig::from_env`)
fails with `missing environment variable KITANI_API_KEY`, and `search-answer` reports
`Disabled::MissingKey` (answer generation is off).

## Browser clients and CORS

The API binds loopback and carries no authentication, so **it sends no CORS headers by
default**: a page served from another origin cannot call it, and a preflight is refused. This is
deliberate — an allow-any-origin localhost API lets any site the user visits drive their index.

To develop the UI against a running API, name the origin explicitly:

```sh
SEARCH_BIND=127.0.0.1:8090 SEARCH_DATA_DIR=/var/lib/search \
SEARCH_ANSWER_ENABLED=1 SEARCH_CORS_ORIGIN=http://127.0.0.1:5173 \
SEARCH_PLANNER_ENABLED=1 \
  agent0 serve
(cd ui && VITE_API_BASE=http://127.0.0.1:8090 npm run dev)   # Vite on :5173
```

Only the listed origins receive `access-control-allow-origin`; any other origin gets none, which
is what makes the browser refuse the request. Production needs no CORS at all: serve the built UI
from the same host (`deploy/nginx.conf`), which the `deploy` npm script prepares.

## Browser escalation (rendering JS-only pages)

Some real pages carry no text until their JavaScript runs: the HTML normalizes to nothing, so
without help they would be indexed as empty or dropped. With the `chromium` feature the crawler
escalates such a page **once** to a headless Chromium, indexes the rendered text, and falls back
to the previous `html-no-text` route if the render is no better. Escalation is opt-in, because it
costs far more than a plain fetch:

```sh
cargo build -p search-agent0 --features chromium
SEARCH_BROWSER=1 SEARCH_CHROMIUM=/path/to/chrome agent0 crawl --seeds crawler/seeds/llm-tech.txt
```

| Variable | Meaning |
| --- | --- |
| `SEARCH_BROWSER` | `1`/`true`/`yes` attaches the Chromium driver (needs `--features chromium`). |
| `SEARCH_CHROMIUM` | Explicit Chromium/Chrome path; otherwise discovery is used. |
| `SEARCH_CHROMIUM_NO_SANDBOX` | `1` runs Chromium with `--no-sandbox` (plus `--disable-dev-shm-usage`). |

`SEARCH_CHROMIUM_NO_SANDBOX` is a **security trade-off, so it is explicit**: with the sandbox off,
a renderer that exploits a bug is not contained. Chromium refuses to start as root without it
(`Running as root without --no-sandbox is not supported`), which is how a container usually runs —
so this box needs it, and a host that can run Chromium as an unprivileged user should not set it.
One persistent browser is launched per crawl and each fetch gets a short-lived private context
(cookies and storage do not carry between pages). Every fetch is bounded by `BrowserLimits`
(`max_ms`). The driver is verified against a real browser by an ignored test:

```sh
SEARCH_CHROMIUM=/path/to/chrome cargo test -p search-browser --features chromium \
  --test chromium_live -- --ignored
```

## The entity graph and its query-time lane (§9.4)

`agent0 graph build` populates the entity graph and writes a sidecar the serving process reads at
startup:

```sh
SEARCH_DATA_DIR=/var/lib/search agent0 graph build [--reset] \
  [--min-count 2] [--window 50] [--max-mentions 40] [--min-docs 3]
# → <data_dir>/graph/entities.bin  (entities, aliases, entity→document join, co-occurrence edges)
```

The sidecar exists because the index schema is **frozen and hash-asserted**: the join cannot become
a table without invalidating every index, so it travels beside the index exactly as the dense
lane's `vectors.bin` does. `agent0 serve` picks it up automatically — the graph lane runs beside
the lexical and dense lanes and its hits are reported as the `graph_expansion` provenance signal.
No sidecar means no graph lane (logged at info); an unreadable sidecar is skipped with a warning
rather than preventing the server from starting.

Two behaviours are deliberate and worth knowing before reading a trace:

* **A source-scoped search returns no graph hits.** The lane cannot scope to a source and will not
  answer with unscoped documents; the source-first lane serves those queries.
* **A query naming no known entity contributes nothing.** The lane is fused with lexical
  retrieval, not a replacement for it.

The engine trace ends with how many documents expansion reached for that query
(`provenance: graph expansion reached N document(s)`), which separates "the graph had nothing" from
"the lane is not wired". A result reports `graph_expansion: 1.0` when its lane names the graph lane
**or** when the graph lane reached that document — fusion keeps one lane per document, so without
the second test a page that both matched lexically and was reached by activation would report no
expansion even though it was expanded, and the signal would read zero for every query.

Seeding is intentionally conservative: a match must look like a *name* — a multi-word phrase, or
carrying a digit, or acronym/mixed case (`vLLM`, `BERT`), or capitalised by the user in the query —
unless the query is three tokens or fewer, which is treated as a term lookup. The graph contains
single capitalised common nouns (`Phrase`, `Database`, `Transformer`) and seeding on those made the
lane fire on unrelated queries. Rebuild the sidecar after a corpus change (it is a snapshot, as
`graph_version` implies):

```sh
SEARCH_DATA_DIR=/var/lib/search SEARCH_READ_ONLY=1 agent0 graph build   # safe beside a crawl
```

## Read-only serving (crawl and serve at once)

Tantivy's writer lock is exclusive per index directory, so a process that opens a writer cannot
share the data root with another one. `agent0 serve` therefore defaults to `SEARCH_READ_ONLY=auto`:
it tries the writer (full function, query-time refresh on) and, when a crawl holds it, comes up
**read-only** with a warning — queries work, query-time refresh does not. The same property is
what lets the embedding backfill run beside a crawl:

```sh
agent0 embed build --out /var/lib/search/vectors.bin     # read-only; safe during a crawl
```

## Real embedding model (BGE-small)

The dense lane runs a real semantic model (BGE-small-en-v1.5) by default; the deterministic
lexical `HashEmbedder` is the fallback. A hermetic build with no ONNX Runtime and no model
download is available with `--no-default-features`:

```sh
cargo build --release --no-default-features -p search-agent0   # hash embedder only
```

With the default build, `agent0 serve` loads the vector sidecar when one exists
(`<data_dir>/vectors.bin`, built by `agent0 embed build`) and logs `dense lane enabled docs=N`.
Without a sidecar the lane is simply absent — and every result's `lane_ranks` shows which lanes
produced it (`bm25`, `dense`, `source_first`).

```sh
cargo build --release --features bge -p search-bench
cargo test  -p search-embed --features bge      # 15 tests; first run fetches the model
```

* Model: `BAAI/bge-small-en-v1.5`, 384-d, CLS-pooled, L2-normalized, via `fastembed`
  on CPU.
* Weights are fetched on **first use** into `<workspace root>/models/fastembed`
  (~128 MB, gitignored — the same `models/` tree used for other server-only artifacts).
  Override the location with `SEARCH_MODEL_CACHE`.
* Two failure modes worth knowing: several processes constructing the model at once
  race on the Hugging Face download (`Failed to retrieve model file 'onnx/model.onnx'`)
  — load it once and share it; and the cache path is resolved by walking up to the
  workspace root, because `cargo test` runs with the CWD set to the crate directory.
* Queries should go through `BgeEmbedder::embed_query` (it applies the instruction the
  model was trained for); passages through `Embedder::embed`. The frozen `Embedder`
  trait has a single method and cannot express that asymmetry.

## The refresh tick

Background freshness work is a discrete job, not a daemon loop yet:

```sh
SEARCH_DATA_DIR=/var/lib/search agent0 refresh
```

It builds the due sources the pipeline knows, runs the scheduler's bounded refresh work
(`search-agent0::DEFAULT_REFRESH_BUDGET`), and prints a JSON report. Run it from a timer
(e.g. a systemd `OnUnitActiveSec` timer) or by hand. Per-origin and total live fan-out
caps (`SchedulerConfig`) hold regardless of how often it runs.

Query-triggered refresh is **opt-in** on the search path:

```sh
SEARCH_QUERY_REFRESH=1 agent0 search "latest tokio release"
```

This runs a P0 query-triggered refresh before searching; it is off by default so ordinary
search stays hermetic. A P0 refresh fetches a not-yet-indexed doc and scores it against
the active query as a live overlay before any global reindex.

## Ingest and search (CLI)

```sh
agent0 ingest https://example.com/doc     # one URL through the full pipeline (prints JSON)
agent0 search "rust async runtime" 5      # query the live index (prints SearchResponse JSON)
agent0 serve                              # start the API (what the units run)
agent0 refresh                            # one background refresh tick (prints JSON)
```

## Backup and restore

A raw `cp` of a **live** `/var/lib/search` is unsafe: the WAL SQLite database
(`meta/meta.sqlite`) and Tantivy's `MmapDirectory` can be read torn mid-write. Stop the
writer first.

```sh
# Consistent backup: stops the search-* units, copies the tree, restarts, tars + SHA-256.
sudo scripts/backup.sh                                   # -> /var/backups/search/search-<UTC>/
sudo scripts/backup.sh --data-dir /var/lib/search --out /var/backups/search
sudo scripts/backup.sh --no-tar                          # keep the plain directory
sudo scripts/backup.sh --no-stop                         # UNSAFE for a live writer; warns loudly
```

`backup.sh` writes `BACKUP_INFO.txt` (source dir, UTC stamp, which units were stopped,
file/byte counts) and, unless `--no-tar`, `search-<UTC>.tar.gz` + `.sha256`.

**Restore** (inverse of the above):

```sh
sudo systemctl stop search-agent.service search-api.service
sudo rsync -a --delete /var/backups/search/search-<UTC>/data/ /var/lib/search/
sudo chown -R search:search /var/lib/search
sudo systemctl start search-agent.service
curl -fsS http://127.0.0.1:8080/health          # expect the restored generation
```

The whole data root is replaceable: Tantivy and the graph can be rebuilt from
`document_version` + blobs + the ingest journal (`STAGE0_SPEC.md` §3), so a backup of the
SQLite db + blobs alone is recoverable.

## Logs and traces

* **`RUST_LOG` is honoured.** A `tracing` subscriber is installed by
  `search_api::init_tracing()` (called from the `agent0` binary entrypoint and from
  `search_api::serve`), with an `env-filter` on `RUST_LOG`, default `info`. `deploy/*.service`
  sets `RUST_LOG=info`, so journald output is populated. URL userinfo is redacted before
  logging; document bodies are never logged.
* **Structured traces** (per-query `Phase`/`action`/`cost_ms` events) are produced by
  `crates/trace` (`PolicyTrace` + `JsonlTraceSink`) and **are wired into `agent0`**: set
  `SEARCH_TRACE_PATH=/var/log/search/trace.jsonl` (or call `Pipeline::set_trace_jsonl(path)`)
  and deep searches append trace events that `read_trace` reads back. See `STAGE8_SPEC.md`.
* **Health telemetry** is the `/health` endpoint above (published `generation`,
  `index_manifest_id`, uptime).

## Unpublished index (`degradations=["index_unpublished"]`)

The indexer's `manifest()` returns `None` until the first manifest is published
(`crates/indexer/src/store.rs`) — an unpublished index has no read-side state. The API
surfaces this rather than lying: when the searcher's manifest is generation `0`, the
`SearchResponse` carries `"degradations": ["index_unpublished"]` and `complete` reflects
the procedure, not a successful web search (`crates/api/src/query.rs`).

What to do:

1. `curl http://127.0.0.1:8080/health` → `generation: 0` means nothing is published.
2. Ingest at least one document and let the pipeline publish a manifest:
   `agent0 ingest <url>` (a successful `Ingested` outcome includes a `generation`).
3. Re-check `/health`; `generation` should increment and search responses should omit the
   `index_unpublished` degradation.
4. If the binary serving the API was built without the real indexer wiring, the searcher
   is a stub and will *always* report `index_unpublished`. The shipped `agent0` enables
   `search-api/real-indexer` (`crates/agent0/Cargo.toml`); a hand-built `search-api`
   without that feature will not find documents.

## Capacity and safety bounds

Defaults from `crates/core/src/limits.rs` (`Limits::default`). `Limits::validate` rejects
`max_robots_bytes < 500 KiB` and `max_decompressed_bytes < max_response_bytes`.

| bound | default | guards |
| :--- | :--- | :--- |
| `max_response_bytes` | 5 MiB | one HTTP response body |
| `max_decompressed_bytes` | 25 MiB | gzip/brotli/zip bomb |
| `max_redirects` | 5 | redirect chains (each hop SSRF-re-validated) |
| `max_parse_ms` | 2000 | one parse |
| `max_archive_expansion` | 100 MiB | archive expansion |
| `max_archive_depth` | 3 | nested archives |
| `max_pdf_pages` | 500 | PDF extraction |
| `max_browser_ms` | 15000 | one browser render |
| `max_robots_bytes` | 512 KiB | robots.txt parse (RFC 9309 needs ≥ 500 KiB) |
| `lease_seconds` | 120 | frontier job lease |
| `max_inflight_fetches` | 32 | global in-flight fetch cap |

The API additionally bounds concurrency (`DEFAULT_MAX_CONCURRENCY` in
`crates/api/src/server.rs`) and sheds load with HTTP **429** when saturated.

Sizing on the reference box (16 vCPU / 64 GB, no GPU), per the deploy plan: 32–64 async
HTTP tasks, 4–8 parsers, 2–4 PDF, 2 browser contexts, 2–4 indexers. This is a **single-box**
system today — see `ROADMAP.md`.
