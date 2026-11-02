> **Historical snapshot (2026-10-27).** Kept because it records the state before the semantic
> layer was built. Do not use it for the current design or current numbers:
> **`docs/ARCHITECTURE.md`** is normative (2026-11-01) and `docs/STATUS.md` carries
> the measurements.

# Current architecture — what the system actually is

Measured on the host on **2026-10-27**, against the running service, the live databases and the
working tree at commit `dbb5a22`. Every number below was read from the system during this
analysis, not copied from an earlier document. Where a number is a *label-derived* metric (nDCG on
corpus-derived queries) that is said so.

This supersedes the "where we are" tables in `PLAN-v2.md` and `docs/legacy/analysis-next-move.md`. It describes
what runs; `docs/legacy/architecture-update-v2.md` describes what is intended.

---

## 1. One-paragraph summary

Hexxica is a Rust, agent-facing search engine: a crawler that fills a frontier from sitemaps and
follows links, an ingestion path that parses pages into metadata + a Tantivy index + derived
artifacts, and an HTTP/MCP query surface with two modes — a fast lexical `/search` and an
`engine:true` mode that runs a budgeted retrieve→fuse→rerank→answer loop. It is **not** the semantic
"brain" the target architecture describes: the concept tables exist and are **empty**, the dense
lane is computed but was weighted **0.0** until today (now 0.5), and the served corpus is **19,255
documents of 1.1 M that have been crawled**. The work of the last two sessions has been to make the
parts that exist honest — an answer that is no longer thrown away, two lexical lanes that are no
longer literally the same call, a crawl that stops before it fills the disk, a caller for lease
recovery, a dense lane that now contributes, and a federated searcher that can put the four shards
behind one query (wired, not yet enabled).

---

## 2. Measured state

### 2.1 Corpus and stores

| Fact | Value | Where it comes from |
| --- | --- | --- |
| **Served corpus** | **5 shards federated, ~1.05 M documents, gen 211k** | `/health` -> `federated-5x211246` |
| Primary root | gen 19254, 19,255 resources, 225 domains | `resource`/`domain` counts |
| Federated latency | **124–145 ms** for 5 shards (was 71–92 ms for 19K docs) | live `POST /search` |
| Fleet (4 shards) | 182,361 · 370,454 · 372,812 · 176,074 resources | per-shard `meta.sqlite` |
| Fleet disk | **310 GB** (56–99 GB per shard) | `du` |
| Serving data root | 6.1 GB (4.5 GB blobs, 315 MB Tantivy, 309 MB text, 430 MB LLM cache) | `du` |
| Roots total on `/` | 601 GB used, **73 GB free (90 %)** | `df` |
| Schema version | serving **v6 / 37 tables**; fleet shards **v4 / 23 tables** | `schema_migration` |

The fleet shards are **two migrations behind the serving root** (v4 vs v6: they lack the concept
graph and the identity layer). A shard opened by today's binary is migrated on open, which is a
write — see §7.3.

### 2.2 Metadata tables on the serving root

| Table | Rows | Meaning |
| --- | --- | --- |
| `resource` | 19,255 | one per fetchable URL |
| `content` | 18,947 | one per extracted text revision |
| `work` | 211 | logical works (a paper at several URLs) |
| `sitemap` | 482 | sitemap documents |
| `sitemap_entry` | **0** | discovered URLs are not persisted here |
| `domain` | 225 | internet hostnames |
| `source_profile` | 249 | per-source crawl/quality profile |
| `website`, `website_host` | 0 | the website layer is defined but unused |
| `entity` | 14,659 | entities (mostly CamelCase tokens, e.g. `A.B`, `AAAI`) |
| `concept`, `doc_concept`, `passage`, `passage_concept`, `concept_relation`, `source_concept` | **0** | **the semantic brain: schema only** |
| `model_registry` | **0** | model versions are not registered |
| `web_link`, `graph_edge` | 0 | the link graph was never built |
| `frontier_entry` | 692 `FETCHING` on serving; **84k–119k `FETCHING`** per fleet shard | zombie leases |

**The single most important fact in this document:** `concept`, `doc_concept` and `passage` exist
in the schema (migrations 0005/0006) and hold **zero rows**. The weighted multi-topic brain the
architecture calls for has its tables and none of its data.

### 2.3 Derived artifacts (serving root)

| Artifact | Size | Content |
| --- | --- | --- |
| `tantivy/base` + `overlay` | 315 MB | BM25 index over title/body/source/published_at |
| `vectors.bin` | 30.7 MB | **384-d, unit-norm** vectors for **19,255** docs, keyed by `resource_id`, magic `ANSEVEC1` |
| `graph/entities.bin` | 30.8 MB | 14,659 entities (entity lane; fleet shards have **0**) |
| `text/` | 309 MB | extracted text per content revision |
| `blobs/` | 4.5 GB | raw HTML (the thing the "never keep downloads" rule is about) |
| `llm-cache/` | 430 MB, 106,147 entries | answer cache, keyed by prompt |

The vector model is **BGE-small-en-v1.5** (`BAAI/bge-small-en-v1.5`, 384-d), embedded through ONNX
Runtime under the `bge` feature. It is **not** MRL-trained, so dimension truncation is not valid.

### 2.4 Retrieval quality (measured, label-limited)

600 corpus-derived queries (title → its own document) against the serving index, `engine:true`,
`--lanes all`:

| Config | nDCG@10 | MRR | recall@10 | p50 ms | p95 ms |
| --- | --- | --- | --- | --- | --- |
| dense weight 0.0 (shipped until today) | 0.4837 | 0.4777 | 0.5017 | 468 | 1011 |
| dense 0.5 | 0.4869 | 0.4826 | 0.5000 | 470 | 1026 |
| dense 0.8 | 0.4873 | 0.4831 | 0.5000 | 535 | 1274 |
| dense 1.2 | 0.4881 | 0.4837 | 0.5017 | 513 | 1214 |
| dense 0.0, after the lane split | 0.4837 | 0.4777 | 0.5017 | 471 | 1010 |
| dense 0.5, after the lane split | 0.4869 | 0.4826 | 0.5000 | 490 | 1083 |

These labels are *corpus-derived*, not human judgements: a title-as-query can be answered lexically,
so the ceiling on this set is low and the dense gain is small but monotone. nDCG +0.9 % relative at
w=1.2 costs +45 ms p50; w=0.5 captures most of the gain for +2 ms. **This is the evidence for the
weight decision, not a claim that semantic search is now solved.**

**Deployed:** `SEARCH_FUSION_DENSE=0.5` is live since 2026-10-27 (drop-in `fusion.conf`). The effect
is visible on the user's own example: for `"chemical reactions that affect romance"`, *Chemistry*
carried `dense: 1` and moved from rank 3 to **rank 1**. The dense lane is no longer computed and
discarded.

**The limit that no weight can fix:** the same query family `"how oxytocin builds attachment"`
returns *TypeScript Nightly Builds* and *Docker Builds*, because the corpus contains **no document
about oxytocin** (measured earlier: 0 title matches for `oxytocin`, `romantic love`,
`phenylethylamine`). Re-weighting cannot rank a document that was never crawled.

---

## 3. Component architecture

36 crates, **57,596 lines of Rust** under `crates/`. By role and size:

### 3.1 Contracts and identity
- **`core`** (923) — `Config`, `AppError`/`AppResult`, the frozen request/response contract,
  `IndexManifest`, filters, `Signal`, `Provenance`.
- **`metadata`** (3,121) — the SQLite identity layer and migrations 0001–0006. Single-writer
  (`MetadataWriter`), read-only readers (`MetadataReader`). Entities: `domain → website → source →
  sitemap → resource → content → work`, plus `passage`, `concept`, `doc_concept`.

### 3.2 Crawl
- **`frontier`** (1,066) — the `frontier_entry` queue with epochs and leases
  (`push/lease/complete/fail/release_expired`). **`release_expired` had no caller until this
  session** (§8).
- **`discovery`** (894), **`scheduler`** (1,294) — sitemap seeding, link extraction, per-origin
  politeness, due-source selection.
- **`fetcher`** (1,985), **`browser`** (710) — HTTP with conditional-request validators; a JS
  escalation driver for pages that render to no text.
- **`sources`** (1,037), **`router`** (353), **`tiers`** (181) — source profiles, routing rules.

### 3.3 Ingest and parse
- **`parser`** (366), **`pdf`** (361), **`patterns`** (294) — HTML→text, PDF extraction, boilerplate
  patterns.
- **`ingest`** (731), **`dedup`** (597) — normalisation, content hashing, near-duplicate detection.

### 3.4 Index and semantics
- **`indexer`** (1,676) — Tantivy 0.22.1, two indices (`base` immutable + `overlay` mutable),
  frozen schema, generations, `all_docs`, read-only open (no writer, no lock).
- **`embed`** (1,075) — the ONNX BGE embedder and the `ANSEVEC1` vector store.
- **`ann`** (750) — approximate nearest neighbour (built; not on the serving query path).
- **`graph`** (1,918) — entity graph, `entities.bin`, the §9.4 expansion lane.

### 3.5 Retrieval and ranking
- **`retrieval`** (1,366) — the four **lanes**: `Bm25`, `SourceFirst`, `Dense`, `Graph`; the
  `Candidate` type with per-lane provenance.
- **`engine`** (5,014) — the budgeted loop: plan → retrieve rounds → budget ledger → **fusion by
  reciprocal rank** (`FusionPolicy`, c=60, `variant_family_cap`) → finalize. Modes and deadlines:
  **Fast 200 ms, Auto 3,000 ms, Deep 20,000 ms**; the deadline is split retrieve 45 % / fuse 10 % /
  finalize 20 % / rerank 25 %. Stop reasons: `Deadline`, `BudgetExhausted`, `SufficientCoverage`,
  `LimitReached`, `NoAdmissibleAction`, `Cancelled`, `Plateau`.
- **`rerank`** (820), **`nav`** (1,592), **`planner`** (591), **`organize`** (2,150) — cross-encoder
  rerank, navigation/anchors, LLM query planning (off the request path, speculative cache),
  answer organisation.

### 3.6 Answer
- **`answer`** (1,796) — evidence selection, LLM call, **atomic cache write** (§8).
- **`cache`** (295), **`distill`** (453) — the on-disk answer cache and distillation helpers.

### 3.7 Surfaces
- **`api`** (5,698) — axum HTTP: `POST /search`, `POST /search_deep`, `GET /read`, `GET /health`;
  the API's `Searcher` trait + `IndexerSearcher` adapter; `SearcherLane` bridging the engine onto
  that trait; `run_engine_search`; **`FederatedSearcher`** (§8).
- **`mcp`** (1,226) — the Model Context Protocol server over the same searcher.
- **`agent0`** (11,888, the largest crate) — the binary: `crawl`, `serve`, `index`, `embed`,
  `frontier recover`, `eval run`, export/import, `refresh`.
- **`bench`** (1,223), **`eval`** (714), **`acceptance`** (1,051), **`dataset`** (2,068),
  **`trace`** (319) — harnesses and diagnostics.

---

## 4. The two request paths (this is the part that differs most from the design)

### 4.1 `/search` — lexical, ~71–92 ms
`execute(searcher, req)` in `api::query`: analyze the text (`site:`/`after:`/`before:` folded into
filters), one `Searcher::search` for `offset+limit` candidates, hydrate/filter/sort/trim, assemble
the frozen response. The `Searcher` is `SourceFirstSearcher`, which runs a **global BM25 scan** and
then re-ranks source-first over the top matching sources (≤8).

### 4.2 `/search` with `engine:true` — the budgeted loop
`run_engine_search` wires the same `Searcher` into **two** `SearcherLane`s and any injected extra
lanes (dense over `vectors.bin`, graph over `entities.bin`), runs the engine, then hydrates the
engine's doc ids from the lanes' retained `IndexHit`s. A live trace of
`"chemical reactions that affect romance"`:

```
plan: mode=Auto lanes=[Bm25] top_k=20 variants=1 rerank=33 refresh=8
round 1: launch 4 searcher(s) over 1 probe(s)
evidence: no unit supported
round 2: action Expand {U1}
cascade stage sizes: [80, 33, 33, 33, 5]
stop: refined to LimitReached
answer: 90 char(s); llm tokens in=1388 out=74
provenance: graph expansion reached 0 document(s)
```

Note `graph expansion reached 0` — the graph lane is enabled but contributes nothing, because
`graph_edge` is empty and the entity set is title-token noise.

### 4.3 What the lanes actually are (the lie this session fixed — and what it revealed)
Both `SearcherLane`s wrapped the **same** `Searcher`, and `SourceFirstSearcher::search` re-ranks
internally. So the `bm25` lane and the `source_first` lane returned **identical lists** — visible in
earlier traces where every result read `{'bm25': 1, 'source_first': 1}`.

Fixed by `Searcher::search_lexical`: the `bm25` lane now takes the raw scan, the `source_first` lane
the re-ranked output, and the cache key includes which call produced it (`index_scans` went 1 → 2).

**But measured live after the fix, the two lanes still agree on 15 of 15 top-5 ranks across three
queries, and the 600-query eval is unchanged to four decimals (0.4837).** The reason is not that the
fix failed — it is that `SourceFirstSearcher`'s re-ranking preserves BM25 order in practice, so the
engine's second lexical lane still carries no new information. The lanes are now honestly *two
functions*; they are still *one signal*. Making the second lexical lane useful needs a genuinely
different ranking (a source prior that reorders, or a learned re-ranker), not a second call to the
same scan.

---

## 5. Identity and storage model

### 5.1 Identifiers
`DomainID` (hostname) · `SourceID` (logical collection) · `ResourceID` (fetchable URL) · `WorkID`
(logical work) · `ContentID` (hash of a text revision) · `PassageID` · `ConceptID` · `SitemapID`.
All exist in the schema. `website`/`website_host` (the layer above sources) is defined and empty;
`work` has 211 rows, so work ≠ resource is already real but barely populated.

### 5.2 What is stored, and the "no downloads" rule
The governing constraint from the user: **keep the index model that points at websites, not the
downloaded articles.** The honest reading, which the code supports:

| Keep (derived) | Delete (source bytes) |
| --- | --- |
| URL, title, headings, ~500-char excerpt, `doc_type`, `language` | full extracted text (`text/`) |
| dense vectors (`vectors.bin`) | raw HTML (`blobs/`) |
| BM25 index (`tantivy/`) | |
| link graph | |

Today the system keeps **both**: 4.5 GB of raw HTML and 309 MB of text sit beside the index. The
deletion pass is planned (`docs/legacy/index-upgrade-plan.md`) and **not done** — the shards are being crawled
right now, so `raw-dataset/` (49 GB) is a live output directory.

---

## 5b. Throughput — the crawl was I/O-bound on its own bookkeeping

Measured 2026-10-28, before and after the fixes below: **106 attempts/min and ~30 docs/h →
~2,000 attempts/min and ~10,000 docs/h (14.6x)**. None of it was the network: a probe of 40 real
seed URLs showed p50 0.3 s / mean 0.8 s, i.e. concurrency 32 alone supports ~2,300/min.

| Cause | Measured | Fix |
| --- | --- | --- |
| `frontier.json` rewritten every 2 s | 213 MB JSON, 0.7 s to serialise, per shard | append delta + snapshot every 150 flushes (`30e559d`, `ebb5c65`) |
| `frontier.push` one autocommit INSERT per URL, 512 per seed | ~80% of process CPU in `sqlite3VdbeExec` | `push_many`, one transaction (`b7f2d07`) |
| lease query could not use its index (`status IN (...)`) | **522 ms per lease call** | partial index + ANALYZE, **0.4 ms** (`2ed2a71`) |
| `dispatch` swept 9,215 origins × 256 in flight | 2.36M map lookups per loop turn | ready deque (`e499b09`) |
| per-seed `SELECT url FROM feed LIMIT 8`, no predicate | same 8 feeds refetched for every seed | one host→url map at startup (`389b527`) |
| duplicate raw spill (data_dir **and** out_dir) | 295 GB + 45 GB, zero name overlap | opt-in `--keep-raw-blobs` (`2d17cd2`) |
| `database is locked` | **1,416 failures/hour** — documents silently dropped | `BEGIN IMMEDIATE` + busy retry (`c06d460`) |
| embedding batch size 1 | 31.5 docs/s vs 98.1 batched | `embed_batch` override (`08e7c67`) |

## 5c. Federation

`SEARCH_FEDERATE_DIRS` makes `serve` answer from all four fleet shards. A shard is opened as **its
Tantivy index only** (`IndexerConfig::read_only` + `IndexerSearcher`) — a full
`Pipeline::open_read_only` builds a `MetadataWriter` (migrations, i.e. writes) and the crawler
holds the writer's lockfile, so it fails on `PermissionDenied` then `Failed to acquire Lockfile`.
The unit runs as `root` with `ReadWritePaths` covering both roots because the crawlers recreate
`.managed.json`/`meta.json` 0600 root-only on every commit.

## 6. Operational surface

- Service: `systemctl search-api` → `/opt/search/bin/agent0 serve`, bound `127.0.0.1:8090`,
  health `{gen 19254, status ok}`.
- Crawl: **three shard crawlers running** (shards 0, 1 and 3 at ~105 % CPU each, 32-way
  concurrency, 250 ms per-origin interval), `--max-docs 250000`; **shard 2 stopped** at 07:41 with
  no error in its log, which is itself a gap — nothing reports that a shard is no longer crawling.
  Over 17 h they ingested 1,010 / 2,444 / 1,354 / 2,581 documents per shard — most work is discovery
  and refusals, not ingestion.
- Env knobs that matter: `SEARCH_DATA_DIR`, `SEARCH_READ_ONLY`, `SEARCH_MIN_FREE_GB`,
  `SEARCH_FEDERATE_DIRS`, `SEARCH_FUSION_{BM25,SOURCE_FIRST,DENSE,GRAPH,C,FAMILY_CAP}`, `SEARCH_BROWSER`,
  `SEARCH_MODEL_CACHE`.
- Answer path: OpenRouter `nvidia/nemotron-3-ultra-550b-a55b:free`, cached on disk.

---

## 7. Differences from the original architecture

### 7.1 Designed and working
Identity schema (0001–0006), Tantivy base/overlay with generations, read-only open beside a writer,
budgeted engine loop with a stop-reason taxonomy, four lane types, dense vectors, entity graph
artifact, LLM planner off the request path, MCP surface, answer cache, federation layer (new).

### 7.2 Designed and **not** working (the gap)
| Designed | Reality |
| --- | --- |
| Weighted multi-topic brain (`doc_concept`) | tables exist, **0 rows** |
| Passage-level semantics (`passage`, `passage_concept`) | **0 rows** |
| Typed concept graph (`concept_relation`) | **0 rows** |
| Link graph (`web_link`, `graph_edge`) | **0 rows** |
| Model versioning (`model_registry`) | **0 rows** |
| Sitemap-persisted discovery (`sitemap_entry`) | **0 rows** |
| Website layer (`website`, `website_host`) | **0 rows** |
| Sparse-feature index (`feature`, `doc_feature`) | **tables do not exist** |
| Dense lane weighted | **0.5** in production since 2026-10-27 (was 0.0) — measured +0.0032 nDCG for +19 ms |
| Graph lane | enabled, contributes **0** documents |
| Federation of the four shards | searcher exists and is wired; **not enabled in production** (`SEARCH_FEDERATE_DIRS` unset) |

### 7.3 Where the implementation diverged from the design
1. **The identity layer is ahead of its data.** Six migrations of schema, almost none of it filled.
   The design assumed the crawl would populate `concept`/`passage`/`web_link` as it ran; it does not
   — ingestion writes `resource`/`content` and stops.
2. **The fleet fell behind the schema.** Shards are v4, the serving root v6. Opening a shard with
   today's binary migrates it (a write), so federation must either migrate-on-open deliberately or
   read the older schema. This is the concrete reason federation is wired but not switched on.
3. **"Two lexical lanes" was one lane twice.** The design's BM25 + source-first pair was
   implemented as one searcher called twice (§4.3).
4. **The answer was on the hot path and then discarded.** `engine:true` called the LLM
   synchronously (5.4–46.7 s) and, on a full disk, threw the paid answer away (`ENOSPC` in the cache
   write).
5. **Storage only ever grew.** Raw HTML was never pruned; the index is 6 GB against 284 GB of raw
   bytes.

---

## 8. Progress — what this session actually changed

All committed, all tested, deployed where it affects the live service.

| Commit | Change | Verified by |
| --- | --- | --- |
| `7180806` | **Answer cache write is best-effort**: a full disk no longer discards a paid answer | live: 431-char answer, repeat served from cache in **301 ms / 0 tokens** |
| `ca0a156` | **Crawl disk guard**: `statvfs` in the loop, stops at `SEARCH_MIN_FREE_GB` (5 GiB) with `stopped_for_disk` | 3 tests incl. an impossible floor stopping before the first request |
| `e0ef193` | **`agent0 frontier recover`**: the caller `release_expired` never had | measured on a shard copy: **82,553 zombie leases** reclaimable; 3 tests |
| `7014c25` | **`bm25` lane is a real lexical scan** (`Searcher::search_lexical`) + fusion weights from env | test: raw vs re-ranked orders must give different `lane_ranks` |
| `135e491` | **`FederatedSearcher`**: N shards fused by reciprocal rank, URL-deduped, degrade-not-fail | 7 tests |
| `dbb5a22` | **`SEARCH_FEDERATE_DIRS`** wiring for `serve`, read-only shard open, never create a shard root | 5 tests |
| (deploy) | **`SEARCH_FUSION_DENSE=0.5` live** | Chemistry moved rank 3 → **1** on the chemistry/romance query; 600-query eval +0.0032 nDCG |

Also this session: `target-browser` removed (4 GB), the deployed binary brought to HEAD with a
rollback copy kept, and `PLAN-v2.md` Phase 0 annotated with what completed and what could not.

### 8.1 Still open, in order
1. **Federation not switched on.** The searcher exists and 4 shards hold 1.1 M resources; they are
   schema v4 and would be migrated on open. Decide migrate-on-open, then set `SEARCH_FEDERATE_DIRS`.
2. **The second lexical lane is still redundant** — the split made the two lanes two functions, but
   they agree on 15/15 top-5 ranks (§4.3). A source prior that actually reorders, or a learned
   re-ranker, is what would make it a second signal.
3. **The brain has no data.** `doc_concept` needs a tagging pass over existing text before a single
   semantic lane can mean anything. This is the highest-value unstarted item.
4. **The corpus lacks the documents the user's example needs** — no page about oxytocin or romantic
   attachment exists in either the served or the crawled corpus. Weights cannot fix coverage.
5. **The 47 GB `raw-dataset/` duplicate** waits for the crawl to stop (verified 100 % duplicate on
   all four shards).
6. **Zombie leases (82.5k/shard)** wait for the same moment, so a slow in-flight fetch is not
   double-dispatched.
7. **Storage rule not enforced**: raw HTML and text still accumulate beside the index.
8. **Shard 2's crawler stopped silently** — no supervisor notices a shard that stopped crawling.

---

## 9. Honest caveats

- **The retrieval metrics use corpus-derived labels**, not human relevance judgements; they measure
  "can the title find its own document", which flatters lexical search. Treat deltas, not absolutes.
- **p50 ~470 ms for `engine:true`** is with three crawlers saturating the disk; the 200 ms Fast
  budget is not met at 19K documents and will not be met by adding shards.
- **Federation has never served a real query.** It is unit-tested against stubs; the four shards
  have not been opened by `serve`.
- **The graph lane's contribution is zero** in every live trace measured; reporting it as an active
  signal would be false. It is enabled, costs time, and returns no documents.
- **The dense lane's gain is small against these labels** (+0.0032 nDCG). It is deployed because it
  is the semantic signal the design calls for and the effect on the user's example query was
  visible (Chemistry 3 → 1), not because the 600-query set can prove a large win.
- **The lane split is mechanically verified but behaviourally invisible** so far (§4.3). Do not
  count it as a ranking improvement.
