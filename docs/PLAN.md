# Plan — what we do, in what order, and what decides it

The design is `docs/ARCHITECTURE.md`. The measurements are `docs/STATUS.md`. The delegable tasks are
`docs/WORK-QUEUE.md`. This file is only sequencing and gates.

Rules that are not negotiable:

1. **A phase that cannot show its gate is not kept.** "It is built" is not a gate; a number is.
2. **Order by dependency, not ambition.** A lane cannot be served before its table has rows; rows
   cannot be dimensional before the dimension table exists; a weight cannot move before an instrument
   can judge it.
3. **A schema change ships with its rebuild** — one operation, never two.
4. **No unmeasured tuning.** A weight, a `c`, a dimension, a threshold: measured or reverted.

---

## State

| Phase | Goal | Status |
| --- | --- | --- |
| P0 | availability: answer delivery, disk guard, lease recovery, deploy HEAD | **done** |
| P1 | federation: four shards + primary behind one query | **done** |
| P2 | derived layer: excerpt-only index, vectors, features, concepts; delete source bytes | **done except source deletion** (deliberate) and the link graph (never captured) |
| P3 | sparse neural memory: encoder, postings, forward index, query projection | **built, earns no weight** — gate not met |
| P4 | routing and ranking: rule router, feature rerank | **built, both off** |
| P5 | source intelligence: `source_concept`, website/section profiles | **not started** |
| P6 | compressed dense / multivector | **not justified** — no measurement asks for it |
| P7 | distributed index | **not justified** — 1.59M docs occupy 1.75 GB of index |

---

## P0 — availability

Gate: a query returns an answer with the disk full; `/search` works at HEAD.

Done: answer cache write is best-effort (the answer is returned regardless), `SEARCH_MIN_FREE_GB`
stops a crawl before `ENOSPC`, `agent0 frontier recover` reclaims expired leases, HEAD deployed.

---

## P1 — make the corpus reachable

Gate: ≥500K documents reachable through one `/search`, deduplicated by `resource_id`.

Done: five roots federated read-only (`SEARCH_FEDERATE_DIRS`), 1.59M documents, `resource_id` is
globally stable so no id rewriting. Source-first lane dropped from ranking (it was BM25 twice).

---

## P2 — the derived layer, then the source bytes

Gate: index ≤2 GB for the fleet; `/read` still returns full documents; nDCG does not drop.

Done: index model IM-3 — `body` removed from Tantivy, `excerpt` (500 chars) stored instead;
**19,624 MB → 1,710 MB** at 1.56M documents; `/read` reads `text/<hash>.txt`; headings/entities
persisted at ingest (migration 0009) so a rebuild is lossless for new documents.

Open: `text/` (18.2 GB) is retained — deleting it is the one-way step, and it is blocked because
pre-existing documents still lack `document_derived` rows; the link graph (`web_link`) was never
captured and raw HTML is no longer kept, so it is unrecoverable for the existing corpus.

---

## P3 — the v2 core: learned sparse memory

Gate: **candidate recall ≥ BM25 at equal candidate counts**, and end-to-end p95 within budget.
Reconstruction error is explicitly not the gate.

Done: `crates/features` (TopK encoder, postings, lane), trained encoder (`features/encoder.bin`,
283K samples), postings for all roots (180 MB), forward index (22.2M rows), query projection,
`agent0 features probe` for lane-only recall.

**Not met:** at weight 0.8 the lane costs 0.091 nDCG on the 600-case set (primary) and 0.0021 at
fleet scale; it ships at 0.0. Diagnosis: the encoder learned a body-similarity space, while the graded
labels reward matching a title's words. Next experiments, in order: train on titles as well (Q1),
IDF-weight the postings (Q2), then re-measure.

---

## P4 — routing and ranking

Gate: router never reduces recall@20 versus searching everything; rerank improves nDCG@10 on a
held-out half.

Built and measured: `SEARCH_ROUTE=1` removes 3 of 5 lane calls at the median query for −0.0007 nDCG
and −67 ms p50; `SEARCH_RERANK=1` moves the top 10 in the wrong direction with the documented prior
weights (−0.021 nDCG), so fitting the weights on a held-out half is the next step, not shipping them.

---

## P5 — source intelligence

Not started. `source_concept` is empty; there is no website/section profile. The dependency is real:
source profiles aggregate `doc_concept`, and document coverage is 32.5% with a 15-concept lexicon, so
a profile built today would describe a third of each site. Gate: source recall@10 on 50 queries whose
correct site is known.

---

## P6 / P7 — deliberately not in this plan

Matryoshka needs an MRL teacher and 4–8× the embedding cost; ANN is unnecessary while 1.39M vectors
are answered by brute force within budget; distributed indexing is unnecessary while one node holds
the corpus in gigabytes. Each becomes relevant only when a measurement says the current form misses a
gate — `docs/STATUS.md` §4 names which measurement.

---

## Gates, current values

| Gate | Target | Measured |
| --- | --- | --- |
| Reachable documents | ≥500K | **1,593,844** (`index audit`, 2026-11-02) |
| Index size, fleet | ≤2 GB | **1.75 GB** |
| Legacy `/search` p95 | <50 ms (design target at scale) | **97 ms** at 1.59M docs |
| `engine:true` p95 | <150 ms | **2,541 ms** — open (P4) |
| nDCG@10 semantic lane vs BM25 | strictly better | dense +0.0017, concept −0.0114, features −0.0910 → **not met** |
| Cross-disciplinary recall | measured | **2.6× BM25 with concept-only ranking** (0.2718 vs 0.1052, n=6) → instrument still too small to set weights |
| Sparse candidate recall vs BM25 | ≥ at equal candidates | measured by `features probe`; weight 0.0 → **not met** |
| Fleet storage ≤15 GB | after deleting `text/` | 33.6 GB with text, 15.4 GB without → **deletion unfired** |

The two structural gates — reachable documents and index size — are met. The quality gate is not, and
the honest reading is that the instrument that could judge it does not yet exist at size.

---

## Decisions this plan has taken

| Decision | Choice | Evidence |
| --- | --- | --- |
| Teacher model | BGE-small-en-v1.5 | 13.2 docs/s under crawl contention; a larger teacher costs 4–8× and no gate asks for it |
| Sparse index | Tantivy + custom postings file | sorted ids, immutable segments, overlay, block-max WAND already exist and are tested |
| Feature count | 4,096, 16 active | 22.2M rows, 180 MB — half of 8,192 and measurable |
| Concept source | hand-verified lexicon, OpenAlex for the scholarly slice | the corpus is mostly software/docs/news; a general taxonomy tagged 32% of it |
| Index identity | `resource_id` (one live doc per resource) | a rebuild once used `docver_id`, which no graded label matched |
| Fusion | RRF, `c=60` | no calibration needed; a single-lane document cannot outvote a two-lane one until weights are fitted |
