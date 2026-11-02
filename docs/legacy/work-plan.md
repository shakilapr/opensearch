# Work Plan — Agent-Native Local Web Search Engine

Status: build plan derived from the four architecture documents in this repo. This is a
construction plan, not a status report. The UI shell (`ui/`) exists and currently runs
against mock/synthesized data. The Rust workspace skeleton now exists and compiles
(`Cargo.toml`, `crates/core`, `crates/metadata`, typed stubs for
`ingest`/`frontier`/`fetcher`/`indexer`/`api`/`agent0`); the frozen Stage-0 contract is
`docs/specs/STAGE0_SPEC.md`. See "Stage-0 review deltas" below for the changes folded in from
the architecture/consistency review.

Source documents this plan must satisfy (deduped by content):

| Doc | Covers |
| :--- | :--- |
| `docs/specs/CRAWLER.md` (= `crawler/*`, `docs/ARCHITECTURE.md`) | Crawler: 5 IDs, source registry, frontier, per-origin scheduling, robots, fetchers, routing, parsers, adapters, freshness, storage tiers, events, security |
| `engine/ARCHITECTURE.md` (= `docs/specs/AGENT_NATIVE_SEARCH_ENGINE.md`) | Engine: 3 planes, data model, retrieval lanes, query pipeline, ranking, agent policy, API, evaluation, 13 build stages |
| `docs/legacy/engine-architecture.md` | Algorithm spec v2: contracts, persistent records, retrieval lanes, RRF, cascade, budgets, coordinator, service boundaries, acceptance tests |
| `README.md` + `ui/README.md` | Repo topology, 5 IDs summary, modes, deployment, UI contract |

Governing principle (both crawler + engine docs): **a compact semantic map of where useful
information exists**, not a web archive. Every fetch answers *"how much will knowing or
refreshing this resource improve search?"*.

---

## 0. Toolchain and stack decision

Observed on the target VPS (`85.137.250.157`, 16 vCPU / 64 GB RAM / 665 GB free, no GPU):

- Node.js v26.7.0, npm 11.19.0 — present
- Python 3.14.7 — present
- Rust / cargo — **installed** (rustup, Rust 1.99.0, `/root/.cargo/bin`); C toolchain
  (`build-essential`, `pkg-config`, `libssl-dev`, `clang`, `lld`, `zlib1g-dev`) installed;
  crates.io reachable and Tantivy 0.22 fetch verified.

---

## Stage-0 review deltas (folded into this plan)

These were added before Stage 0 because they are painful to retrofit once the crawler is
producing data. The authoritative, frozen form is `docs/specs/STAGE0_SPEC.md`.

1. **Consistency model + append-only ingestion journal.** `crates/ingest` is the sole owner
   of the ingestion consistency boundary. An operational `ingest_op` / `ingest_seq` (NOT a
   sixth semantic identity beside the five IDs) records every transition; every projection
   is idempotent. Lifecycle:
   `FETCHED → NORMALIZED → BLOB_WRITTEN → METADATA_COMMITTED → LEXICAL_INDEXED →
   GRAPH_INDEXED → VISIBLE`. Search exposes only versions below a published
   watermark/manifest, so Tantivy and the graph can be rebuilt from metadata.
2. **SQLite is a single-writer subsystem from day one.** WAL, one writer, all mutations
   flow through a bounded queue (`crates/metadata`) that batches transactions. Crawler
   tasks never contend on the DB directly.
3. **Tantivy is designed around batches, not individual updates.** Two-level index:
   immutable-ish **base** + small **recent overlay**; new/live docs hit the overlay,
   a builder periodically folds them into base segments.
4. **Frontier leases + backpressure.** Every dispatched job carries `lease_until`,
   `attempt`, `owner`, and a monotonic `epoch` fencing token; a crashed worker's job
   becomes eligible again after expiry. All channels bounded (fetch→parse→index→graph);
   fetchers shed load when parsing lags.
5. **All safety/resource bounds are Stage-0 acceptance criteria**, not Stage-5 work:
   max response bytes, decompressed bytes, redirects, parse time, archive expansion,
   post-redirect DNS/IP validation, PDF size/pages, browser runtime.
6. **Exact RFC 9309 robots semantics, including failure modes:** a successfully fetched
   file's rules are obeyed; 4xx/unavailable MAY crawl; 5xx or network failure MUST
   disallow; cached robots ≤24h; parse limit ≥500 KiB. Covered by named tests, not a
   generic `allowed()` boolean.
7. **Benchmark from Stage 0/1:** 30–50 golden queries in Stage 0, 100–200 in Stage 1,
   growing into the serious 500–2,000-query set at Stage 2.
9. **CrawlPolicy ≠ RobotsPolicy** — kept as separate types in `crates/fetcher`.
10. **QueryAnalysis is a frozen contract** in `crates/core`.
12. **No vector DB yet** — dense lane deferred until the sparse/lexical base is measured.
13. **Pack versioning** for dataset packs (builder + `packs/`).
14. **Observability:** `tracing` + Prometheus; OTLP optional (OTel Rust is Beta).

Revised Stage-0 acceptance test: seed URL → robots → frontier lease → HTTP fetch →
normalize → ContentId → DocumentVersion → journal → Tantivy commit → manifest publish →
`/search`; then restart and confirm the same result, and re-ingest and confirm no
duplicate version.

`engine/ARCHITECTURE.md` §34 recommends Rust for the core. That is the right call for the
runtime (Tantivy, mmap indexes, frontier throughput), but it is a prerequisite to install,
not an assumption to code against.

Decision for this plan:

- **Core (crawler + engine runtime): Rust** — install via rustup in Stage 1. Tantivy is the
  lexical engine (Rust-native), so the core language follows the index.
- **Builder / dataset importers (Common Crawl, OpenAlex, dumps): Python** — DuckDB/PyArrow/
  Polars are strongest here per the crawler doc §4 and engine doc §5.
- **UI: TypeScript/React/Vite** — already present.
- **Browser + PDF + embedding workers: isolated processes** (Playwright, GROBID, FastEmbed)
  so they never contaminate the core dependency tree (crawler §21, engine §34).

If Rust install is blocked, the fallback is a Python core using `tantivy-py`; record the
decision in `docs/` before writing code so it is not revisited mid-build.

---

## 1. Target repository layout

Build out this tree. Existing dirs are marked `[exists]`.

```text
projects/search/
├── Cargo.toml                    # workspace root
├── crates/
│   ├── core/          # IDs, config, errors, time, metrics
│   ├── metadata/      # SQLite: domains, sources, resources, works, versions, crawl state
│   ├── graph/         # entities, concepts, typed edges, activation
│   ├── frontier/      # priority queue, per-origin queues, origin state
│   ├── fetcher/       # HTTP, conditional, robots, URL canon, SSRF guard
│   ├── crawler/       # scheduler, discovery, response router, adapters
│   ├── parser/        # HTML, PDF, feeds, sitemap, JSON/XML, OAI
│   ├── indexer/       # Tantivy lexical + overlay + manifests + ingestion log
│   ├── retrieval/     # lexical, sparse, dense, graph, source, freshness lanes
│   ├── ranking/       # RRF fusion, feature ranker, cross-encoder, diversify
│   ├── freshness/     # change model, refresh scheduler, conditional revalidate
│   ├── agents/        # SearchState, action frontier, policies, selector
│   ├── api/           # HTTP endpoints (axum), MCP server
│   └── eval/          # benchmark harness, judges, metrics
├── builder/                       # Python dataset importers
│   ├── commoncrawl/ openalex/ arxiv/ wikidata/ openlibrary/
│   └── packs/                     # pack manifest + release assembly
├── workers/
│   ├── browser/       # Playwright Chromium, contexts, network interception
│   ├── pdf/           # GROBID + text/OCR queues
│   └── embedding/     # FastEmbed/BGE-small CPU embeddings
├── adapters/          # per-source discovery/identity rules (data or code)
├── benchmarks/        # queries/, judgments/, ablations/, reports/
├── ui/                            # [exists] Vite + React search frontend
├── crawler/            [exists]   # architecture docs
├── engine/             [exists]   # architecture docs
├── docs/               [exists]   # architecture docs
├── models/             [exists]   # trained/local model artifacts (gitignored)
├── raw-dataset/        [exists]   # scraped web data + bulk dumps (gitignored)
├── deploy/            # systemd units, nginx, env templates
└── docs/legacy/work-plan.md        # this file
```

`raw-dataset/` is the on-disk home for **scraped web data**: raw HTML/PDF/JSON blobs, the
blob-cache, WARC/Parquet shard downloads, and normalized content tiers. Layout:

```text
raw-dataset/
├── blobs/<hash[0:2]>/<hash>          # Tier-3 raw cache, short TTL
├── warc/                             # Common Crawl / archive shards (transient)
├── parquet/                          # imported dataset shards (transient)
└── text/<hash[0:2]>/<hash>.zst       # Tier-2 extracted content
```

`models/` holds everything we produce or download as a model artifact:

```text
models/
├── embeddings/        # BGE-small / SPLADE weights (GGUF/ONNX/safetensors)
├── reranker/          # cross-encoder
├── classifier/        # routing/selector small models
└── policy/            # distilled search-policy checkpoints (Stage 9)
```

Both stay gitignored (already in `.gitignore`) and server-only.

---

## 2. The invariant: 5 IDs

Every crate and store resolves to these. Do not invent parallel identifiers.

| ID | Meaning | Example | Owner store |
| :--- | :--- | :--- | :--- |
| `DomainID` | hostname | `arxiv.org` | metadata |
| `SourceID` | logical collection targeted by routing | `docs.python.org/asyncio` | metadata + graph |
| `ResourceID` | fetchable address | URL / sitemap / feed / PDF | metadata + frontier |
| `WorkID` | logical intellectual object | a paper across preprint+publisher | metadata + graph |
| `ContentID` | deduplicated payload | syndicated article copies | indexer |

Plus engine-layer records from `engine-architecture.md` §4 that hang off those IDs:
`DocumentVersion`, `Passage`, `GraphEdge`, `Evidence`, `IndexManifest`, `PolicyTrace`.
Preserve unknown timestamps as unknown — never substitute crawl time.

---

## 3. Stage plan

Stages follow `README.md` §8 and `engine/ARCHITECTURE.md` §35/§24. Each stage lists
deliverables, the subsystems it exercises, and a **verifiable exit gate**. Do not proceed
past a gate until it is demonstrated with real output.

### Stage 0 — Skeleton & core infrastructure

Goal: URL → crawl → index → search works end-to-end before any volume exists.

Deliverables:
- Rust workspace + crates `core`, `metadata`, `frontier`, `fetcher`, `indexer`, `api`
  (stubs with real interfaces).
- `core`: the 5 IDs (strongly typed newtypes), config loader, error taxonomy, structured
  logging, `IndexManifest` type.
- `metadata` (SQLite): tables for domains, sources, resources, works, document versions,
  crawl state, freshness. Migrations. Idempotent upserts.
- `frontier`: `FrontierEntry` + per-origin `OriginState` (concurrency, next_allowed_time,
  error rate, adaptive delay) — **global queue dispatches through origin queues, never FIFO**
  (crawler §10–11).
- `fetcher`: persistent HTTP client (HTTP/1.1+2, pooling, gzip/br, redirects, timeouts,
  range + conditional requests, DNS cache, TLS reuse — crawler §17); URL canonicalization
  (crawler §13); SSRF guard blocking `127/8`, RFC1918, link-local, cloud metadata, `file://`
  (crawler §45).
- `robots`: centralized `RobotsPolicy` cache, RFC 9309 parsing, allow/deny middleware
  (crawler §6). One cache, not per-worker fetches.
- `indexer`: Tantivy index with fields title/headings/body/url/entities/source/type/date;
  BM25 search (engine §13, crawler §48 Tier-1).
- Exact-content dedup via `exact_hash` → `ContentID` (crawler §36 first half).
- `api`: `POST /search` returning the `SearchResponse` contract (engine-architecture §3),
  plus `GET /health`.
- Queue internals from `raw-dataset/blobs/` for Tier-3 raw cache with TTL.

Corpus: ~100 hand-selected pages (docs sites, a few repos, a couple of feeds).

Exit gate: a seed URL is fetched, robots-checked, parsed, indexed, and appears in
`/search` results, reproducibly, with `snapshot_id` and `timing_ms` returned.

### Stage 1 — 10K LLM Search MVP

Goal: the first real milestone — search complicated LLM/tech questions and get useful
results.

Deliverables:
- Sitemap discovery (crawler §7) and RSS/Atom feed discovery (crawler §8) → resource records
  without fetching every page.
- Response router by MIME (crawler §19); HTML route with Readability-style extraction.
- Browser escalation path (`workers/browser`): persistent Chromium + short-lived contexts,
  resource limits (max browser time, blocked ads/fonts/video, max DOM), and network
  interception to discover JSON endpoints for future direct fetch (crawler §21–22).
- PDF pipeline (`workers/pdf`): size check → hash dedup → text-layer test → classifier →
  GROBID for scholarly / generic parser / OCR queue (crawler §23).
- Source registry populated: `SourceProfile` with topics, entities, patterns, authority,
  change rate, js probability, historical fetch stats (crawler §3).
- Entity aliases + concept graph + source graph (engine §13) stored in `graph`/metadata;
  `WorkID` resolution for papers via DOI/arXiv/PMID (crawler §24).
- Source-first retrieval (engine §17) over the source graph + BM25.
- Freshness metadata captured per source (engine §11) — `published_at`, `modified_at`,
  `fetched_at`, `verified_at`, `feed_watermark` kept distinct (engine-architecture §14).
- Search UI wired to the real API (replace `hexxicaClient` mock in
  `ui/src/services/`), including the "Why this result?" signal breakdown (engine §26) and
  live overlay messaging (engine §27).

Corpus: ~10K indexed documents; ≥100K known URLs; hundreds–thousands of sources spanning
research/docs/GitHub/blogs/news/PDFs.

Exit gate: fixed query set (LLM-related, multi-word, ambiguous) returns relevant results
from the local index with no external search API, and the UI shows per-signal provenance.

### Stage 2 — Search quality & benchmarks

Stop adding volume. Build the measuring stick.

Deliverables:
- `benchmarks/`: 500–2000 queries across short factual, long semantic, ambiguous-entity,
  current, papers, docs, GitHub, community, multi-hop; LLM-judge + manual spot-check
  labels; time-split dynamic queries to avoid leakage (engine §29, engine-arch §22.1).
- Small CPU embeddings (`workers/embedding`, BGE-small ~67 MB) for source + selected
  passage vectors (engine §13).
- RRF fusion over lanes (engine-arch §10, c≈60 start, weights tuned).
- Feature ranker + small cross-encoder rerank of 40–100 candidates (engine §21).
- Entity disambiguation retaining hypotheses until context resolves (engine §14,
  engine-arch §6) — never collapse "Apple" early.
- Result diversification via greedy MMR objective (engine-arch §16).
- Ablations required: BM25; BM25+dense; +sparse; source-routing with/without global path;
  cross-encoder on/off (engine-arch §22.1).

Exit gate: hybrid retrieval measurably beats plain BM25 on nDCG@10 / MRR, at a resource
budget recorded next to the number.

### Stage 3 — Live freshness loop

Deliverables:
- HOT/WARM/COLD source classes; learned change model
  `P(changed) = 1 - exp(-rate × age)` (engine §11, crawler §39).
- Conditional revalidation: store ETag/Last-Modified, send `If-None-Match`/
  `If-Modified-Since`, treat 304 as unchanged-without-reindex (crawler §18).
- Query-triggered crawl: active queries ("latest X", "X today") spawn P0 refresh jobs that
  outrank background work (crawler §40, engine §12). Scheduler priority tiers:
  `P0 QUERY CRITICAL / P1 HOT / P2 NORMAL / P3 DISCOVERY / P4 BACKFILL`.
- Live overlay: new docs scored against the active query immediately, merged into results
  before global reindex (engine §20).
- Mutable recent-document overlay + immutable base segments; lane watermarks merge by
  version (engine-arch §5.3).
- Activation scheduler with bounded live-fetch fan-out (2–4 origins auto, 8 total cap) and
  atomic budget ledger charged before dispatch (engine-arch §13, §17).

Exit gate: a "latest/today" query returns current results the local index did not yet
contain, without any commercial search engine; refresh does not storm (per-origin limits
hold under load).

### Stage 4 — Agent navigation

Deliverables:
- `SearchState`, action frontier (`SearchAction`: retrieve/expand_graph/refresh/follow_link/
  rerank/verify_join/widen), exact visited/action-key set, cycle detection (engine-arch §15).
- Best-first traversal; beam search for ambiguity (beam 2–4, reserved slot per credible
  interpretation) (engine §18).
- Evidence joins for multi-hop: bind candidate identities, then retrieve dependent units
  with the same join key + compatible time order (engine §7, engine-arch §7).
- Policy hierarchy (engine-arch §12.1): rules → small classifier → small local model →
  bounded LLM; escalation only on important uncovered constraint + predicted gain >
  cost + admissible action + budget (engine-arch §12.4).
- Provider adapters: `AlgorithmPolicy`, `OpenAI`, `Claude`, `DeepSeek`, `GLM`,
  `CallerAgentPolicy`, `LocalPolicy` behind `SearchPolicy.choose(state, actions)`
  (engine §19). Agent gets only structured state + allowed action IDs; never a raw URL.
- Stopping: bounded/deadline/budget/coverage-driven; log plateau heuristics as heuristics
  (engine-arch §17).

Exit gate: difficult ambiguous/multi-hop queries improve vs one-shot retrieval, simple
queries stay in fast path, and no agent call happens when policy/rules suffice.

### Stage 5 — Scale to 100K–1M documents

Deliverables:
- Storage tiers enforced (crawler §48); archive-bomb + resource-limit guards
  (crawler §46–47).
- Near-duplicate detection: SimHash/MinHash + verification (crawler §36 second half).
- URL pattern learning (`/docs/*` useful, `/tag/*` low, `/calendar/*` trap) feeding
  frontier priority (crawler §14, §57).
- Adaptive crawl scheduling + compact graph encoding + compressed storage
  (engine §35 Stage 5).
- Measure at each step: index bytes/doc, crawl CPU/doc, search latency, RAM, dup ratio,
  relevance (engine §35).

Exit gate: p95 latency and quality stay stable as corpus grows 10K → 100K → 1M.

### Stage 6 — Dataset bootstrapping (the Data Builder)

Deliverables (`builder/`, Python, streaming):
- Common Crawl URL Index via DuckDB/Parquet → discovery, not full-page ingest.
- OpenAlex JSONL/Parquet snapshot → scholarly Work/entity/citation resolution.
- Wikidata / Wikipedia → entity + alias seeds.
- Open Library dumps → book Work/edition/author/location model.
- OAI-PMH adapter with resumption tokens + checkpoints (crawler §25).
- Rule: `download shard → normalize → keep useful records → delete shard`. Never copy
  upstream wholesale; discard raw after normalization (engine §5).
- Release packs: `core/developer/research/AI/science/books/news-source` containing
  entities, concepts, sources, URLs, titles, abstracts, edges, compact structures — **not**
  page dumps (engine §35 Stage 11).

Exit gate: source map grows orders of magnitude faster than direct crawl, on bounded disk,
with packs installable and loadable at runtime.

### Stage 7 — Service surfaces & deployment

Deliverables:
- HTTP API finalized (engine §28): `/search`, `/search_deep`, `source_lookup`,
  `source_refresh`, `read`, `find_in_document`, `follow_path`, `backtrack`.
- MCP server exposing the same tools so Hermes/OpenClaw see "localhost search" (engine §28).
- `deploy/`: systemd units for api, scheduler, http workers, parser workers, pdf worker,
  browser worker, indexer; nginx for UI; env templates. Modular monolith + background
  workers first; split services only when isolation justifies it (engine-arch §2).
- Artificial 100 GB storage budget enforced in early stages (engine §33).
- UI build/deploy via existing `ui/package.json` `deploy` script to the web root.
- Windowed deployment on the 16 vCPU / 64 GB box: 32–64 async HTTP tasks, 4–8 parsers,
  2–4 PDF, 2 browser contexts, 2–4 indexers (crawler §51).

Exit gate: reboot → services come back; UI, HTTP API, and MCP all serve the same index.

### Stage 8 — Evaluation, learning, and hardening

Deliverables:
- Benchmark matrix (engine-arch §22.1) + the 12 acceptance tests (§22.2) as automated
  integration tests — including "Apple" ambiguity, identifier survival across rewrites,
  unselected-source recall via global lane, overlay-before-embedding, homepage-refresh ≠
  article freshness, no cross-person join, syndication ≠ corroboration, SSRF blocking,
  partial responses on lane/deadline failure, tombstone/permission suppression in caches.
- `PolicyTrace` logging, scope-safe traces, propensity logging for offline policy eval
  (engine §30, engine-arch §22).
- Source priors with smoothing/decay + cold-start exploration; versioned model/graph
  rollout with rollback.
- Caches with owners + invalidation rules; exact result-cache key includes query, context
  hash, filters, tz, temporal interval, scope version, index manifest, ranking policy,
  freshness (engine-arch §20).

Exit gate: full acceptance suite green; calibration/reliability reported for any
probabilistic predictor; degradation paths return valid partial responses.

### Stage 9 — Optional advanced retrieval & policy distillation

Only on benchmark evidence that it helps *our* query set.

Deliverables:
- SPLADE/BMP, filter-aware ANN (ACORN-style), quantization, and optionally MUVERA or WARP
  evaluated as **distinct backends**, not interchangeable rerankers (engine §21,
  engine-arch §11).
- Distill agent traces (`SearchState`+actions+outcome) into a 20M–150M param local policy
  (engine §31); compare vs Claude/OpenAI/DeepSeek on traces.

Exit gate: each adopted technique improves the measured quality/cost frontier at matched
relevance/cost; policy matches teachers on navigation without paid inference.

### Stage 10+ — Distribution & community

- Distributed crawl (scheduler cluster → origin queues → workers → parsers → distributed
  index) only after quality is proven (engine §35 Stage 10).
- Public pipeline: community source packs, adapters, judgments, privacy-preserving
  telemetry (engine §35 Stage 12).

---

## 4. Cross-cutting workstreams (run in every stage)

Security (crawler §45, engine §32, engine-arch §21):
- SSRF guard on every URL + redirect incl. DNS rebinding; private/link-local/metadata blocked.
- Crawled content is `UNTRUSTED_WEB_DATA`: never instructions, never tool grants, never
  fetch-destination changes.
- Bounds on decompression, download bytes, redirects, parse time, browser time, PDF pages,
  archive nesting; never trust remote `Content-Length`.
- Access-scope filtering enforced in indexes, graph, caches, snippets, and evidence joins;
  recheck at finalization.

Caching (crawler §49, engine-arch §20): DNS, robots, HTTP body, parsed doc, browser result,
sitemap, feed, entity/source — each with its own owner + TTL.

Events (crawler §53): `DOMAIN_DISCOVERED`, `RESOURCE_DISCOVERED/FETCHED/UNCHANGED/FAILED`,
`DOCUMENT_PARSED`, `WORK_RESOLVED`, `SOURCE_DISCOVERED`, `LINK_DISCOVERED`,
`CONTENT_CHANGED`, `RESOURCE_DELETED` — components communicate through these so distribution
is a config change, not a rewrite.

Failure handling (crawler §42–44): classify DNS/conn/TLS/timeout/429/403/404/410/5xx/parse/
browser/oversize/robots; 429 obeys Retry-After; retry belongs in the frontier with
exponential backoff + jitter, not sleeping workers; deletions set `deleted_at` and move
`ACTIVE→STALE→DELETED→ARCHIVED` without erasing history.

Observability: structured traces with query class, plan, candidate provenance, actions,
latency/cost, gaps, freshness outcome, model/index versions.

---

## 5. Definition of done (MVP, engine §37)

The MVP is complete when all of these work **together** — not each in isolation:

```text
crawler + source registry + URL frontier
normal fetch + feeds + sitemaps + PDF handling
entity/concept/source graph
BM25 + semantic source retrieval + reranking
freshness check + query-time refresh
normal search UI
agent API (HTTP + MCP)
benchmark / evaluation system
```

The full architecture must be present; individual components may start simple. Scaling is
then iterative improvement, not a rewrite.

---

## 6. Immediate next actions (Stage 0 kickoff)

1. Install Rust toolchain (`rustup`) — prerequisite for the core and Tantivy.
2. Create `Cargo.toml` workspace + `crates/core` with the 5-ID newtypes and error/config.
3. Create `crates/metadata` SQLite schema + migrations for the persistent records.
4. Implement `crates/frontier` (frontier entry + per-origin queues) and `crates/fetcher`
   (HTTP client, canonicalization, robots, SSRF guard).
5. Stand up `crates/indexer` with a Tantivy schema matching the fielded design.
6. Wire `crates/api` `POST /search` + `/health` to the index.
7. Seed ~100 pages through the real pipeline; verify exit gate with real output.
