# Stage status — what is done and what is left, per stage

This answers one question precisely: **for each stage in `work-plan.md` §3, what remains?**
`docs/ROADMAP.md` maps the plan to crates; this file is the item-level audit and is meant to be
edited as items close. Nothing here is aspirational: every "left" item names what it needs
(code, data, a decision, or scale) and where the evidence lives.

Legend for "left": **code** = implementable in this repo now · **data** = needs wall-clock
collection (crawl/model download) · **decision** = needs a product/scope call · **scale** =
multi-week architecture work, gated by the plan on proven quality.

## Stage 0 — Skeleton & spine
- **Done:** all ten spine crates; `agent0 ingest/search/serve/crawl/refresh/export/import/graph`;
  5-ID invariant; ingest lifecycle; fault suite, restart and duplicate-ingest tests.
- **Left:** nothing outstanding.

## Stage 1 — 10K LLM Search MVP
- **Done:** sitemap + RSS/Atom discovery; MIME router with a Readability-style HTML route; PDF
  pipeline wired (`search-pdf` is an `agent0` dependency); source registry populated (58
  `source_profile` rows); freshness metadata (`published_at`/`modified_at`/`fetched_at`/
  `verified_at`/`feed_watermark` kept distinct); source-first retrieval; UI wired to the real API
  with a "Why this result?" provenance panel (`ui/src/components/SearchItem.tsx`);
  **19 255 indexed documents across 222 hosts** in the serving index (generation 19254,
  2026-10-11), and a separate four-shard fleet build past **323 000 documents / 3 948 hosts**.
- **Left:**
  1. **Source breadth — data.** 222 hosts in the serving index (3 948 across the fleet build)
    against a target of "hundreds–thousands"; the fleet is **not yet merged** into the serving
    index (see `docs/ROADMAP.md` gap 1). The seeds file holds ~235 distinct roots ordered
    breadth-first, the crawl caps attempts per origin (`--origin-cap` / `ORIGIN_CAP`), and the
    crawler runs *beside* the server, so the corpus can grow without taking the API down. It
    needs hours of crawl time.
  2. **Browser escalation — done, live-verified.** `crates/browser` had **no dependents** and its
     real driver had never been compiled. Both are fixed: an HTML page that normalizes to no text
     escalates once to a `BrowserFetcher` and is indexed from the render (falling through to the
     old `html-no-text` route if the render is no better), and `ChromiumBrowser` now compiles and
     renders: `cargo test -p search-browser --features chromium --test chromium_live -- --ignored`
     returns text that exists only after the page's JavaScript runs. Wiring: `SEARCH_BROWSER=1`
     (feature `chromium`), `SEARCH_CHROMIUM` for the executable, `SEARCH_CHROMIUM_NO_SANDBOX=1`
     where Chromium refuses to start as root. Policy is pinned by three hermetic `MockBrowser`
     tests in `crates/agent0/tests/crawl.rs`.
     *Remaining:* escalate on a *thin* (not only empty) normalisation, and a per-host escalation
     budget so a JS-heavy origin cannot multiply crawl cost.
  3. **Entity graph *lane* — built and live.** `agent0 graph build` populates entities, aliases,
     co-occurrence edges and works, and now also writes the **entity→document sidecar** (the join
     the frozen schema cannot hold). `search_graph::activate` runs bounded personalised PageRank
     over the co-occurrence edges, and `crates/api/src/graph_lane.rs` seeds it from the entities a
     query names, joins them to documents, and stamps the hits `Lane::Graph` — which is exactly
     what the API already reported as `graph_expansion`, so the signal is now measured rather than
     a placeholder. Wiring is automatic: the binary loads the sidecar when it exists, beside the
     dense lane.
     *Remaining:* the lane cannot honour a source scope (it returns empty for a scoped search
     instead of leaking), and its weight in the fused ranking has not been tuned or measured
     against the eval set. Seeding is deliberately conservative — a match must look like a name
     (phrase, digit, acronym, or capitalised in the query) unless the query is short enough to be a
     term lookup — because the graph holds single capitalised common nouns (`Phrase`, `Database`)
     that would otherwise make the lane fire on any query.
  4. ~~UI provenance verified live.~~ **Done** — verified in a browser (see the exit gate above),
     with the CORS gap that blocked it closed (`SEARCH_CORS_ORIGIN`, tested in
     `crates/api/tests/cors.rs`).

## Stage 2 — Search quality & benchmarks
- **Done:** `eval`, `embed`, `rerank`, `ann` present; RRF fusion; feature ranker; MMR
  diversification; entity hypotheses retained until context resolves (§14); ablation harness with
  committed `results.json`; real BGE-small-en-v1.5 behind `feature = "bge"`.
- **Left:**
  1. **Labelled benchmark at scale — data/decision.** The plan asks for 500–2000 queries across
     classes with LLM-judge + manual labels; `benchmarks/` holds 50 over a 30-document fixture.
     Generating labels costs model calls and a decision to spend them.
  2. **Dense lane — DONE (corpus backfill is data, and it is complete).** Built end to end:
     `crates/embed::VectorStore` (a `doc_id` → vector sidecar carrying `source_id`, so a scoped
     lane call can honour it, with a byte-stable on-disk format), `crates/retrieval::DenseLane`
     (a `LaneSearcher` over it, brute-force cosine, refusing a store whose dimension does not
     match its embedder), `agent0 embed build` (backfill, opens the index **read-only** so it runs
     beside a crawl), and API wiring (`AppState::with_extra_lanes`; the engine fuses any injected
     lane by rank, naming it in `lane_ranks`). The `bge` feature is **off by default in
     `crates/embed`** (the library needs no model download), but the **`agent0` binary enables it
     by default** (`crates/agent0/Cargo.toml`: `default = ["bge"]`): the server builds a real
     BGE-small-en-v1.5 lane (384-d, weights under `<data_dir>/fastembed`), `agent0 embed build`
     defaults to `--embedder bge`, and when the weights are unavailable the server serves **no
     dense lane** (with a warning) — it does not fall back to a lexical stand-in. `crates/bench`
     is the exception that defaults to the lexical `HashEmbedder`, so the committed
     `benchmarks/results.json` measures a lexical dense lane, honestly labelled; add
     `--features bge -- --embedder bge` to measure the semantic one.
     Live: the serving binary runs BGE — `embedder bge (384 dimensions)`, sidecar `docs=19255`
     after the 2026-10-11 rebuild — and every result for "what is retrieval augmented
     generation" carries `{"bm25": n, "dense": m, "source_first": k}` with dense ordering
     genuinely different from BM25. Remaining refinement: brute force does not scale past ~100K
     documents (`crates/ann` exists for that), and the fixture benchmark has not been re-run to
     state a serving-quality delta.
  3. **Real sparse weights and a trained cross-encoder — decision taken: ship the documented
     proxies.** They stay proxies, labelled as such in `docs/ROADMAP.md` gaps 5–6.
  4. **Ablations measured on the real corpus — code.** Ablations exist, but only on the fixture.

## Stage 3 — Live freshness loop
- **Done:** HOT/WARM/COLD classification (`crates/scheduler/src/class.rs`); learned change model
  `P(changed) = 1 − exp(−rate·age)` (`change.rs`); adaptive delay (`adaptive.rs`); conditional
  revalidation with `If-None-Match`/`If-Modified-Since` and 304-as-unchanged; query-triggered
  P0 refresh behind `SEARCH_QUERY_REFRESH`; mutable overlay + immutable base; watermarks merged by
  version; activation scheduler with a bounded fan-out and an atomically charged budget ledger.
- **Left:**
  1. **Live "latest/today" demonstration against real hosts — data.** The exit gate ("a latest
     query returns results the index did not yet contain, without storming") is covered by
     hermetic tests (`tests/stage3_e2e.rs`) and has not been re-run against the live web.
  2. **Refresh is disabled while serving read-only** (see "Operational consequence" below) — a
     crawl holding the index lock means the server cannot refresh. That is honest, not silent:
     the server logs it and `query_refresh` returns a no-op report.

## Stage 4 — Agent navigation
- **Done:** `SearchState`, action frontier (`retrieve/expand_graph/refresh/follow_link/rerank/
  verify_join/widen`), visited/action-key set, cycle detection; best-first traversal + beam;
  evidence joins with join keys and time order; policy hierarchy `Rules → Algorithm →
  CallerAgent → Provider` (`crates/nav/src/policy.rs`); bounded/deadline/coverage-driven stopping.
  **Query deconstruction by a live model — built and wired.** `search_engine::QueryPlanner` is an
  injection seam (like `Answerer`/`EvidenceVerifier`); `crates/planner` implements it over the
  cached LLM client (crowbot's `deepseek-v4.1-flash` by default, cached, opt-in) and turns a
  conversational query into expansion probes — canonical entity names, phrase-with-slop probes —
  which the engine now actually *searches*: the §8 query variants `build_plan` always produced
  were previously computed and discarded, and are now launched in round 1 and fused per family
  under the §10 cap. Each probe is still rejected by `variant_preserves_constraints` if it drops a
  constraint. See `docs/PLANNER.md`. *Remaining:* no live measurement of planner-on vs planner-off
  quality (needs a key and the eval harness).
- **Left:**
  1. **Real provider transports for the *action frontier* — code.** `ProviderPolicy` talks to a
     `ModelClient` trait with hermetic fakes; there is no HTTP transport for
     OpenAI/Claude/DeepSeek/GLM, so the *navigation* loop (which action to take next) cannot be
     driven by a live model. Needs transports + a key-path policy (the answer crate's pattern:
     env-only, never logged) + tests with a recorded fake. *Distinct from query deconstruction,
     which now has a live path — see the Done list.*
  2. **Agent policy on the live corpus — data.** No measurement of ambiguous/multi-hop queries
     improving versus one-shot retrieval at 11K documents.

## Stage 5 — Scale to 100K–1M documents
- **Done:** storage tiers (`crates/tiers`) with a move planner and migration 0004; archive-bomb
  and resource guards; near-duplicate detection (MinHash + LSH bands, `crates/dedup`); URL
  pattern learning (`crates/patterns`: `templatize`, `observe`, `is_paginated`).
- **Left:**
  1. **Enforce the tiers on the live store — code.** Classification and planning exist; no
     scheduled promotion/demotion runs against `/var/lib/search`.
  2. **Feed learned URL patterns into frontier priority — code.** `patterns` is exercised by unit
     tests, not wired into `enqueue`'s `url_rank`.
  3. **Measure at 100K → 1M — data/scale.** p95 latency, bytes/doc, RAM, dup ratio are unmeasured
     beyond ~12K documents.
  4. **Distributed crawl/index — scale.** Explicitly gated by the plan on proven quality.

## Stage 6 — Dataset bootstrapping
- **Done:** four adapters (`CommonCrawlAdapter`, `OpenAlexAdapter`, `WikidataAdapter`,
  `OaiPmhAdapter`), streaming pack writer/validator, `PACK_SCHEMA=1`, fixtures.
- **Left:**
  1. **Run an adapter against a real snapshot — data.** Fixture-tested only; no Common Crawl
     URL-index or OpenAlex snapshot has been read.
  2. **Wire the output into the crawl — code.** No path takes a generated pack and enqueues it.

## Stage 7 — Service surfaces & deployment
- **Done:** MCP server with `search` / `deep_search` / `fetch_doc` over an `HttpBackend`;
  `docs/MCP.md`; `deploy/` units + nginx; `dist/` build/publish/backup scripts.
- **Left:**
  1. **MCP needs a running API — by design, documented.** An embedded in-process backend does not
     exist.
  2. **Two deploy units both target `127.0.0.1:8080`** — they must not run on one host as shipped.

## Stage 8 — Evaluation, learning, hardening
- **Done:** `trace` (real JSONL sink wired into `agent0`), `cache`, source priors, 12+ hermetic
  acceptance tests, `scripts/engine-eval.sh` (8-query end-to-end engine evaluation with
  expectations and provenance checks).
- **Left:**
  1. **Per-request access logs — code.** The subscriber is installed but HTTP requests are not
     logged, so journald has pipeline events only.
  2. **Acceptance suite against the live corpus — code.** The acceptance tests are hermetic; the
     live checks are the ad-hoc `engine-eval.sh` runs.

## Stage 9 — Optional advanced retrieval & policy distillation
- **Done:** int8 quantization + IVF/HNSW-lite (`crates/ann`), `SparseExpander` (fixture-backed),
  `LinearPolicy` (`crates/distill`) — all deterministic and fixture-trained.
- **Left:** trained-by-real-data versions of all three; `docs/ROADMAP.md` gaps 4–6 record that the
  shipped models are proxies.

## Stage 10 — Distribution & operations
- **Done:** `scripts/build-release.sh`, `publish-pack.sh`, `backup.sh`; `docs/OPERATIONS.md`,
  `DISTRIBUTION.md`, `CONTRIBUTING.md`; release binaries staged in `dist/bin`.
- **Left:** distribution beyond one box (plan §Stage 10+): scheduler cluster, origin queues,
  workers, distributed index, public pack registry. Not started, by design.

## Operational consequence of this audit

The previous blocker is gone: `agent0 serve` now opens the index **read-only when a writer holds
it** (`SEARCH_READ_ONLY=auto`, the default), so a crawl and the server share one data root. The
trade-off is explicit and logged — while the crawler holds the lock the server cannot perform
query-time refresh — rather than the two processes refusing to coexist. Both were verified
running together: crawl ingesting, `GET /health` at generation 12094, `POST /search {"engine":
true}` returning coverage 1.0 with a cited answer, no SQLite lock errors (WAL).

## v2 retrieval architecture — where it stands (2026-11-01)

The v2 plan (`docs/PLAN-v2.md`, `docs/architecture-update-v2.md`) is tracked in
`docs/EXECUTION-STATUS-v2.md` with measured numbers. In one paragraph: the concept layer (A), the
dense fleet vectors (B) and the learned sparse-feature index (D) are built and serving-reachable;
routing and stage-3 rerank (E) are implemented behind `SEARCH_ROUTE` / `SEARCH_RERANK` and off by
default; the storage rebuild (C) is the one phase still pending, and it must run after B and D so
the corpus is not re-embedded for nothing.

Every semantic lane exists and none of them yet beats BM25 on the 600-query graded set: dense
+0.0017 nDCG, graph 0.0000, concept −0.0114, learned features −0.0910 at weight 0.8. Each lane's
fusion weight is therefore whatever was measured, and for features that is **0.0** — the lane is
built, tested, and deliberately absent from scoring. The sparse index itself is complete: 1.39M
documents, 16.0 postings each, 22.2M `doc_feature` rows, 87.6–100% resource coverage.
