# Current architecture, model layer, and progress

**Status report — measured 2026-10-23 on the serving host.**
Every number below was read from the running system or the repository on that date; nothing is
estimated. Where a figure comes from an older document instead, the date of that measurement is
named. Where something could not be verified, it says so.

Three different things are in play and they must not be conflated:

| Layer | What it is | Where it stands |
| --- | --- | --- |
| **Deployed** | `/opt/search/bin/agent0 serve`, the live API behind `https://search.hexxica.space` | binary built **2026-10-13 08:51** |
| **Committed** | git `main`, HEAD `ecf913a` | **2026-10-18** |
| **Working tree** | uncommitted edits to the LLM config, docs and UI | **23 modified files, not committed** |

So the running service is **five days behind the repository**, and the repository itself has an
uncommitted LLM-provider migration on top of HEAD. Any statement about "the architecture" has to
say which of the three it means; this report does, per section.

---

## 1. What the system is

A single-box, agent-native search engine: it crawls and indexes real web pages, retrieves over a
local index with several lanes, fuses them, verifies that the evidence covers the query's
constraints, and optionally asks a hosted LLM to write a cited answer from the selected evidence.

End-to-end path:

```
seeds ─▶ discovery (sitemap / RSS / links) ─▶ frontier ─▶ fetcher (HTTP, browser escalation, PDF)
      ─▶ router (MIME) ─▶ parser (Readability-style HTML, PDF text) ─▶ normalized document
      ─▶ metadata DB (SQLite) + index (Tantivy: base segments ∪ mutable overlay)
      ─▶ lanes: BM25 · source-first · dense (BGE) · entity graph
      ─▶ weighted RRF fusion ─▶ coverage/evidence verification ─▶ (optional) LLM answer
      ─▶ HTTP API /search, /search_deep, /read, /health ─▶ nginx ─▶ web UI + MCP server
```

Everything is Rust: 36 crates in `crates/` (spine, crawl, retrieval, engine, model-facing, service).
The engine core (`crates/engine`) is a frozen contract from `docs/specs/ENGINE_SPEC.md`; the API adapts the
index onto it (`crates/api/src/engine.rs`).

Live proof on 2026-10-23:

- `GET https://search.hexxica.space/health` → `200`,
  `{"generation":19254,"index_manifest_id":"gen-19254","status":"ok","uptime_seconds":817252}` (9.5 days).
- `POST /search {"query":"what is retrieval augmented generation","engine":true}` → 80 matches,
  `coverage 1.0`, `mandatory_ok true`, real titles and provenance
  (`bm25`/`dense`/`source_first` ranks per result).

---

## 2. The model layer

This is the part that changed most since the original design, so it is described in full.

### 2.1 Roles — where a model is actually called

| Role | Crate | When | Calls per unit | On the serving path? | Live? |
| --- | --- | --- | --- | --- | --- |
| **Structuring** (portable pack) | `search-organize` | offline, `agent0 export [--organize]` | 1 per page, +1 escalation if output is invalid | no | yes (used to build packs) |
| **Query deconstruction** | `search-planner` | query time, background | ≤1 per distinct query | no — speculative | **no, disabled** |
| **Answer generation** | `search-answer` | query time, after selection | ≤1 per uncached prompt | **yes**, synchronous | yes |
| **Navigation / action choice** | `search-nav` + `ProviderPolicy` | query time | — | no | **no** — no HTTP transport exists |
| **Embeddings** | `search-embed` (fastembed BGE) | offline build | 1 per document | no | yes (sidecar) |
| **Evaluation judges** | — | — | — | — | **not implemented** |

There is no local inference anywhere: every LLM call is an HTTPS request to a hosted
[OI]-chat-compatible endpoint, and nothing is trained. The only model weights on disk are the
embedding model's (`bge-small-en-v1.5`, under `<data>/fastembed`), which run locally on CPU and
are never used for chat.

### 2.2 One provider chain, three consumers

All three LLM consumers share one configuration and one failover mechanism.

**Configuration precedence** (`crates/organize/src/config.rs`, `crates/answer/src/config.rs`,
`crates/planner/src/config.rs`): vendor-neutral `SEARCH_LLM_*` wins, then the consumer-specific
variable, then the legacy `CROWBOT_*`. Credentials come from `SEARCH_LLM_API_KEY`, else
`OPENROUTER_API_KEY`, else `CROWBOT_API_KEY` — environment only, never a file, never logged.

**The chain** (`ChainTransport`): an ordered list of `(provider, model, transport)` entries from
`SEARCH_LLM_MODELS`. The first entry that answers wins; a failure — connection, timeout, non-2xx,
unparseable body — moves to the next entry with **its own model id** (vendors do not share ids) and
records an `LlmAttempt { provider, model, ok, error }`. An all-fail chain reports every attempt
rather than "no answer".

**The fallback provider** (`FallbackTransport`) is a *second vendor*, tried only after the whole
chain fails, with its own base URL and model id. It is disabled entirely when no fallback key is
set. Today it is disabled: the process environment has no `CROWBOT_API_KEY` and no
`SEARCH_LLM_FALLBACK_API_KEY`.

**The attempt ledger is user-visible.** Attempts travel through the engine
(`SearchResponse.llm_attempts`), are written to the trace (`llm: <provider> <model> ok|failed (…)`),
and the UI header renders the model that answered plus the token cost
(`ui/src/components/SearchResultsView.tsx` → `AnswerPanel`). A query that needed four models looks
different from one that succeeded first time.

**Live configuration** (read from `/proc/323580/environ`, 2026-10-23):

```
SEARCH_LLM_BASE_URL = https://openrouter.ai/api/v1
SEARCH_LLM_MODEL    = nvidia/nemotron-3-ultra-550b-a55b:free
SEARCH_LLM_MODELS   = nvidia/nemotron-3-ultra-550b-a55b:free,
                      poolside/laguna-s-2.1:free,
                      inclusionai/ling-3.1-flash,
                      google/gemma-4-31b-it:free,
                      google/gemma-4-26b-a4b-it:free
SEARCH_ANSWER_ENABLED = 1        SEARCH_ANSWER_TIMEOUT_MS = 40000
SEARCH_PLANNER_ENABLED = 0       SEARCH_LLM_FALLBACK_* = unset
```

Observed on a real query: `openrouter nvidia/nemotron-3-ultra-550b-a55b:free ok`,
`tokens in=781 out=301 cached=0`, one attempt.

### 2.3 Per-role budgets

| Knob | Structuring | Planner | Answer |
| --- | --- | --- | --- |
| Timeout | 180 s | 180 s | 40 s live (default 20 s) |
| `max_tokens` | model default | model default | 1024, raised to 8192 for a reasoning id |
| Concurrency | 8 (`SEARCH_LLM_CONCURRENCY`) | 2 | 4 |
| Cache | `<data>/llm-cache`, key `(base_url, model, prompt)` | `<llm-cache>/planner` | same dir, key `sha256(model \0 prompt_version \0 prompt)` |
| Escalation | one call to a *different* model id (`SEARCH_LLM_ESCALATE_MODEL`, default `t/sonar` in the working tree) | none | none |
| Retries | per-chain entry | per-chain entry | none (a retry would double hot-path latency) |

The answer prompt is hard-capped: 8 passages, 1000 characters per passage, 24 evidence records,
8000 characters total (`crates/answer/src/prompt.rs`). The model is instructed to answer only from
the supplied evidence, to say so when the evidence is insufficient, and to cite passage indices.
`PROMPT_VERSION = "answer-v1"` is part of the cache key, so a prompt change cannot serve a stale
answer.

### 2.4 The planner is speculative, not blocking

`crates/agent0/src/planner_cache.rs` wraps the planner so a request never waits for a model:
a hit is an in-process `HashMap` lookup; a miss returns immediately (the engine searches the
verbatim query) and plans in the background; a miss may wait at most
`SEARCH_PLANNER_WAIT_US` (default 3000 µs) for a plan already in flight. A second identical query
gets the plan. The module records that the first blocking planner call took **83 s** on this host,
which is why planning was moved off the request path rather than tuned.

The planner's expansions are searched as §8 query variants — up to 3 probes plus the original
(`MAX_QUERY_VARIANTS = 4`) — each rejected if it drops a constraint, and fused per family so
rewrites cannot outvote the query the user typed.

### 2.5 What is *not* a model, but is often described as one

| Component | Reality |
| --- | --- |
| Cross-encoder reranking | `search-rerank` has a `LexicalCrossEncoder` proxy (token overlap). It is **not linked into `search-api` or `agent0`** — no reranking happens on the serving path. |
| Sparse / SPLADE retrieval | `SparseExpander` is a deterministic co-occurrence expander built from a committed fixture. Not a learned model, and not on the serving path. |
| Learned navigation policy | `search-distill::LinearPolicy` is fixture-trained and not on the serving path. |
| ANN index | `search-ann` implements int8 + IVF/HNSW-lite, but the served dense lane is brute-force cosine. `search-ann` is not a dependency of `agent0`. |
| LLM-as-judge evaluation | Does not exist. `scripts/engine-eval.sh` is expectation-based (8 queries). Its only artifact (`ops/engine-eval-2026-10-12.json`) has `answer_chars: 0` for all 8 — the answerer was off, so **answer quality has never been measured**. |

---

## 3. The retrieval and ranking model (as served)

Lanes over the Tantivy index (base segments ∪ mutable overlay):

| Lane | Implementation | Weight in fusion |
| --- | --- | --- |
| BM25 | Tantivy lexical, global recall path, always present | **1.0** |
| source-first | same index, source-routed accelerator for conceptual queries | **0.8** |
| dense | `DenseLane` over a `doc_id → vector` sidecar, BGE-small-en-v1.5, 384-d, brute-force cosine | **0.0** |
| entity graph | `GraphLaneSearcher` over personalised PageRank on co-occurrence edges, joined via the entity→document sidecar | **0.0** |

Fusion is weighted reciprocal rank fusion, `score(d) = Σ w_l / (60 + rank_l(d))`
(`crates/engine/src/fuse.rs`; the coordinator always passes `FusionPolicy::default()`).

**This is worth stating plainly: the dense and graph lanes are searched, their ranks are reported
per result (`lane_ranks: {bm25, dense, graph, source_first}`), and they contribute zero to the
score.** The default policy defines weights only for BM25 and source-first, and an unknown lane
contributes nothing. The arithmetic is visible in a live response: a document ranked 1 in both
BM25 and source-first scores `1.0/61 + 0.8/61 = 0.0295082`, which is exactly the
`0.02950819581747055` the API returned. Semantic retrieval therefore cannot reorder results today;
it can only annotate them.

The §10 cascade (`run_cascade`) names five stages, but the "feature ranker", "cross-encoder" and
"evidence verification" stages are **caps** — each takes the first *n* documents. Ordering is RRF
alone; diversification is truncation to `final_k`.

Mode budgets (`crates/engine/src/plan.rs`, `budget.rs`):

| Mode | Deadline | Rounds | Live fetches | Selector calls | Rerank cap |
| --- | --- | --- | --- | --- | --- |
| fast | 200 ms | 1 | 0 | 0 | 30 |
| auto (default) | 3 s | 2 | 8 | 1 | 100 |
| deep | 20 s | 5 | 30 | 4 | 300 |

Evidence coverage is token-based, not entity-aware: a page containing a unit's tokens can satisfy
it. The trace names the supporting document so the verdict is auditable, and the answerer is the
honesty gate.

---

## 4. Corpus, index, and packs — measured today

### Serving index (`/var/lib/search`, generation 19254, manifest published 2026-10-09T20:37)

| Quantity | Value |
| --- | --- |
| documents (manifest `doc_count`) | **19,255** |
| distinct domains | 225 |
| source profiles | 249 |
| content rows | 18,947 |
| entities / graph edges / works | 14,659 / 130,359 / 211 |
| dense sidecar | 384-d, `vectors.bin` 30.7 MB, 19,255 vectors |
| LLM cache entries | **106,142** |

### Fleet (`/var/lib/search-fleet`, four host-disjoint shards, 307 GB)

| Shard | resources | published index docs | domains (rows) | structured pack records |
| --- | --- | --- | --- | --- |
| 0 | 180,393 | 131,014 | 56,525 | 20,253 |
| 1 | 367,045 | 122,012 | 37,992 | 811 |
| 2 | 370,897 | 124,639 | 31,062 | 11,311 |
| 3 | 171,849 | 123,496 | 43,405 | 3,411 |
| **total** | **1,090,184** | **501,161** | 168,984 | **45,786** |

Domain counts are per-shard distinct and overlap across shards; they are not a deduplicated host
count. "Resources" includes fetched-but-not-published and tombstoned rows, which is why it exceeds
published documents.

The fleet is **not merged into the serving index**. The site serves 19,255 documents; 501,161
published documents sit in four shards with no graph, no entities (`entity = 0` on every shard) and
no dense sidecar. The `deploy-fleet-corpus` work item is still `todo`.

### Crawl

**No crawler is running.** `pgrep -f "agent0 crawl"` is empty and the shard logs are gone.
`ops/state.json` last recorded 894,181 ingested documents at 2026-10-16T04:44:39. The crawl has not
run since; the disk is now full, so restarting it is blocked on space (§7).

---

## 5. Progress against the plan

Stage status (from `docs/specs/ROADMAP.md` and `docs/specs/STAGE_STATUS.md`, both last edited 2026-10-13…16):

| Stage | State |
| --- | --- |
| 0 Skeleton & spine | done, nothing outstanding |
| 1 10K-document MVP | landed except **source breadth in the served index** |
| 2 Quality & benchmarks | landed as a **30-document, 50-query fixture**, not a corpus benchmark |
| 3 Freshness loop | landed; query-time refresh disabled while serving read-only |
| 4 Agent navigation | landed as a deterministic loop; model-driven action selection has no transport |
| 5 Scale to 100K–1M | components landed; tiers not enforced on the live store, patterns not wired into priority, unmeasured past ~12K |
| 6 Dataset bootstrapping | adapters landed, fixture-tested only; no generated pack has been enqueued |
| 7 Service surfaces | API + MCP + nginx landed; MCP needs a running API by design |
| 8 Eval & hardening | hermetic acceptance + trace landed; no per-request access logs |
| 9 Advanced retrieval | proxies only (sparse, cross-encoder, linear policy), none serving |
| 10 Distribution & ops | single box; no distributed crawl |

Work ledger (`ops/plan.json`): **11 done, 2 in progress, 2 todo, 1 blocked** of 16 items.

What is genuinely new since the earlier reports:

- **Index model** (2026-10-18, undeployed): generations allocated durably and published in two
  phases; counts published from segment metadata instead of a corpus scan; bulk import batches into
  base rather than the overlay; stored `entities` carried through export; writer arena made
  configurable (`SEARCH_INDEX_WRITER_MB`). The commit itself records that write amplification
  (~7×, merge churn) is **not** fixed.
- **LLM layer** (2026-10-12/13): one provider chain for the engine, per-attempt reporting, token
  cost in the UI, explicit non-streamed completions, `OPENROUTER_API_KEY` accepted.
- **Portable packs**: incremental, resumable export (`PackWriter`, a shard every 500 records), which
  is what let the four-shard structuring finish at 45,786 records instead of writing nothing.

---

## 6. Differences from the original architecture

The original design is `docs/specs/AGENT_NATIVE_SEARCH_ENGINE.md` (2,161 lines) for the engine and
`docs/ARCHITECTURE.md` (2,372 lines) for the universal crawler. These are the places where what
runs today differs from what those documents describe.

### 6.1 The model layer

| Original | Now | Why it changed |
| --- | --- | --- |
| "An LLM is optional" and guides only difficult searches; a dedicated small navigation model is a later stage (§31), trained from traces | Three LLM roles exist: offline structuring, optional query deconstruction, optional answer generation. Navigation is still the deterministic `RuleSelector`. No small model is trained; `LinearPolicy` is a fixture proxy | The engine's action frontier had no HTTP transport, so the model never entered the loop; the LLM entered where it was cheap and cacheable instead |
| One provider (`crowbot`, `deepseek-v4.1-flash`) | A chain of five free models on OpenRouter, first answer wins, optional second-vendor fallback | Free-tier models fail often; a chain plus an attempt ledger turned "no answer" into an auditable sequence |
| Query expansions planned inside the plan (§8) | Planning is opt-in, **disabled in production**, and architecturally *speculative*: the request never waits for it (3 ms ceiling) | A blocking planner call recorded at 83 s; the contract allows a planner to return nothing without changing behaviour |
| Local inference contemplated (§1) | No local inference at all; hosted HTTP only | No GPU on the host (virtual VGA adapter only), 16 vCPU / 62 GB RAM, committed to crawl and index |
| Answer generation §16/§22.1 as a post-selection stage | Implemented exactly as a post-selection stage, but its cache write is on the critical path: **ENOSPC discards a paid-for answer** | The cache was treated as required rather than best-effort |

### 6.2 Ranking

| Original | Now |
| --- | --- |
| Dense and sparse lanes contribute to a fused, feature-ranked, cross-encoded ranking cascade | Dense and graph lanes are computed and reported but carry **weight 0.0**; the cascade's ranker stages are truncation. Served ordering is BM25 (1.0) + source-first (0.8) RRF |
| Feature ranker + cross-encoder as cascade stages | `search-rerank` exists, is tested, and is **not a dependency** of `search-api`/`agent0` |

This is the largest functional gap between the documented architecture and the running system.

### 6.3 Crawler

`docs/ARCHITECTURE.md` specifies a universal crawler with OAI-PMH, GROBID, archive, Git, forum,
image, video and dataset adapters. Implemented: HTTP with conditional requests, robots policy,
adaptive per-origin politeness, sitemap and RSS/Atom discovery, MIME routing, Readability-style HTML,
PDF text extraction, Chromium escalation for JS-only pages, MinHash near-duplicate detection, URL
pattern learning. Adapters for Common Crawl, OpenAlex, Wikidata and OAI-PMH exist but are
fixture-tested only. No GROBID, no archive/Git/forum adapters, no OCR queue.

### 6.4 Scale and shape

| Original | Now |
| --- | --- |
| Grow from one box to a coordinator with scheduler, origin queues, stateless workers, distributed index (Stage 10+) | One box, one serving process, four host-disjoint crawl shards, no merge, no coordinator |
| Corpus breadth "hundreds to thousands of sources" served | 225 domains served; 501,161 documents across ~169K domain rows sit unmerged in the fleet |

### 6.5 Documentation drift (specific)

- `docs/specs/ANSWER.md` still says the deployment runs **Sonar first and crowbot's deepseek last**. It
  does not: it runs OpenRouter `nvidia/nemotron-3-ultra-550b-a55b:free` first with **no fallback
  provider configured**.
- `docs/specs/ANSWER.md` says the key requirement is `CROWBOT_API_KEY`; the code accepts
  `SEARCH_LLM_API_KEY`, `OPENROUTER_API_KEY` or `CROWBOT_API_KEY`.
- The provider rename (kitani → crowbot → OpenRouter) exists in the working tree as **23
  uncommitted files**; HEAD still names the older providers in places.

---

## 7. Current defects and blockers

1. **Disk is 100% full.** `/dev/vda1`: 709 GB total, 684 GB used, **0 bytes available**. Consumers:
   `/var/lib` 322 GB (fleet 307 GB), `/root/projects/search` 217 GB (build targets).
   Consequence, observed live: the answer path asks the model, the model answers
   (781 in / 301 out, billed), and then `write_cache_atomic(...)?` fails with
   `io error: No space left on device` — the engine logs `answer failed: …` and returns
   `answer: null`. **Every uncached answer is currently paid for and discarded.** The cache write
   should be best-effort (log and return the answer), and space must be freed.
2. **The deployment is five days behind.** The binary predates the durable two-phase generation
   publish, metadata-based counts, base-segment bulk import and the configurable writer arena. It
   was also built two minutes *before* `322691c` (explicit `stream: false`), so it corresponds to no
   commit exactly. It cannot be rebuilt right now: the disk is full.
3. **The crawl is stopped** and has not run since 2026-10-16. The fleet has not grown since, and it
   cannot be restarted while the disk is full.
4. **The planner is off** (`SEARCH_PLANNER_ENABLED=0`), so no query deconstruction runs in
   production, and planner-on vs planner-off quality has never been measured.
5. **Dense and graph lanes have zero fusion weight**, so the semantic lane cannot change ranking —
   the one thing it was built for.
6. **The fleet is not merged**: 19,255 served documents against 501,161 published in the shards.
7. **No answer-quality measurement exists.** The only engine evaluation ran with the answerer off.
8. **23 uncommitted files**, including the LLM provider migration and its documentation.

---

## 8. What is verified, and what is not

Verified on 2026-10-23 by direct observation: the service is up and answering (`/health`, a real
`/search` with `engine: true` returning 80 matches, coverage 1.0, real titles and provenance); the
model chain is OpenRouter's five-model list and the first model answered; the answer was lost to
ENOSPC; the index generation is 19254 with 19,255 documents; the fleet holds 1,090,184 resources
and 501,161 published documents across four shards; the LLM cache holds 106,142 entries; no crawler
process is running; the disk has zero bytes free; the deployed binary predates HEAD.

Not verified, and therefore not claimed: that the current test suite passes (it cannot be built with
no free disk), any answer-quality or ranking-quality figure on the real corpus, any measurement
above ~12K documents, and any statement about distributed behaviour.
