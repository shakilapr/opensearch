# Agent-Native Local Web Search Engine

An open-source, self-hosted search engine built for **AI agents** (Hermes, OpenClaw, and
custom autonomous agents) and human users. It runs on a single standard VPS (16 vCPU /
64 GB RAM, **no GPU**) with **no commercial search API**.

> **A compact semantic map of where useful information exists** — not a web archive.
> Every fetch answers *"how much will knowing or refreshing this resource improve search?"*

```text
AGENT        ->  Judgment   (interprets ambiguity, navigates hard queries)
SEARCH ENGINE->  Reflexes   (query planner, lanes, candidate fusion, reranking)
WEB MEMORY   ->  Memory     (Tantivy lexical index, entity/source graph, source profiles)
CRAWLER      ->  Senses     (politeness, discovery frontier, freshness monitoring)
LIVE WEB     ->  Reality
```

## Repository layout

```text
projects/search/
├── Cargo.toml            # Rust workspace (32 members)
├── crates/               # the engine + crawler (Rust)
├── ui/                   # search frontend (Vite + React + TypeScript)
├── deploy/               # systemd units + nginx (see deploy/README.md)
├── benchmarks/           # Stage-2 smoke set: corpus/ + queries.example.jsonl + results.json
├── datasets/fixtures/    # Stage-6 adapter fixtures (committed, hermetic)
├── docs/                 # specs + architecture (see below)
├── scripts/              # build-release.sh, publish-pack.sh, backup.sh
├── raw-dataset/          # [server-only, gitignored] scraped data & dumps
├── models/               # [server-only, gitignored] model weights
└── docs/legacy/work-plan.md          # the 11-stage build plan
```

## The pipeline

Everything resolves to **five semantic IDs** (`crates/core`): `DomainId`, `SourceId`,
`ResourceId`, `WorkId`, `ContentId`. Ingestion is an append-only, idempotent state machine
(`crates/ingest`):

```text
seed URL -> robots -> frontier lease -> HTTP fetch -> normalize -> ContentId
        -> DocumentVersion -> ingest journal -> Tantivy commit -> manifest publish -> /search
```

Search exposes a document only once it is below a published watermark/manifest, so the
index is always rebuildable from metadata + blobs + the journal. The workspace crates:

| crate | package | owns |
| :--- | :--- | :--- |
| `core` | `search-core` | 5 IDs, config, error taxonomy, `IndexManifest`, safety `Limits`, time (no I/O) |
| `metadata` | `search-metadata` | SQLite **single-writer** subsystem, migrations, batched `Mutation` writer |
| `ingest` | `search-ingest` | ingestion state machine + append-only journal (consistency boundary) |
| `frontier` | `search-frontier` | per-origin priority dispatch, leases + epoch fencing |
| `fetcher` | `search-fetcher` | HTTP client, URL canonicalization, SSRF guard, robots, `CrawlPolicy` |
| `parser` | `search-parser` | response normalization (HTML → `NormalizedDoc`) |
| `indexer` | `search-indexer` | Tantivy base + overlay, manifest publish |
| `discovery` | `search-discovery` | sitemap + RSS/Atom feed discovery |
| `router` | `search-router` | MIME-based response routing |
| `browser` | `search-browser` | browser escalation (hermetic mock; real Chromium behind `chromium`) |
| `pdf` | `search-pdf` | PDF pipeline (size → hash → text-layer → classify) |
| `sources` | `search-sources` | source registry + Bayesian-ish priors |
| `graph` | `search-graph` | entity / concept / source graph, scholarly `WorkId` resolution |
| `retrieval` | `search-retrieval` | retrieval lanes (BM25, source-first, sparse expander) |
| `eval` | `search-eval` | metrics, dataset loading, ablation runner |
| `embed` | `search-embed` | embedding lane (`HashEmbedder` default; BGE behind `bge`) |
| `rerank` | `search-rerank` | RRF fusion, feature ranker, cross-encoder iface, MMR, disambiguation |
| `scheduler` | `search-scheduler` | freshness classes, change model, budget ledger, activation scheduler |
| `bench` | `search-bench` | ablation harness — binary `bench` |
| `nav` | `search-nav` | `SearchState`, action frontier, policy tiers, provider adapters |
| `dedup` | `search-dedup` | SimHash64, MinHash + LSH, shingling |
| `tiers` | `search-tiers` | HOT/WARM/COLD storage tiers + move planner |
| `patterns` | `search-patterns` | URL templatization + pattern learning |
| `dataset` | `search-dataset` | Common Crawl / Wikidata / OpenAlex / OAI-PMH adapters + release packs |
| `mcp` | `search-mcp` | stdio MCP server — binary `search-mcp` |
| `trace` | `search-trace` | `PolicyTrace` + JSONL sink |
| `cache` | `search-cache` | TTL + LRU caches with generation invalidation |
| `ann` | `search-ann` | int8 quantization, IVF, HNSW-lite |
| `distill` | `search-distill` | deterministic linear policy distillation |
| `acceptance` | `search-acceptance` | workspace acceptance tests (see gaps) |
| `api` | `search-api` | HTTP API (`POST /search`, `GET /health`); library, served by `agent0` |
| `agent0` | `search-agent0` | the driver binary — `agent0` |

## Quick start

```sh
# 1. Build everything.
cargo build --workspace                 # release binaries: scripts/build-release.sh

# 2. Ingest a URL (prints IngestOutcome JSON), then query it.
agent0 ingest https://example.com/doc
agent0 search "rust async runtime" 5

# 3. Serve the HTTP API (this is what the systemd units run).
agent0 serve                            # POST /search, GET /health on 127.0.0.1:8080
curl -fsS http://127.0.0.1:8080/health

# 4. One background refresh tick (Stage-3 freshness loop).
agent0 refresh

# 5. Benchmark the retrieval lanes.
cargo run -p search-bench --release     # writes benchmarks/results.json

# 6. UI.
cd ui && npm ci && npm test && npm run build
```

Configuration is via the environment (`crates/core::Config::from_env`):
`SEARCH_DATA_DIR` (default `/var/lib/search`), `SEARCH_BIND` (default
`127.0.0.1:8080`), `SEARCH_USER_AGENT`, `RUST_LOG`. `SEARCH_QUERY_REFRESH=1 agent0 search
"latest X"` opts into a P0 query-triggered refresh. See `docs/OPERATIONS.md`.

## Service surfaces

* **HTTP API** — `POST /search` (the `SearchResponse` contract in `docs/specs/STAGE0_SPEC.md` §7)
  and `GET /health` (`generation`, `index_manifest_id`, uptime). Served by `agent0 serve`.
* **MCP server** — `search-mcp`, JSON-RPC 2.0 over stdio, for MCP clients (Hermes, Claude
  Desktop). Register it with `command`/`args`; see [`docs/specs/MCP.md`](docs/specs/MCP.md).

## Data & server-only paths

`raw-dataset/` (scraped blobs, WARC/Parquet, extracted text), `models/` (weights), built
packs, and the runtime data root `/var/lib/search` (WAL SQLite + Tantivy mmap + blobs +
text) are **gitignored and server-only**. A WAL SQLite database or a Tantivy
`MmapDirectory` copied mid-write is corrupt — the data root must live outside any synced
path (`Config::assert_not_synced` enforces this). See `docs/specs/DISTRIBUTION.md`.

## Where the specs live

`docs/` is the authority. If code and a spec disagree, the spec wins until a commit says
otherwise.

| doc | covers |
| :--- | :--- |
| `docs/legacy/work-plan.md` | the 11-stage build plan |
| `docs/specs/STAGE0_SPEC.md` … `docs/specs/STAGE10_SPEC.md` | the frozen contract for each stage |
| `docs/ARCHITECTURE.md`, `docs/specs/AGENT_NATIVE_SEARCH_ENGINE.md`, `docs/specs/CRAWLER.md` | the original architecture documents |
| `docs/specs/MCP.md` | MCP server registration, tools, protocol |
| `docs/specs/DISTRIBUTION.md` | what ships, pack format/versioning, server-only paths |
| `docs/OPERATIONS.md` | runbook: services, refresh, backup/restore, bounds |
| `docs/specs/ROADMAP.md` | stage status + honest remaining gaps |
| `docs/specs/CONTRIBUTING.md` | build/test commands and the per-stage workflow |
| `deploy/README.md`, `benchmarks/README.md`, `ui/README.md` | deployment, bench, UI |

## Status

The build stages and their real status are in [`docs/specs/ROADMAP.md`](docs/specs/ROADMAP.md). In
short: the single-box spine (Stages 0–10) is implemented and covered by hermetic tests.
All three MCP tools (`search`/`deep_search`/`fetch_doc`) are backed by `HttpBackend` — a
loopback HTTP client, so MCP still needs a running API, but nothing is stubbed. The Stage-8
`acceptance` crate holds **13 hermetic tests** and is green; the browser escalation path is
**wired** (`SEARCH_BROWSER=1`, feature `chromium`); the entity-graph lane **runs**.
Distributed crawl is not started. Known gaps include no real BGE/SPLADE weights, no trained
cross-encoder (deterministic lexical proxies are used and labelled as such), single-box
only, and a frontier single-writer exception. Read that file before relying on a stage.

Corpus figures are **dated**, because they drift as the crawler runs. As of **generation
19254 (2026-10-11)** the *serving* index holds **19 255 documents across 222 hosts**; a
separate four-shard *fleet* build has grown to **~323 000 documents across 3 948 hosts** and
is **not yet imported** into the serving index (`ops/state.json` carries the live counts;
tracked as the `deploy-fleet-corpus` work item). Cite the generation, not the prose.
