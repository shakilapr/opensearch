# Roadmap

The 11-stage plan lives in [`../docs/legacy/work-plan.md`](../docs/legacy/work-plan.md) §3; each stage's frozen
contract is a `docs/specs/STAGE<N>_SPEC.md`. This file maps that plan to what is actually in the
repo **today**, and states the gaps plainly. "Landed" means the crate/interfaces exist and
are exercised by hermetic tests in-crate; it does **not** mean a production exit gate has
been demonstrated at scale.

A real crawler corpus now exists and is measured — in fact **two corpora are in play and they
are not the same thing**. (1) The *serving* index in the server data root (`/var/lib/search`,
gitignored): **19 255 documents across 222 hosts, generation 19254 (2026-10-11)**. (2) A
*fleet* build split across four shards under `/var/lib/search-fleet/shard-{0..3}`, grown to
**~323 000 documents across 3 948 hosts**, **not yet merged** into the serving index (the
`deploy-fleet-corpus` work item). Both are data, neither is committed, and both are
reproducible from `scripts/corpus-run.sh` / `scripts/scale-crawl.sh` and the seed list;
`ops/state.json` carries the live counts. Corpus figures drift as the crawler runs — cite the
generation, not the prose.

## Stage status

| Stage | Deliverable (spec) | In the repo | Status |
| :--- | :--- | :--- | :--- |
| **0** Skeleton & spine | `core`, `metadata`, `ingest`, `frontier`, `fetcher`, `parser`, `indexer`, `api`, `agent0` | all present; `agent0 ingest/search/serve` wired; fault suite + restart/dup tests in-crate | **landed** |
| **1** 10K LLM MVP | `discovery`, `router`, `browser`, `pdf`, `sources`, `graph`, `retrieval` | all present; 19 255 documents / 222 hosts indexed in the serving index (gen 19254), a four-shard fleet build past 323 000 / 3 948; conditional-refresh `304` path; `TextVerifier` coverage against document text; `agent0 graph build` populates entities/aliases/edges/works; browser escalation wired (`SEARCH_BROWSER`); graph lane live; UI service client | **landed — except source breadth** (see gap 1) |
| **2** Quality & benchmarks | `eval`, `embed`, `rerank`, `bench`, `benchmarks/` | present; 30-doc / 25-query smoke set + committed `results.json` | **landed** as a smoke set |
| **3** Freshness loop | `scheduler` | present; `agent0 refresh` tick + opt-in `SEARCH_QUERY_REFRESH` P0; live overlay | **landed** |
| **4** Agent navigation | `nav` | present: `SearchState`, action frontier, beam, policy tiers, provider adapters (fake-client tested) | **landed** |
| **5** Scale | `dedup`, `tiers`, `patterns`, migration `0004` | present | **landed** |
| **6** Dataset builder | `dataset` + `datasets/fixtures/` | present: 4 adapters, `write_pack`/`validate_pack`, `PACK_SCHEMA=1` | **landed** |
| **7** Service surfaces | `mcp` + `deploy/` + `docs/specs/MCP.md` | present; deploy units + nginx; all three MCP tools (`search`/`deep_search`/`fetch_doc`) backed by `HttpBackend` | **landed** |
| **8** Eval & hardening | `trace`, `cache`, sources priors, `acceptance` | all present; **13 hermetic acceptance tests** green; trace wired into `agent0` | **landed** |
| **9** Advanced retrieval | `ann`, `distill`, retrieval sparse | present: int8/IVF/HNSW-lite, `SparseExpander`, `LinearPolicy` (all deterministic, fixture-trained) | **landed** |
| **10** Distribution & ops | docs + scripts | `dist/` build/publish/backup scripts + the four docs here | **landed** |
| **10+** Distributed crawl & community | — | not started | **planned** |

## Query planning (new)

The engine's §8 query expansion is no longer inert: an opt-in LLM planner
(`crates/planner`, `search_engine::QueryPlanner`) deconstructs a query into entity/phrase probes
that are searched as variants under the §10 family cap, on the cached crowbot model. Off by
default; see `docs/specs/PLANNER.md`. What is *not* done: a measured planner-on vs planner-off quality
delta on the real corpus (the harness exists, the key and the run do not).

## Honest remaining gaps

These are real, current limitations — none is a hidden TODO:

1. **Source breadth is short of the Stage-1 corpus criterion, but far closer.** The plan asks
   for "hundreds–thousands of sources". The serving index now spans **222 hosts** and the
   four-shard fleet build spans **3 948 distinct hosts** (2026-10-11), up from the 53 hosts of
   the first measured run. The crawl drains the seed list *before* a resumed backlog and caps
   attempts per origin (`--origin-cap`, `ORIGIN_CAP`), and the seed file holds ~235 distinct
   roots spread breadth-first. What the criterion still wants is a *published, imported* corpus
   at that breadth: the fleet is 3 948 hosts but is **not yet merged** into the serving index
   (the `deploy-fleet-corpus` work item), so the *served* breadth is 222, not 3 948.
2. ~~**The browser escalation path exists but is not wired.**~~ **Closed** (`b5cdeba`,
   *feat(crawl,browser): escalate JS-only pages to Chromium, and make the driver real*).
   `crates/browser` now has a dependent: an HTML page whose normalization yields no text
   escalates once to a `BrowserFetcher` and is indexed from the render (falling through to the
   old `html-no-text` route if the render is no better), and `ChromiumBrowser` compiles and
   renders real JavaScript (`cargo test -p search-browser --features chromium --test
   chromium_live -- --ignored`). Wiring: `SEARCH_BROWSER=1` (feature `chromium`),
   `SEARCH_CHROMIUM` for the executable, `SEARCH_CHROMIUM_NO_SANDBOX=1` where Chromium refuses
   to run as root. Policy is pinned by three hermetic `MockBrowser` tests in
   `crates/agent0/tests/crawl.rs`. *Remaining:* escalate on a *thin* (not only empty)
   normalization, and a per-host escalation budget.
3. ~~**The graph is populated, the graph *lane* is not.**~~ **Closed** (`d337319`).
   `crates/api/src/graph_lane.rs` seeds `search_graph::activate` (bounded personalised PageRank
   over the co-occurrence edges) from the entities a query names, joins them to documents via the
   entity→document sidecar, and stamps the hits `Lane::Graph` — so `graph_expansion` is measured,
   not a placeholder. `agent0 serve` loads the sidecar automatically when it exists, beside the
   dense lane. *Remaining:* the lane cannot honour a source scope (it returns empty for a scoped
   search rather than leaking), and its weight in the fused ranking is untuned and unmeasured
   against the eval set.
4. **The embedder default is *split by crate*, and the docs long said otherwise.** In
   `crates/embed` the `bge` feature is **off** by default, so the *library* needs no model
   download; in `crates/bench` the default is likewise the lexical `HashEmbedder`, which is why
   the committed `benchmarks/results.json` is a lexical dense lane (honestly labelled as **not** a
   semantic model). But **`crates/agent0` enables `bge` by default** (`default = ["bge"]`), so the
   *serving* binary runs real BGE-small-en-v1.5 (384-d) — that is what the live deployment does
   (weights in `/var/lib/search/fastembed`), and `agent0 embed build` defaults to `--embedder
   bge`. If the weights cannot be loaded the server serves **no dense lane** (with a warning),
   never a lexical stand-in. Rebuild with `--features bge -- --embedder bge` to measure the
   semantic lane; on the paraphrase query set it moves nDCG@10 0.4238 → 0.5842 (see
   `benchmarks/README.md`).
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
8. **Single-box only.** One `agent0 serve` process owns the data root and the API. There is no
   scheduler cluster and no distributed index. `deploy/` ships two units that must **not** both
   bind `127.0.0.1:8080` on one host. (The old *"a crawl and a server cannot share a data root"*
   limitation is gone: `SEARCH_READ_ONLY` defaults to auto — a server that cannot take the writer
   falls back to read-only and logs `serving READ-ONLY — query-time refresh is off`
   (`crates/agent0/src/main.rs`), so a crawl and `agent0 serve` coexist on one root. The
   trade-off is disabled query-time refresh, not a failure to start.)
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

From `docs/legacy/work-plan.md` §3 / `engine/ARCHITECTURE.md` §35: distributed crawl (scheduler cluster
→ origin queues → workers → parsers → distributed index) **only after quality is proven**,
and a public pipeline of community source packs, adapters, judgments, and privacy-
preserving telemetry. Neither is started. Treat the current system as a proven single-box
spine to grow from, not a distributed one.
