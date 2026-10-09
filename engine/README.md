# Where the engine is implemented

This directory holds the **specification**. The code that implements it lives in `../crates`
(Rust workspace) and `../ui`. This file is the map between them, so a reader of the spec can find
the implementation of any section — and see plainly what is *not* implemented yet.

Nothing here is aspirational: every "implemented" line names a path you can open.

## Section → code

| Spec | What it covers | Implementation |
| --- | --- | --- |
| §3 Core data model (Entity, Concept, Domain, Source, Resource, Work, Content) | the 5-id identity model | `crates/core/src/ids.rs`, `crates/core/src/contract.rs`; graph rows in `crates/metadata`, entities in `crates/graph` |
| §4 Storage architecture | data root, tiers, caches | `crates/core/src/config.rs`, `crates/tiers`, `crates/cache`, `crates/indexer/src/store.rs` |
| §5 The data builder | turning fetched bytes into indexed documents | `crates/agent0/src/pipeline.rs` (ingest lifecycle), `crates/ingest`, `crates/parser`, `crates/dataset` |
| §6–§12 Crawl and discovery plane | sources, frontier, per-origin scheduling, fetch, freshness, query-triggered crawl | `crates/agent0/src/crawl.rs`, `crates/frontier`, `crates/fetcher`, `crates/discovery`, `crates/router`, `crates/scheduler` |
| §13 Indexing plane | Tantivy base+overlay, manifest gating, incremental indexing | `crates/indexer`, `crates/metadata` |
| §14–§17 Query pipeline, routing, parallel retrieval, source-first | analysis → plan → lanes → fusion | `crates/engine` (coordinator, `analyze`, `plan`, `coord`, `evidence`), `crates/retrieval` |
| §18 Search frontier and graph navigation | entity graph, expansion | `crates/graph/src/activate.rs` (bounded personalised PageRank), `crates/graph/src/sidecar.rs`, lane in `crates/api/src/graph_lane.rs` |
| §19–§20 Agent policy, live retrieval loop | bounded action space, injected LLM decisions | `crates/engine` (`Deps.answerer`, every LLM decision behind a trait with a deterministic stub) |
| §21–§24 Reranking, ranking, diversity, stopping | cascade, ranker, MMR, stop reasons | `crates/rerank`, `crates/ann`, `crates/engine/src/coord.rs` (`StopReason`) |
| §25 Search modes | fast / auto / deep | `crates/engine` modes; mode routing in `crates/api/src/engine.rs` |
| §26–§27 Search UI, live UI | the client | `ui/` (real client, 33 vitest tests) |
| §28 Agent API | `/search`, `/search_deep`, `/read` + MCP | `crates/api/src/server.rs`, `crates/mcp` — **partial, see below** |
| §29 Evaluation system | metrics, benchmarks | `crates/eval` (nDCG/MRR/recall), `crates/bench`, `benchmarks/queries.*.jsonl`; acceptance harness `crates/acceptance` is a **stub** |
| §30 Learning system | feedback into ranking | `crates/distill`, `crates/organize` (data structuring) — **partial** |
| §31 Dedicated future search model | trained model | not implemented (the portable pack + `crates/dataset` are the data side) |
| §32 Security architecture | SSRF, limits, trust boundary | `crates/fetcher` (SSRF guard), `crates/core/src/config.rs` (limits) |
| §33 Deployment | units, nginx | `deploy/` |
| §34 Repository architecture | suggested layout | partially matched — see below |

## Verified gaps

Recorded here because a spec that nobody audits is a wish. Each line was checked by searching the
tree, not from memory.

- **§28 Agent API is ~a third implemented.** HTTP exposes `/health`, `/search`, `/search_deep`,
  `/read`; MCP exposes `search` only. Absent with **zero occurrences anywhere**: `source_lookup`,
  `source_refresh`, `find_in_document`, `follow_path`. `backtrack` exists only as a name in
  `crates/sources`. The request contract has no `freshness` field. The underlying capabilities for
  four of the five tools do exist (`Indexer::doc_text`, the sources table with `due_sources`,
  `Pipeline::run_refresh_tick`, and `Graph::expand`/`edges_of`) — it is the surface that is missing.
- **§44 deletion detection** — *now implemented* (`crates/agent0/src/crawl.rs`, `Pipeline::mark_deleted`):
  a 404/410 for an indexed URL marks the document DELETED, keeping its bytes.
- **Crawler §24–§35 content routes are absent**: research-paper pipeline, OAI-PMH, books and
  libraries, archives, git/software, forums, news/blogs, docs sites, APIs/JSON/XML, S3/datasets,
  images, video/audio. `crates/sources` is the adapter surface; `crates/patterns` is a partial
  pattern engine.
- **Crawler §54–§58 learning layers are absent**: specialized adapters, source learning, URL-pattern
  learning, query-demand learning.
- **Crawler §50 distributed architecture** is deliberately not built; §51's single-server layout is
  what runs.
- **Crawler §61 step `update_graph` is not part of the crawl loop** — the graph is a separate batch
  (`agent0 graph build`), so crawled entities reach the query lane only after a rebuild.
- **§34 layout**: `models`, `agents`, `/workers`, `/builder` (commoncrawl, openalex, arxiv,
  wikidata, openlibrary), `/adapters` (oai, arxiv, github, libraries) and `/packs` do not exist as
  directories. Their functionality is either elsewhere (`browser`, `pdf`, `embed` live under
  `crates/`) or genuinely missing (the builder sources and adapters).
- **§29 acceptance tests / §24 promotion gates**: `crates/acceptance` is 4 lines.

## Measured quality (so the numbers are not a surprise)

`agent0 eval run` over 600 corpus-derived cases (`benchmarks/queries.corpus.jsonl`, labels are a
proxy: query = title, label = that document), through the shipped engine with the dense and graph
lanes enabled:

```
nDCG@10 0.5015 | MRR 0.5003 | recall@10 0.5050 | p50 1174 ms | p95 2762 ms | lane errors 0
```

It finds the described page about half the time and, when it does, ranks it first or second. There
is no baseline comparison yet (same cases with the extra lanes off, and with the engine off), so
whether the engine beats plain lexical retrieval on this corpus is **not yet known**.
