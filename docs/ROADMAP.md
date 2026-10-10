# Roadmap

The 11-stage plan lives in [`../work-plan.md`](../work-plan.md) §3; each stage's frozen
contract is a `docs/STAGE<N>_SPEC.md`. This file maps that plan to what is actually in the
repo **today**, and states the gaps plainly. "Landed" means the crate/interfaces exist and
are exercised by hermetic tests in-crate; it does **not** mean a production exit gate has
been demonstrated at scale.

A real crawler corpus now exists and is measured: **10 190 indexed documents across 53 hosts**
in `raw-dataset/` (raw blobs + `crawl-log.jsonl` + resumable `frontier.json`) with the index in
the server data root (`/var/lib/search`, gitignored). It is not committed — it is data, and it
is reproducible from `scripts/corpus-run.sh` and the commit's seed list.

## Stage status

| Stage | Deliverable (spec) | In the repo | Status |
| :--- | :--- | :--- | :--- |
| **0** Skeleton & spine | `core`, `metadata`, `ingest`, `frontier`, `fetcher`, `parser`, `indexer`, `api`, `agent0` | all present; `agent0 ingest/search/serve` wired; fault suite + restart/dup tests in-crate | **landed** |
| **1** 10K LLM MVP | `discovery`, `router`, `browser`, `pdf`, `sources`, `graph`, `retrieval` | all present; 10 190 documents / 53 hosts crawled and indexed; conditional-refresh `304` path; `TextVerifier` coverage against document text; `agent0 graph build` populates entities/aliases/edges/works; UI service client | **landed — except source breadth and browser escalation** (see gaps 1–2) |
| **2** Quality & benchmarks | `eval`, `embed`, `rerank`, `bench`, `benchmarks/` | present; 30-doc / 25-query smoke set + committed `results.json` | **landed** as a smoke set |
| **3** Freshness loop | `scheduler` | present; `agent0 refresh` tick + opt-in `SEARCH_QUERY_REFRESH` P0; live overlay | **landed** |
| **4** Agent navigation | `nav` | present: `SearchState`, action frontier, beam, policy tiers, provider adapters (fake-client tested) | **landed** |
| **5** Scale | `dedup`, `tiers`, `patterns`, migration `0004` | present | **landed** |
| **6** Dataset builder | `dataset` + `datasets/fixtures/` | present: 4 adapters, `write_pack`/`validate_pack`, `PACK_SCHEMA=1` | **landed** |
| **7** Service surfaces | `mcp` + `deploy/` + `docs/MCP.md` | present; deploy units + nginx; all three MCP tools (`search`/`deep_search`/`fetch_doc`) backed by `HttpBackend` | **landed** |
| **8** Eval & hardening | `trace`, `cache`, sources priors, `acceptance` | all present; **12 hermetic acceptance tests** green; trace wired into `agent0` | **landed** |
| **9** Advanced retrieval | `ann`, `distill`, retrieval sparse | present: int8/IVF/HNSW-lite, `SparseExpander`, `LinearPolicy` (all deterministic, fixture-trained) | **landed** |
| **10** Distribution & ops | docs + scripts | `dist/` build/publish/backup scripts + the four docs here | **landed** |
| **10+** Distributed crawl & community | — | not started | **planned** |

## Query planning (new)

The engine's §8 query expansion is no longer inert: an opt-in LLM planner
(`crates/planner`, `search_engine::QueryPlanner`) deconstructs a query into entity/phrase probes
that are searched as variants under the §10 family cap, on the cached kitani model. Off by
default; see `docs/PLANNER.md`. What is *not* done: a measured planner-on vs planner-off quality
delta on the real corpus (the harness exists, the key and the run do not).

## Honest remaining gaps

These are real, current limitations — none is a hidden TODO:

1. **Source breadth is short of the Stage-1 corpus criterion.** The plan asks for
   "hundreds–thousands of sources"; the measured run reached **53 hosts**, dominated by two
   large sites (docs.rs, huggingface.co). The crawl now drains the seed list *before* a resumed
   backlog and caps attempts per origin (`--origin-cap`, `ORIGIN_CAP`), and the seed file has
   ~235 distinct roots, so the next run spreads across publishers — but the criterion is met by
   evidence, not by intent, and it is not met yet.
2. **The browser escalation path exists but is not wired.** `crates/browser` (persistent
   Chromium, short-lived contexts, resource limits) has no dependents: nothing in `agent0` or
   the router ever escalates to it, so JS-only pages are simply not served. The PDF path *is*
   wired (`search-pdf` is a dependency of `agent0`).
3. **The graph is populated, the graph *lane* is not.** `agent0 graph build` now writes
   entities, aliases, co-occurrence edges and works, but engine §9.4 still runs no
   personalised-PageRank activation over them: retrieval consumes the graph only through
   title/alias signals.
4. **The semantic embedder is opt-in, not the default.** Real BGE-small-en-v1.5 weights now
   work (`feature = "bge"` in `crates/embed`, via `fastembed`; weights fetched on first use
   into `<workspace>/models/fastembed`). The **default** build still uses the deterministic
   `HashEmbedder`, which is a lexical bag-of-words model, **not a semantic one** — so the
   default `benchmarks/results.json` still reports a lexical dense lane, honestly labelled.
   Rebuild with `--features bge -- --embedder bge` to measure the semantic lane; on the
   paraphrase query set it moves nDCG@10 0.4238 → 0.5842 (see `benchmarks/README.md`).
5. **No real SPLADE weights.** `SparseExpander` (`crates/retrieval/src/sparse.rs`) is a
   deterministic co-occurrence expander built from a committed fixture — a stand-in for a
   learned sparse model, not one.
6. **No trained cross-encoder.** `LexicalCrossEncoder` (deterministic lexical overlap) is
   the proxy. On the lexical smoke set it *hurts* nDCG (0.985 → 0.941) while on the
   paraphrase set it helps (0.424 → 0.619); both are honest findings about the proxy, not a
   tuned result. See `benchmarks/README.md`.
7. **Benchmarks are a fixture, not a corpus measurement.** Stage 2 asks for 500–2000 labelled
   queries; `benchmarks/` holds 50 over a 30-document corpus. `scripts/engine-eval.sh` measures
   the *engine* end to end against the real index (8 queries, expectations, provenance checks),
   which is a different and narrower thing than a labelled relevance benchmark at scale.
8. **Single-box only.** One `agent0 serve` process owns the data root and the API — and the
   index writer lock is exclusive, so a crawl and a server cannot share a data root; a build or
   a crawl requires stopping the server (observed twice as `Failed to acquire Lockfile`). There
   is no scheduler cluster and no distributed index. `deploy/` ships two units that must **not**
   both bind `127.0.0.1:8080` on one host.
9. **Frontier single-writer exception.** Frontier lease acquisition deliberately keeps its
   own dedicated SQLite connection (an atomic read-modify-write) beside the batched
   metadata writer (`STAGE0_SPEC.md` §1). This is the layer that would be replaced at
   distributed scale.
10. **`doc_type` and `language` are indexed but not stored** in the Tantivy schema
    (`STAGE0_SPEC.md` §6): they support filtering, but a result cannot echo them back.
11. **`Indexer::manifest()` returns `None` before the first publish.** An unpublished index
    has no read-side state; searches then carry `degradations=["index_unpublished"]`
    (`crates/api/src/query.rs`). See `OPERATIONS.md`.
12. **MCP is a loopback HTTP client — it needs a running API.** `search`, `deep_search` and
    `fetch_doc` are all backed by `HttpBackend` (`crates/mcp/src/http.rs`), which talks to
    `/search`, `/search_deep` and `/read` on `SEARCH_API_BASE` (→ `SEARCH_BIND` → the
    `127.0.0.1:8080` default). With no API reachable, every tool returns an honest
    `isError`; there is no embedded in-process backend.
13. **Verification is token-based, not entity-aware.** §16 coverage can be satisfied by a page
    that contains a unit's tokens without the fact (recorded in `engine/engine-architecture.md`
    §26). The trace names the supporting document so the verdict can be audited; the answerer
    remains the honesty gate.
14. **`RUST_LOG` filtering is coarse.** The subscriber is installed
    (`search_api::init_tracing`) but nothing emits per-request access logs, so HTTP request
    traces are not in journald — only pipeline `tracing` events.
15. **Packs are produced/consumed by library calls, not a CLI or registry.** There is no
    `search-dataset` binary and no remote pack registry (see `DISTRIBUTION.md`).

## What Stage 10+ requires

From `work-plan.md` §3 / `engine/ARCHITECTURE.md` §35: distributed crawl (scheduler cluster
→ origin queues → workers → parsers → distributed index) **only after quality is proven**,
and a public pipeline of community source packs, adapters, judgments, and privacy-
preserving telemetry. Neither is started. Treat the current system as a proven single-box
spine to grow from, not a distributed one.
