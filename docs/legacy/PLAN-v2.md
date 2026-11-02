# Plan v2 — from the current engine to a neural associative index

This is the consolidated plan. It supersedes the sequencing in `docs/legacy/index-upgrade-plan.md`,
`docs/legacy/semantic-brain-design.md` and `docs/legacy/analysis-next-move.md`; those remain as the reasoning behind the
decisions. `docs/legacy/architecture-update-v2.md` is the target design; this file is what we do, in what
order, with the gates that decide whether a step is kept.

Everything marked **measured** was read from the running system or the repository, not estimated.

---

## 0. Where we actually are (measured 2026-10-27)

| Fact | Value | Consequence |
| --- | --- | --- |
| Serving index | gen 19254, **19,255 documents**, 225 domains | the served corpus is 3.8% of the published fleet |
| Fleet published | **501,247 documents** across four shards, 1,090,184 resources | 26× the served corpus, unmerged |
| Documents with text on disk but unpublished | **589,572** | a real backlog, no network needed |
| Frontier | 9,050,755 URLs over 338,609 domains, **328,712 stuck `FETCHING`** with expired leases | discovery is ahead of fetching; leases must be recovered before a resume |
| Disk | 596 GB used, **78 GB free** | enough to build and to run a derived-layer pass |
| Legacy `/search` latency | **71–92 ms** | this is the number the <50 ms p95 target must be measured against |
| `engine:true` latency | **5.4–46.7 s** | an LLM call on the hot path whose answer is then discarded (ENOSPC) |
| Dense lane weight | **0.0** | the semantic judgement is computed and thrown away |
| source-first lane | **identical to BM25** on 64/64 results measured | a duplicate lane, not an accelerator |
| Documents with embeddings | 19,255 of 1,090,184 | the sparse-feature stage needs embeddings first |
| Embedding throughput | **8 docs/s** (BGE-small, 2,000 chars) | 40 min serving / 17 h published fleet / 38 h everything |
| Raw HTML vs index | **284 GB** vs **6 GB** | the storage problem is the raw cache, not the index |

---

## 1. Check of `architecture-update-v2.md` against the code

### 1.1 Already true, or cheaper than the document assumes

| v2 claim | Reality |
| --- | --- |
| "Eliminate duplicate source-first ranking behavior" (§19) | Confirmed independently: source-first returns the same hits as BM25 because both lanes wrap one `Searcher` with a shared request cache and `source_id=None`. Measured 64/64. |
| "Separate local search from optional answer generation" (§19) | Required, and urgent: the answer path is synchronous on the request and currently loses every answer to `ENOSPC`. |
| Block-Max WAND / SEISMIC-style candidate selection (§8) | **Already available**: `tantivy 0.22.1` ships `src/query/boolean_query/block_wand.rs`, and our index is Tantivy. A sparse-feature index can reuse WAND rather than requiring a new engine. |
| Sparse postings, quantized weights, immutable segments (§9) | Tantivy provides sorted doc ids, block summaries, skip lists, immutable segments and a mutable overlay — the same shape the document specifies. |
| `model_registry` with versions and status (§18) | Landed in migration 0005. |
| Typed concept graph, bounded traversal (§13) | `concept_relation` landed in 0005; `graph_edge` (entity co-occurrence) exists but is empty on every fleet shard. |
| Identity layer: website ≠ hostname, work ≠ location (§4) | Landed in migration 0006. |
| Source profiles normalised by document count (§17) | `source_concept.doc_count` landed in 0006 — the column exists precisely so the normalisation is auditable. |
| Storage: only activated features materialised (§16) | At our scale it is tiny: 32 features/doc × 1.09M docs ≈ **35 M postings ≈ 280 MB** at 8 B each. The feature matrix itself (8,192 × 384 f32) is **12.6 MB**. |

### 1.2 Gaps the document does not cover

| Gap | What it needs |
| --- | --- |
| **`FeatureID` has no home.** v2 §4 requires a feature id bound to a model version; the schema has concepts but no features | new tables: `feature(feature_id, model_id, version, space_id)`, `doc_feature(resource_id, feature_id, activation)` as the forward index; postings live in the sparse index (§4 below) |
| **Multidimensional associations.** v2 §6 wants `R(d,c) = [r1…rm]`; `doc_concept` stores one `weight` + `relation_kind` | either `doc_concept_dim(resource_id, concept_id, dim, value, model_version)` or a fixed set of named columns; the dimension table is safer because the dimensions will change |
| **Query encoder (Model D).** Nothing maps queries into the feature space | a CPU model, or — the cheap first version — the same teacher embedding projected through the trained `W`, which needs no new model at all |
| **Router (Model E).** No learned routing exists; `RuleSelector` is deterministic | first version: a hand-set rule table over query features (has digits / quoted phrase / question word / concept count), measured against a global fallback |
| **Latency budget vs reality.** <50 ms p95 (§15) | the legacy path is 71–92 ms **at 19K documents**; the target is only meaningful after the sparse path replaces full scans, and must be measured at 500K+ |
| **Matryoshka (§10).** "models specifically trained to preserve information at the selected dimensions" | BGE-small-en-v1.5 is **not** MRL-trained, so truncating it is invalid (the document says this). Either adopt an MRL teacher (bge-m3 class) at 4–8× the embedding cost, or drop coarse-to-fine until the sparse path is measured. |
| **`SEISMIC`-class compression** | Tantivy's posting format is not SEISMIC; matching its compression would mean a custom index. Decide after measuring WAND on real postings, not before. |

### 1.3 One correction to v2's framing

v2 presents the neural associative index as the primary retrieval subsystem. That is the right
*long-term* target, but at the current state the binding constraints are not semantic: they are a
discarded answer path, a 26× corpus that is not served, a duplicate ranking lane, and 284 GB of raw
HTML. The plan below puts those first because every one of them is cheaper than training a model and
each one changes what the semantic layer has to work with.

---

## 2. The plan

Order is by value per week, with a gate that must pass before the next phase starts. A phase that
cannot show its gate is not kept.

### Phase 0 — Recover availability (days)

1. **Answer-cache write becomes best-effort.** A successful completion must be returned even when
   the cache write fails; add an `ENOSPC` test. Gate: a query returns an answer with the disk full.
   **Done** (`7180806`): the write logs a warning and the answer is returned; the test simulates the
   failure with `ENOTDIR`. Deployed and verified live — the same query now answers in 431 chars, and
   the repeat hits the cache (301 ms, zero tokens).
2. **Free space**: `raw-dataset/` blobs 47 GB (100% duplicate of the fleet's `content` hashes),
   `target-browser` 4 GB, journals 1.7 GB. Gate: ≥100 GB free.
   **Partly done**: `target-browser` removed (4 GB). The `raw-dataset` blobs are the *live* crawl's
   output directory — four shard crawlers were running when this was attempted — so deleting them
   must wait for the crawl to stop. The duplicate claim is verified for all four shards
   (174,216/174,219 · 202,507/202,509 · 333,477/333,480 · 168,359/168,361).
3. **Recover 328,712 expired `FETCHING` leases** so a resumed crawl does not leak attempts.
   **Tool done** (`e0ef193`): `agent0 frontier recover [--dir PATH]… [--now EPOCH]`. Measured on a
   copy of shard 0: **82,553 rows reclaimed**. Not run against the live shards yet, because the
   crawlers are active and an expired lease there may belong to a slow in-flight fetch.
4. **Deploy HEAD** (the deployed binary is 2026-10-13; HEAD has two-phase generation publish,
   metadata counts, base-segment bulk import, configurable writer arena). Gate: smoke query set on
   the new binary, rollback copy kept.
   **Done**: rebuilt at HEAD, installed to `/opt/search/bin/agent0`, rollback copy kept, service
   healthy (gen 19254), `/search` verified.

### Phase 0b — Stop the disk filling again (done, `ca0a156`)

The crawl has filled this filesystem twice; the second time it also took the answer cache with it.
`SEARCH_MIN_FREE_GB` (default 5 GiB, `--min-free-gb` overrides, `0` disables) is now checked inside
the crawl loop via `statvfs` every 32 turns, and the run stops with `stopped_for_disk: true` in its
summary instead of continuing until `ENOSPC`. 3 tests. It takes effect when the running shards next
restart — they are still on the previous binary.

### Phase 1 — Make the corpus reachable, and fix the ranking lie (1–2 weeks)

1. **Federate the four shards** read-only (each answers in 112–230 ms standalone today; resource ids
   are globally stable, 0 mismatches in 20,000-URL samples). Gate: 518K documents reachable through
   `/search`, deduplicated by `resource_id`, p95 measured.
2. **Delete the duplicate lane**: either give source-first a real source scope or remove its weight;
   give dense a non-zero weight. Gate: on the 600 graded queries in
   `benchmarks/queries.corpus.jsonl`, nDCG@10 improves or the change is reverted.
3. **Decouple the answer**: `engine:true` must not pay an LLM call it cannot deliver; answer
   generation becomes opt-in per request. Gate: `/search` p95 within budget with answers off.

### Phase 2 — Build the derived layer, then delete the source bytes (1 week)

Per `docs/legacy/index-upgrade-plan.md`, in this order, because each source can only be deleted after the next
artifact is built from it:

1. `blobs → headings, web_link` (link edges are unrecoverable later);
2. `text → excerpt (~500 chars), vectors`;
3. verify (counts, byte-equality of excerpt vs blob text, `/read` unchanged);
4. delete `blobs` (284 GB) then `text` (13 GB).

Gate: fleet ~307 GB → ~12 GB, and the 600-query nDCG does not drop.

### Phase 3 — Sparse neural memory (the v2 core, 2–4 weeks)

1. **Embeddings for the corpus** — the true prerequisite. 8 docs/s measured: 17 h for 501K, 38 h for
   everything. Start with the published fleet.
2. **Train the Top-K sparse encoder (Model B)** on those embeddings: `z = TopK(ReLU(We+b))`,
   4,096–8,192 features, 16–32 active. The matrix is 8,192 × 384 — 12.6 MB — so training is minutes
   per epoch on CPU, not days. Gate: reconstruction is *not* the metric; retrieval quality on the
   graded set is.
3. **Sparse postings in a Tantivy index** keyed by feature id, weights as quantized boosts, reusing
   Block-WAND for candidate selection. Gate: candidate recall vs BM25 at equal candidate counts.
4. **Query encoder (Model D), cheapest version first**: project the query embedding through the same
   `W` — no new model. Gate: end-to-end p95 < 50 ms at 500K documents.

### Phase 4 — Routing and ranking (2–3 weeks)

1. Rule-based router first (query features → which experts), with a mandatory global lexical
   fallback. Gate: router never reduces recall@20 versus searching everything.
2. Lightweight ranker over bounded candidates (sparse score, dense score, concept coverage, source
   quality, freshness, direct-vs-inferred). Start from rank fusion, then compare a trained linear /
   GBDT ranker. Gate: nDCG@10 improvement over fusion, measured.

### Phase 5 — Source intelligence (1–2 weeks)

Aggregate `doc_concept` → `source_concept` (normalised by `doc_count`), populate `website` /
`source_section` from the 274,996 sitemaps and 9.05M frontier URLs, and expose `search_sources` /
`map_source`. Gate: source recall@10 on queries where the right source is known.

### Phase 6 — Compressed dense and multivector (only if measured)

Matryoshka needs an MRL teacher; adopting one multiplies embedding cost 4–8×. Do this only if Phase
3's gate shows the sparse path missing recall that dense recovers.

### Phase 7 — Scale and distribute

Only after single-node benchmarks show it is necessary. Not started, by design.

---

## 3. What each phase changes in the repository

| Phase | Crates / files |
| --- | --- |
| 0 | `crates/answer/src/client.rs` (best-effort cache), `crates/frontier` (lease recovery), deploy scripts |
| 1 | `crates/api` (multi-index `Searcher`), `crates/engine/src/fuse.rs` (weights), `crates/agent0` (answer flag) |
| 2 | `crates/agent0` (a `reparse` verb), `crates/indexer` (STOREd headings), `crates/metadata` (web_link population) |
| 3 | new `crates/features` (encoder, postings, query projection), `crates/embed` (batch embed), `crates/metadata` (feature tables) |
| 4 | `crates/api` (router + ranker wiring), `crates/engine` (score components) |
| 5 | `crates/sources`, `crates/api` (source endpoints), `crates/metadata` (`source_concept` aggregation) |

### Schema still to add

- `feature(feature_id, model_id, version, space_id, label)` — a `FeatureID` is meaningless without
  the model that produced it (v2 §4).
- `doc_feature(resource_id, feature_id, activation)` — the forward index for reranking and updates.
- `doc_concept_dim(resource_id, concept_id, dim, value, model_version)` — multidimensional `R(d,c)`
  (v2 §6) without committing to a fixed set of columns.
- Postings themselves live in the sparse Tantivy index, rebuildable from `doc_feature`.

---

## 4. Decisions this plan still needs

1. **Teacher model.** Stay on BGE-small (cheap, 8 docs/s) or move to a larger teacher (bge-m3 class)
   for better latent features at 4–8× the embedding cost. Recommendation: stay on BGE-small for
   Phase 3, revisit only if the gate fails.
2. **Sparse index: Tantivy vs custom.** Recommendation: Tantivy first, because Block-WAND, segments
   and the overlay already exist and are tested; write a custom index only if compression or
   latency proves inadequate.
3. **Feature count.** 4,096 vs 8,192. Recommendation: 4,096 first — 16.9M postings at 1.09M docs,
   half the index, and the vocabulary can grow later with a new model version.
4. **Concept vocabulary source.** OpenAlex's hierarchy covers the scholarly slice only; the rest of
   the corpus is software, docs, news and — measured — a great deal of spam. Recommendation: start
   with a few hundred hand-verified concepts plus OpenAlex's subfields for research pages.

---

## 5. Gates (the numbers that decide)

| Gate | Threshold | Why |
| --- | --- | --- |
| Answer delivered on a full disk | 1/1 | the current failure is total, not partial |
| Reachable documents | ≥500K | federation is the cheapest coverage win |
| `/search` p95, answers off | <50 ms at 500K docs | v2 §15; today's legacy path is 71–92 ms at 19K |
| nDCG@10, semantic lane vs BM25-only | strictly better on 600 graded queries | a semantic layer that does not beat lexical is not kept |
| Cross-disciplinary recall@20 | measured, reported | the oxytocin case is the acceptance test |
| Fleet storage | ≤15 GB after Phase 2 | the index-only model's whole point |
| Sparse candidate recall vs BM25 | ≥ BM25 at equal candidate count | WAND on features must not lose documents |

---

## 6. What is deliberately not in this plan

Distributed indexing, trillion-edge materialisation, transformer training from scratch, a learned
router before a rule-based one, and Matryoshka representations before an MRL teacher. Each is either
premature by the measurements above or requires hardware this host does not have.
