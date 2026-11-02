# Codebase SLOC & Data Inventory Report

Measured in this workspace on 2026-10-24. Numbers are measurements, not estimates, except
where explicitly marked "approx" or "est".

Tooling: `cloc 1.98`. `cloc` was not installed and the root filesystem was 100% full, so the
Debian/Ubuntu `.deb` plus its Perl dependencies were unpacked under `/tmp/opencode/clocroot`
and run from there (`PERL5LIB=/tmp/opencode/clocroot/usr/share/perl5`). Counts cover tracked
and untracked working-tree files; `target*/`, `node_modules`, `.git` are excluded unless stated.

Definitions used below:

* **code** = cloc "code" lines (no blanks, no comments).
* **prod approx** = crate `src/` code minus the inline `#[cfg(test)] mod tests` block, measured
  as non-blank/non-`//` lines from the marker to EOF. It is a close lower bound, not exact.
* **test-dir** = code under `crates/<crate>/tests/`.
* **inline test** = the subtracted `#[cfg(test)]` lines (reported separately).

---

## 1. Executive summary

| Metric | Value |
| :--- | ---: |
| First-party code (tracked files, all languages) | **59,577** lines |
| Rust (36 workspace crates, src + tests) | **40,797** lines |
| Rust production logic (approx) | **23,792** lines |
| Rust test code (test dirs + inline tests) | **17,005** lines |
| Rust test share | **~42% of Rust, ~30% of src** |
| UI source (TypeScript + CSS, excluding tests) | **3,011** lines (+548 test = 3,559 total) |
| Docs/specs (Markdown, first-party) | **~9,966** lines |
| Vendored reference repos (not ours) | **649,227** lines |
| Code in the 4 biggest crates (`agent0`, `engine`, `api`, `metadata`) | **34.8%** of prod code |
| Code in the 10 smallest crates | **~8%** of prod code |
| Serving index | **19,255 docs / 222 hosts / 6.1 GB** |
| Fleet build (not yet serving) | **1,090,819 doc versions / 129,299 hosts / 307 GB** |
| Raw crawl cache | **873,585 blobs / 40.8 GiB / 1.81M crawl attempts** |
| Query corpora | **2.08M query rows / 1.62M unique / 7.7 GB** |
| Runtime data : source code | **~373 GB data vs 59.6k lines → ~6.3 MB per line** |

The codebase is small and dense (a 24k-line Rust production core) sitting under an unusually
large data payload (~373 GB). The asymmetry is the defining property of this project: **the
engine is ~60k lines; the data it operates on is ~6 million times larger by byte count.**

---

## 2. Top-level parts of the repository

| Part | Purpose | Files | Code (cloc) | On-disk |
| :--- | :--- | ---: | ---: | ---: |
| `crates/` | Rust workspace: engine + crawler spine, 36 crates | 251 | 41,597 | 3.0 MB |
| `ui/` | React/Vite search frontend (source only) | 34 | 3,011 (+548 test) | 244 KB |
| `ui/` (full, incl. `package-lock.json`) | Frontend + lockfile | 44 | 6,766 | 121 MB |
| `docs/` | Frozen stage specs, architecture, runbooks | 26 | 5,881 | 328 KB |
| `crawler/` | Crawler spec + seed list (`llm-tech.txt`, 402 seeds) | 3 | 2,130 | 100 KB |
| `engine/` | Engine spec (`§6–§18`) + mapping to code | 3 | 2,191 | 108 KB |
| `benchmarks/` | 30-doc corpus, 3 query sets, `results.json` | 35 | 1,256 | 332 KB |
| `datasets/fixtures/` | Hermetic adapter fixtures (CC, Wikidata, OpenAlex, OAI-PMH, ANN, distill) | 8 | 263 | 80 KB |
| `scripts/` | Build, crawl, eval, backup, deploy, pack scripts | 10 | 803 | 68 KB |
| `tools/progress_watch.py` | Work-plan watcher (runs `auto`, dispatches `code`) | 1 | 807 | 56 KB |
| `ops/` | Watcher state, plan, progress journal, dispatch logs | 5 JSON + logs | 876 | 1.8 MB |
| `deploy/` | systemd units + nginx config + README | 1 md (+units) | 55 | 32 KB |
| `references/repos/` | Vendored study repos: tantivy, pisa, pyserini, seismic, splade, faiss | 3,137 | 649,227 | 123 MB |
| `references/research/` | Index-algorithm research note | 1 | 9,100 B | 9 KB |
| `dist/` | Prebuilt `agent0`, `bench`, `search-mcp` binaries | 4 | 0 | 33 MB |
| `raw-dataset/` | Server-only crawl blobs/logs/frontiers (gitignored) | 873,585 | 0 | 51 GB |
| `models/` | BGE-small weights (fastembed, gitignored) | 11 | 0 | 128 MB |
| `target/`, `target-main/`, `target-browser/` | Cargo build artifacts (gitignored) | — | 0 | 87 + 76 + 4 GB |

Build artifacts (167 GB) and the server data roots are not source; they are listed only so the
storage picture is complete.

---

## 3. Per-crate SLOC (all 36 crates)

Sorted by production code. `total code` includes `src/`, `tests/`, `build.rs`-free TOML/SQL
files that cloc counts. Percentages are of the 23,792-line production total.

| Crate | What it does | Files | Total code | Prod approx | Test-dir | Inline test | % prod |
| :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| `agent0` | Driver binary: ingest/search/serve/crawl/refresh/export/import/graph, pipeline composition | 21 | 7,132 | 4,042 | 2,425 | 608 | 17.0% |
| `engine` | Query-time algorithmic core: analyze, units, plan, budget, fuse, evidence, diversify, coord | 17 | 3,729 | 2,162 | 1,530 | 27 | 9.1% |
| `api` | HTTP API: `POST /search`, `/search_deep`, `GET /health`, `/read`; engine route + graph lane | 14 | 3,930 | 1,958 | 1,259 | 685 | 8.2% |
| `metadata` | SQLite single-writer, migrations, batched `Mutation` writer | 12 | 2,421 | 1,279 | 906 | 0 | 5.4% |
| `dataset` | Common Crawl / Wikidata / OpenAlex / OAI-PMH adapters + release packs | 12 | 1,627 | 1,102 | 469 | 42 | 4.6% |
| `graph` | Entity/concept graph, co-occurrence, sidecar, scholarly `WorkId` resolution | 6 | 1,458 | 950 | 0 | 496 | 4.0% |
| `organize` | LLM page organizer: raw web text → structured records, disk cache, model chain | 11 | 1,599 | 893 | 240 | 443 | 3.8% |
| `fetcher` | HTTP client, URL canonicalization, SSRF guard, robots, `CrawlPolicy` | 12 | 1,490 | 859 | 423 | 184 | 3.6% |
| `indexer` | Tantivy base + overlay, manifest publish, frozen schema | 6 | 1,288 | 857 | 41 | 371 | 3.6% |
| `nav` | Agent `SearchState`, action frontier, beam search, policy tiers | 10 | 1,241 | 833 | 365 | 31 | 3.5% |
| `bench` | Ablation harness binary `bench` | 4 | 1,009 | 774 | 70 | 138 | 3.3% |
| `answer` | Opt-in LLM answer generation, model chain/fallback, evidence prompts | 9 | 1,280 | 760 | 235 | 261 | 3.2% |
| `core` | 5 semantic IDs, config, errors, `IndexManifest`, limits, time (no I/O) | 12 | 691 | 567 | 0 | 108 | 2.4% |
| `discovery` | Sitemap + RSS/Atom discovery (pure parsing) | 5 | 776 | 562 | 0 | 200 | 2.4% |
| `retrieval` | Retrieval lanes: BM25, source-first, dense adapter, sparse expander | 7 | 1,056 | 552 | 74 | 413 | 2.3% |
| `mcp` | stdio MCP server `search-mcp` (`search`, `deep_search`, `fetch_doc`) | 9 | 982 | 531 | 435 | 0 | 2.2% |
| `frontier` | Per-origin priority dispatch, leases, epoch fencing | 6 | 823 | 505 | 275 | 23 | 2.1% |
| `embed` | Embedding lane (`HashEmbedder`; BGE behind feature) + vector sidecar | 3 | 780 | 488 | 0 | 278 | 2.1% |
| `scheduler` | Freshness classes, change model, budget ledger, activation | 9 | 973 | 439 | 0 | 525 | 1.8% |
| `ann` | int8 quantization, IVF, HNSW-lite | 2 | 600 | 418 | 0 | 173 | 1.8% |
| `sources` | Source registry, source profiles/priors, arXiv helpers | 3 | 781 | 406 | 0 | 361 | 1.7% |
| `rerank` | RRF fusion, feature ranker, cross-encoder iface, MMR, disambiguation | 8 | 597 | 332 | 0 | 253 | 1.4% |
| `ingest` | Ingest state machine + append-only journal (consistency boundary) | 6 | 514 | 314 | 179 | 0 | 1.3% |
| `browser` | Browser escalation (hermetic mock; Chromium behind feature) | 4 | 526 | 307 | 52 | 149 | 1.3% |
| `eval` | nDCG/MRR/recall, dataset loading, ablation runner | 3 | 552 | 254 | 45 | 241 | 1.1% |
| `planner` | Opt-in LLM query planning → expansion probes | 5 | 408 | 251 | 140 | 0 | 1.1% |
| `distill` | Deterministic linear policy distillation | 3 | 352 | 236 | 24 | 81 | 1.0% |
| `dedup` | SimHash64, MinHash + LSH, shingling | 7 | 458 | 213 | 79 | 158 | 0.9% |
| `parser` | Response normalization (HTML → `NormalizedDoc`) | 3 | 298 | 212 | 0 | 72 | 0.9% |
| `patterns` | URL templatization + pagination/pattern learning | 2 | 243 | 166 | 0 | 69 | 0.7% |
| `pdf` | PDF: size → hash → text layer → classify | 3 | 295 | 156 | 127 | 0 | 0.7% |
| `router` | MIME/magic-byte/extension response routing | 2 | 287 | 131 | 0 | 148 | 0.6% |
| `trace` | `PolicyTrace` + JSONL sink | 2 | 260 | 125 | 0 | 125 | 0.5% |
| `cache` | TTL + LRU caches with generation invalidation | 2 | 214 | 109 | 0 | 99 | 0.5% |
| `tiers` | HOT/WARM/COLD storage tiers + move planner | 2 | 137 | 48 | 0 | 80 | 0.2% |
| `acceptance` | Workspace-level acceptance tests (13 hermetic tests) | 9 | 790 | 1 | 770 | 0 | 0.0% |
| **total** | 36 crates | **251** | **41,597** | **23,792** | **10,163** | **6,842** | **100%** |

### 3.1 Where the production logic really lives

Largest source modules (code lines):

| Lines | Module |
| ---: | :--- |
| 2,074 | `crates/agent0/src/pipeline.rs` (ingest lifecycle / composition) |
| 1,093 | `crates/indexer/src/store.rs` (Tantivy store, manifest, schema hash) |
| 1,083 | `crates/metadata/src/writer.rs` (single-writer, batching, WAL) |
| 920 | `crates/api/src/query.rs` |
| 884 | `crates/api/src/engine.rs` |
| 644 | `crates/engine/src/coord.rs` (coordinator loop) |
| 612 | `crates/engine/src/analyze.rs` |
| 592 | `crates/agent0/src/main.rs` (CLI) |
| 517 | `crates/embed/src/lib.rs` |
| 461 | `crates/frontier/src/queue.rs` |
| 447 | `crates/agent0/src/deep.rs` |

### 3.2 Test concentration

* `agent0`: 2,425 test-dir + 608 inline lines ≈ **75% as much test code as production code**.
* `engine`: 1,530 + 27 ≈ **72%**.
* `metadata`: 906 + 0 ≈ **71%**.
* `mcp`: 435 + 0 ≈ **82%**.
* `pdf`: 127 + 0 ≈ **81%**.
* Zero dedicated test-dir tests but inline `#[cfg(test)]` suites: `scheduler` (525),
  `graph` (496), `retrieval` (413), `sources` (361), `rerank` (253), `scoring`-adjacent
  modules. These still have tests; cloc simply counts them inside `src/`.
* Zero tests at all (no test dir, no `#[cfg(test)]`): **none found**; every crate has at least
  an inline module or a test dir.
* Total test markers: **824** `#[test]`/`#[tokio::test]` attributes in `crates/`, plus **36**
  Vitest tests in `ui/src`.

### 3.3 The smallest parts (and what they do)

| Crate | Prod lines | What it is |
| :--- | ---: | :--- |
| `acceptance` | 1 | Container crate whose value is its 770-line test suite (cross-crate contracts). |
| `tiers` | 48 | Classifies a document into HOT/WARM/COLD and plans moves — pure function. |
| `cache` | 109 | TTL+LRU cache, generation invalidation — small by design. |
| `trace` | 125 | Emits/reads `PolicyTrace` JSONL events. |
| `router` | 131 | Content-Type → magic → extension routing; deliberately tiny. |
| `pdf` | 156 | PDF gate: size, hash dedup, text-layer test, classify. |
| `patterns` | 166 | URL templatization (digits/UUID/dates → tokens). |
| `parser` | 212 | HTML normalization entry point. |
| `dedup` | 213 | SimHash/MinHash/LSH over shingles. |
| `distill` | 236 | Fits a tiny linear policy from traces. |

---

## 4. Non-crate code parts

| Part | Language mix | Code | Role |
| :--- | :--- | ---: | :--- |
| `ui/src` | TS 2,838 + CSS 721 (of which 548 test) | 3,011 non-test | React frontend: search box, results, provenance panel, telemetry, image grid, pagination; API client `hexxicaClient.ts` (422) is the largest module. |
| `ui/src` tests | TS | 548 | 6 Vitest files. |
| `docs/` | Markdown | 5,881 | Frozen Stage 0–10 contracts + architecture + ops. Largest: `AGENT_NATIVE_SEARCH_ENGINE.md` 1,561; `STAGE0_SPEC.md` 414; `OPERATIONS.md` 283. |
| `crawler/` | Markdown + Text | 2,130 | `UNIVERSAL_CRAWLER_ARCHITECTURE.md` (1,697) + section→code map + 402-seed list. |
| `engine/` | Markdown | 2,191 | Engine architecture/spec map. |
| `benchmarks/` | JSON 948, HTML 210, MD 98 | 1,256 | 30 HTML docs, 604+31+64 query JSONL lines, committed `results.json`. |
| `scripts/` | Shell 703, Python 100 | 803 | Build, corpus crawl, bench ingest, engine eval, fleet merge, UI deploy, pack publish, backup, scale-crawl. Largest: `engine-eval.sh` 174. |
| `tools/` | Python | 807 | `progress_watch.py`: executes `auto` plan items, dispatches `code` items, verifies evidence. |
| `ops/` | JSON | 876 | `plan.json` (16 items: 11 done, 2 todo, 2 in progress, 1 blocked), `state.json`, `progress.jsonl` (481 rows), dispatch logs (~1.3 MB text). |
| `deploy/` | Markdown + unit files | 55 | systemd `search-api`/`search-agent`/`progress-watch` + nginx. |
| `datasets/fixtures/` | JSON 167, XML 96 | 263 | Hermetic inputs for the Stage-6 adapters. |
| `references/repos/` | C++ 108,498; Rust 146,334; Python 89,377; headers 54,140; CUDA 26,740; JSON 102,310; … | 649,227 | **Vendored, not first-party**: tantivy 248,607; faiss 231,389; pyserini 104,691; pisa 46,853; seismic 11,837; splade 5,814. Exclude from "our code". |

Working notes (`docs/legacy/next-move.md` 56 KB, `recieved.md` 49 KB, `to-be-sent.md` 22 KB,
`docs/legacy/analysis-next-move.md`) are untracked Markdown and not included in the committed totals.

---

## 5. Data: what was crawled, downloaded, and indexed

The project has **five distinct data planes**. They must not be added together as if they were
one corpus.

### 5.1 Serving index — `/var/lib/search` (6.1 GB)

| Quantity | Value |
| :--- | ---: |
| Indexed documents (`document_version`) | **19,255** |
| Distinct hosts with ≥1 doc | **222** |
| Manifest generation / published doc_count | **19254 / 19,255** |
| Content rows | 18,947 |
| Raw blob store | 18,947 files, **4.53 GiB** (avg 251 KB) |
| Extracted text store | 18,947 files, **281 MB** (avg 14.8 KB/doc) |
| Estimated words (sampled 500 files, 0.145 words/byte) | **~40.7M words** |
| Tantivy overlay | 314 MiB (`.store` 114 MiB, `.pos` 116 MiB, `.idx` 35 MiB) |
| `meta.sqlite` (+WAL) | 85 MiB + 5.6 MiB |
| Structured pack (`pack-structured/`) | 19,058 records in 39 shards, 286 MiB |
| LLM response cache | 106,142 files, 430 MiB (avg 872 B) |
| Graph sidecar | 14,659 entities, 130,359 co-occurrence edges, 211 arXiv works; 29 MiB |
| Dense sidecar `vectors.bin` | 19,255 vectors × 384 dims f32, 29.3 MiB |
| Frontier rows | 28,236 (NEW 21,774 · DONE 4,789 · FAILED 979 · FETCHING 692 · READY 2) |
| Source profiles | 249 |
| Storage amplification | raw→text **16.9×**, text→index **1.17×**, raw→index **14.4×** |

This is what the live API serves today: ~19k pages, ~40M words, ~4.5 GiB of raw HTML compressed
into ~0.3 GiB of extracted text and a ~0.3 GiB inverted index.

### 5.2 Fleet build — `/var/lib/search-fleet` (307 GB, **not yet serving**)

| Shard | Resources | Content | Doc versions | Hosts w/ docs | Frontier URLs |
| :--- | ---: | ---: | ---: | ---: | ---: |
| 0 | 180,393 | 177,382 | 180,481 | 42,668 | 2,797,499 |
| 1 | 367,045 | 360,452 | 367,311 | 31,142 | 1,708,383 |
| 2 | 370,897 | 364,068 | 371,071 | 24,118 | 1,839,429 |
| 3 | 171,849 | 168,462 | 171,956 | 31,819 | 2,705,444 |
| **total** | **1,090,184** | **1,070,364** | **1,090,819** | **129,299 (union)** | **9,050,755** |

| Store | Size |
| :--- | ---: |
| Blobs | 1,071,029 files, **281 GiB** |
| Extracted text | 1,070,909 files, **10.3 GiB** (avg 10.3 KB/doc) |
| Tantivy indexes | **5.9 GiB** (4 shards) |
| SQLite metadata | **4.7 GiB** (4 × 1.1–1.3 GB) |
| Published packs | 35,786 records, 395 MB |
| Storage amplification | raw→text **27.3×**, text→index **0.57×**, raw→index **47.5×** |

**Important discrepancy:** `README.md` and `docs/specs/ROADMAP.md` still cite **~323,000 fleet
documents / 3,948 hosts**, and `ops/state.json` (2026-10-16) records 894,181 total docs. The
databases now hold **1.09M doc versions / 129,299 hosts**. The prose is stale; cite these
measured counts, and re-check them before any capacity decision.

### 5.3 Raw crawl cache — `raw-dataset/` (51 GB, gitignored)

| Shard | Blobs | Blob bytes | Crawl-log lines | Frontier URLs |
| :--- | ---: | ---: | ---: | ---: |
| 0 | 174,219 | 9.33 GB | 324,831 | 15,009,063 |
| 1 | 200,075 | 9.94 GB | 548,849 | 21,427,775 |
| 2 | 333,481 | 15.22 GB | 559,009 | 1,534,280 |
| 3 | 165,810 | 9.27 GB | 356,205 | 3,299,497 |
| unsharded | 18,948 | 4.53 GB | 22,928 | 0 |
| **total** | **873,585** | **40.8 GiB** | **1,811,822** | **41,270,615** |

Crawl outcomes over all 1.81M attempts:

| Outcome | Attempts | Share | Meaning |
| :--- | ---: | ---: | :--- |
| `ingested` | 910,452 | **50.3%** | fetched, normalized, indexed |
| `failed` | 662,874 | **36.6%** | HTTP 4xx/5xx, robots, DB-lock, index-thread errors |
| `denied` | 201,222 | **11.1%** | robots/policy/content-type refusals |
| `routed` | 36,712 | 2.0% | handed to a non-HTML route (PDF etc.) |
| `unchanged` | 562 | 0.0% | conditional refresh, 304 |

Declared response bytes over all attempts: **265.95 GB fetched for 40.8 GiB retained** — i.e.
the crawler downloads ~6.5 bytes for every byte it stores. Top failure classes are robots
disallow, HTTP 403, `database is locked`, index-thread errors, and 429 rate limits.

The frontier files describe a discovery universe of **41.3M URLs**, of which ~9.05M remain in
the fleet's live frontier rows and ~28k in the serving frontier. Frontier is the largest single
"data" object by cardinality, not by bytes (~41M URL strings ≈ 3.3 GB JSON).

### 5.4 Query corpora — `/var/lib/search-queries` (7.7 GB)

| Asset | Size / rows |
| :--- | :--- |
| Raw downloads | **7.67 GB** across 25 files |
| `keyphrases-448m.json.xz` | 4.7 GB (largest single asset) |
| ESCI products / examples | 1.1 GB / 49 MB parquet |
| `ecosystems-keywords.csv` | 746 MB |
| ORCAS clicks / queries (gz) | 315 MB / 100 MB |
| Webis suites (QSec, QInc, QSpell, QTM, SMC, NIL, TREC) | ~150 MB total |
| AOL query log | 39 MB gz, **1,525,535 rows** |
| ORCAS query log | **10,405,342 rows** |
| Unified `queries.txt` | **1,622,185 unique normalized queries** (58 MB) |
| Unified `queries.parquet` / `urls.parquet` | 55 MB / 374 MB |
| `sources.json` totals | **2,079,841 query rows**; query→URL ground truth = 0 rows |

This plane is not indexed by the engine; it is evaluation/learning material. Note that
`query_url_rows: 0`: the query→URL ground-truth join has **not** been materialized, despite the
374 MB `urls.parquet` output. That is a measurable gap, not a storage one.

### 5.5 Test/benchmark data, models, binaries

| Asset | Size | Notes |
| :--- | ---: | :--- |
| `/var/lib/search-e2e` | 1.1 GB | E2E fixture root (blobs 1,015 MB, index 41 MB). |
| `/var/lib/search-corpus` | 4.7 MB | Hermetic corpus fixture. |
| `benchmarks/corpus` | 144 KB | 30 HTML docs, 5 sources. |
| `models/fastembed` | 128 MB | One 133 MB BGE-small ONNX blob + tokenizer/config. |
| `dist/bin` | 33 MB | `agent0` 19.7 MB, `bench` 10.3 MB, `search-mcp` 4.3 MB. |
| Cargo build dirs | **167 GB** | 87 GB + 76 GB + 4 GB; disposable, not data. |

### 5.6 Data vs code

| View | Value |
| :--- | ---: |
| First-party code | 59,577 lines |
| Data retained (serving + fleet + raw + queries + e2e) | **~372.7 GB** |
| Bytes of data per line of code | **~6.3 MB/line** |
| Serving docs per production kLOC | 19,255 / 23.8 ≈ **809 docs/kLOC** |
| Fleet docs per production kLOC | 1,090,819 / 23.8 ≈ **45,833 docs/kLOC** |
| Raw HTML per extracted text byte | 16.9× (serving), 27.3× (fleet) |
| Frontier URLs per indexed doc | 2,143 (41.3M / 19.3k) |

The frontier ratio is the most decision-relevant number: **the crawler knows about ~2,100
candidate URLs for every page it has indexed.** Crawl breadth, not index size, is the current
bottleneck.

---

## 6. What SLOC does *not* measure here

SLOC is a poor proxy for this project on three axes, so the data metrics above are reported
separately:

1. **Data engineering is byte-dominated.** A 900-line indexer (`indexer`) governs 635 GB of
   Tantivy output across the fleet; a 48-line `tiers` module governs placement of 1.09M docs.
   Line count says nothing about the storage bill.
2. **Test density is uneven.** `acceptance` is 1 production line and 770 test lines; `agent0`
   is 4,042 production and 3,033 test lines. A crate with zero test-dir lines may still have a
   500-line inline suite (`scheduler`, `graph`).
3. **Much of the "code" in the tree is not ours.** `references/repos/` is 649k lines of
   vendored Tantivy/Faiss/PISA/Pyserini/Seismic/SPLADE, ~10.9× the first-party Rust codebase.
   It is study material; excluding it is mandatory for any "our code" claim.

---

## 7. Caveats and method notes

* `cloc` was run from an unpacked `.deb` because the disk was full. Counts are cloc 1.98's
  standard language mapping; `package-lock.json` is counted as JSON (3,037 lines) and is
  included only in the "full `ui/`" row.
* "Docs/specs ~9,966" = `docs/` 5,881 + `crawler/` 1,741 + `engine/` 2,191 + `deploy/` 55 +
  `benchmarks/README.md` 98. It excludes untracked working notes.
* Fleet/serving document counts use `document_version` rows (one per ingest), not `content`
  rows (deduplicated blobs); the two differ by ~19k in serving and ~20k in fleet.
* `hosts with docs` is a union across shards; per-shard sums overlap because the same host can
  appear in more than one shard.
* The crawl-log totals combine the unsharded log and the four shard logs; the unsharded log
  overlaps the serving run and should not be added to serving DB counts as unique documents.
* `raw-dataset` blob sizes are gzip-compressed `.html.gz`; serving/fleet blobs are uncompressed
  HTML. Cross-plane byte ratios therefore mix compression regimes; compare within a plane.
