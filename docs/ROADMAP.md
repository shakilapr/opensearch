# Roadmap

The 11-stage plan lives in [`../work-plan.md`](../work-plan.md) §3; each stage's frozen
contract is a `docs/STAGE<N>_SPEC.md`. This file maps that plan to what is actually in the
repo **today**, and states the gaps plainly. "Landed" means the crate/interfaces exist and
are exercised by hermetic tests in-crate; it does **not** mean a production exit gate has
been demonstrated at scale (there is no crawl corpus of 10K–1M documents in this repo).

## Stage status

| Stage | Deliverable (spec) | In the repo | Status |
| :--- | :--- | :--- | :--- |
| **0** Skeleton & spine | `core`, `metadata`, `ingest`, `frontier`, `fetcher`, `parser`, `indexer`, `api`, `agent0` | all present; `agent0 ingest/search/serve` wired; fault suite + restart/dup tests in-crate | **landed** |
| **1** 10K LLM MVP | `discovery`, `router`, `browser`, `pdf`, `sources`, `graph`, `retrieval` | all present; conditional-refresh `304` path; UI service client | **landed** (corpus scale not in-repo) |
| **2** Quality & benchmarks | `eval`, `embed`, `rerank`, `bench`, `benchmarks/` | present; 30-doc / 25-query smoke set + committed `results.json` | **landed** as a smoke set |
| **3** Freshness loop | `scheduler` | present; `agent0 refresh` tick + opt-in `SEARCH_QUERY_REFRESH` P0; live overlay | **landed** |
| **4** Agent navigation | `nav` | present: `SearchState`, action frontier, beam, policy tiers, provider adapters (fake-client tested) | **landed** |
| **5** Scale | `dedup`, `tiers`, `patterns`, migration `0004` | present | **landed** |
| **6** Dataset builder | `dataset` + `datasets/fixtures/` | present: 4 adapters, `write_pack`/`validate_pack`, `PACK_SCHEMA=1` | **landed** |
| **7** Service surfaces | `mcp` + `deploy/` + `docs/MCP.md` | present; deploy units + nginx; MCP `search` tool live | **partial** — `deep_search`/`fetch_doc` not wired (below) |
| **8** Eval & hardening | `trace`, `cache`, sources priors, `acceptance` | `trace`/`cache`/priors present; **`acceptance` crate is empty (no tests)** | **partial** |
| **9** Advanced retrieval | `ann`, `distill`, retrieval sparse | present: int8/IVF/HNSW-lite, `SparseExpander`, `LinearPolicy` (all deterministic, fixture-trained) | **landed** |
| **10** Distribution & ops | docs + scripts | `dist/` build/publish/backup scripts + the four docs here | **landed** |
| **10+** Distributed crawl & community | — | not started | **planned** |

## Honest remaining gaps

These are real, current limitations — none is a hidden TODO:

1. **No real semantic embedding weights.** BGE-small is not bundled and `feature = "bge"`
   (`crates/embed`) is off; the dense lane uses the deterministic `HashEmbedder`, which is
   a lexical bag-of-words model, **not a semantic embedding**. `benchmarks/results.json`
   prints this as `UNAVAILABLE` rather than fabricating a dense gain.
2. **No real SPLADE weights.** `SparseExpander` (`crates/retrieval/src/sparse.rs`) is a
   deterministic co-occurrence expander built from a committed fixture — a stand-in for a
   learned sparse model, not one.
3. **No trained cross-encoder.** `LexicalCrossEncoder` (deterministic lexical overlap) is
   the proxy. On the smoke set it *hurts* nDCG (0.985 → 0.941); that is an honest finding
   about the proxy, not a tuned result. See `benchmarks/README.md`.
4. **Single-box only.** One `agent0 serve` process owns the data root and the API. There is
   no scheduler cluster, no distributed index. `deploy/` ships two units that must **not**
   both bind `127.0.0.1:8080` on one host.
5. **Frontier single-writer exception.** Frontier lease acquisition deliberately keeps its
   own dedicated SQLite connection (an atomic read-modify-write) beside the batched
   metadata writer (`STAGE0_SPEC.md` §1). This is the layer that would be replaced at
   distributed scale.
6. **`doc_type` and `language` are indexed but not stored** in the Tantivy schema
   (`STAGE0_SPEC.md` §6): they support filtering, but a result cannot echo them back.
7. **`Indexer::manifest()` returns `None` before the first publish.** An unpublished index
   has no read-side state; searches then carry `degradations=["index_unpublished"]`
   (`crates/api/src/query.rs`). See `OPERATIONS.md`.
8. **MCP `deep_search` and `fetch_doc` are declared but not backed.** Only `search` is
   wired end-to-end; the other two return an honest `isError` until the HTTP API exposes
   `/search_deep` and `/read` (`crates/mcp/src/main.rs`). `docs/MCP.md` lists all three
   tools; this note is the current truth about their backends.
9. **The Stage-8 `acceptance` crate has no tests.** `crates/acceptance` exists with the
   spec'd dev-dependencies but contains only `src/lib.rs`; the 12 workspace-level
   acceptance tests in `STAGE8_SPEC.md` are **not implemented**. Per-crate integration
   tests (agent0, api, fetcher, metadata, …) do exist.
10. **No tracing subscriber is installed.** Crates are instrumented with `tracing`, but the
    binaries never initialise a subscriber, so `RUST_LOG` produces no output today.
11. **Packs are produced/consumed by library calls, not a CLI or registry.** There is no
    `search-dataset` binary and no remote pack registry (see `DISTRIBUTION.md`).

## What Stage 10+ requires

From `work-plan.md` §3 / `engine/ARCHITECTURE.md` §35: distributed crawl (scheduler cluster
→ origin queues → workers → parsers → distributed index) **only after quality is proven**,
and a public pipeline of community source packs, adapters, judgments, and privacy-
preserving telemetry. Neither is started. Treat the current system as a proven single-box
spine to grow from, not a distributed one.
