# Status — what is measured, and what is left

Updated 2026-11-02. Every number is from this host; the commands that produced them are in
`docs/BENCHMARKS.md`. Design: `docs/ARCHITECTURE.md`. Plan and gates: `docs/PLAN.md`.

---

## 1. Where the system is

| Fact | Value |
| --- | --- |
| Indexed documents | 1,593,844 (5 roots; `index audit` ratio ≥0.998 per shard) |
| Resources in metadata | 1,595,771 |
| Index size, all roots | 1.75 GB (1.10 KB/doc) |
| Extracted text on disk | 18.2 GB (the ingest source; not deleted) |
| Metadata | 11.6 GB |
| Dense vectors | 2.14 GB, 1,387,584 documents, 384-d |
| Sparse features | 22,201,352 rows, 4,096 features, 16 active/doc, 180 MB postings |
| Concept associations | 885,364 rows over 506,363 documents (**32.5%**), 15 concepts |
| Concept dimensions | 1,892,440 rows (2.14 per association) |
| Concept graph / source profiles / links / passages | **0 rows each** |
| Entity graph | 130,359 rows, primary root only |
| Migration ledger | 9 on every root |
| Legacy `/search` | p50 34 ms, p95 97 ms |
| `engine:true` | p50 620 ms, p95 2,541 ms |

The corpus is crawled and searchable. The semantic layer is built and, measured, not yet worth a
fusion weight. That distinction is the whole of the current state.

---

## 2. Experiments that decided something

600-case corpus-derived set (query = the document's own title), Fast mode, one process per config:

| Configuration | nDCG@10 | Δ |
| --- | --- | --- |
| federated BM25 baseline | 0.2481 | — |
| dense w=0.8 | +0.0017 (primary-root run 0.4830 vs 0.4813) | noise |
| graph w=0.8 | ±0.0000 | empty on the fleet |
| concept w=0.8 | −0.0114 (monotonic in weight) | harm |
| features w=0.8 | −0.0021 fleet / −0.0910 primary | harm |
| features only, no lexical lanes | 0.0677 | the lane cannot stand alone |

Concept-first ranking (corpus-derived relatedness graph, `agent0 concepts relate`; 2026-11-02):

| Set | Ranking | nDCG@10 | recall@10 |
| --- | --- | --- | --- |
| 600 corpus-derived | BM25 | 0.2112 | 0.4200 |
| 600 corpus-derived | concept-only | 0.1328 | 0.2567 |
| cross-disciplinary (n=6) | BM25 | 0.1052 | 0.1667 |
| cross-disciplinary (n=6) | **concept-only** | **0.2718** | **0.3333** |

Cross-disciplinary set — query shares no content word with the answer's title (n=6, lanes at weight
0.8 rather than concept-only):

| Lane | nDCG@10 | vs BM25 |
| --- | --- | --- |
| BM25 | 0.0645 | — |
| concept | **0.2500** | 3.9× |
| dense | 0.2384 | 3.7× |

Both tables are true. The title-derived set cannot represent cross-disciplinary retrieval, and the
cross-disciplinary set is too small (one case ≈ 0.017 nDCG) to set a weight. That is why every
semantic lane ships at 0.0 — and why growing the second instrument is the top quality task.

Routing and rerank (built, measured, off):

| Configuration | nDCG@10 | p50 | lane calls p50/p95 |
| --- | --- | --- | --- |
| `SEARCH_ROUTE` off | 0.4830 | 222 ms | 5 / 5 |
| `SEARCH_ROUTE=1` | 0.4824 | 155 ms | 2 / 4 |
| `SEARCH_RERANK=1` | 0.4600 | 158 ms | — (prior weights, unfitted) |

---

## 3. Defects found and fixed in this cycle

| Defect | Effect | Fix |
| --- | --- | --- |
| Rebuild used `docver_id` as `doc_id` | index identities became `res_x:1` while the crawler writes `res_x`; graded labels (resource ids) matched nothing, nDCG read **0.0**, `/read` 404'd | `indexbuild` writes `resource_id`; rebuild re-run |
| A rebuilt index had no manifest | `/health` reported generation 0 = `index_unpublished` for 1.59M documents | `Pipeline::publish_index`, called by `index build`, exposed as `agent0 index publish` |
| `/read` compared only `resource_id` | 404 for every id `/search` returned | accepts either id, prefers the exact version |
| `/read` used the primary metadata only | a federated result was never found | deep searcher consults every served root |
| Schema change without rebuild | every index unopenable (the earlier revert) | schema + rebuild are one scripted operation |
| Rebuild cannot reproduce headings/entities | 99.6% of documents lost the 2.0/3.0-boosted fields | migration 0009 `document_derived`, written at ingest |
| Index stored the article | 12.7 KB/doc, 19.6 GB | IM-3: excerpt-only, 1.10 KB/doc, 1.75 GB |

---

## 4. What is left

Ordered by what unblocks what. Task detail in `docs/WORK-QUEUE.md`.

### 4.1 Correctness debt from the rebuild

| # | Item | Measured now | Why it matters | Effort |
| --- | --- | --- | --- | --- |
| R1 | Headings/entities backfill | 6,390 of 1,595,771 documents have a `document_derived` row (0.4%) — only documents ingested after migration 9 | the two fields boosted 2.0/3.0 are empty for 99.6% of the corpus; the ranking effect is unmeasured because the graded set is title-derived | 6 h |
| R2 | Index vs metadata gap | index 1,592,836 / metadata 1,595,771 (−2,935) | either textless rows or un-indexed documents; checked by hand today | 3 h |
| R3 | Weight-0 lanes still execute | `source_first` appears in `lane_ranks` despite weight 0.0; concept costs ~1.5 s/query across 5 shards | a lane that cannot change a result should not be called | 2 h |
| R4 | `/read` fallback was silent | now logged (`doc_text: source text absent, returning the indexed excerpt`) with doc id and length; the API still has no `source` field, so a client cannot see the degradation | surface it as `source: text|excerpt` in `DocResponse` | 1 h |

### 4.2 Matrices still empty or too thin

| # | Matrix | Measured now | Blocker / next step | Effort |
| --- | --- | --- | --- | --- |
| M1 | document × concept | 885,364 rows, **32.5% coverage, 15 concepts** | lexicon breadth is the limit; grow to ~300 with per-concept title validation | 8 h |
| M2 | concept × dimension | 1,892,440 rows (2.14 per association) | dimensions must be shown independent, not three views of one count | 3 h |
| M3 | concept × concept (typed) | **0 rows** | OpenAlex subfields + hand-written edges; 1–2 hops, marked inferred | 6 h |
| M4 | source × concept | **0 rows** | needs M1; aggregate normalised by `doc_count`, never a weight sum | 5 h |
| M5 | page → page links | **0 rows; raw HTML no longer retained** | capture at ingest from now on, or accept the loss for the existing corpus | 6 h |
| M6 | passage → concept | **0 rows** | passage split at ingest; after M1/M5 | 6 h |
| M7 | entity graph | 130,359 rows, **primary root only** | build on the fleet or drop the lane claim | 4 h |

### 4.3 Retrieval quality — every semantic lane ships at 0.0

| # | Item | Measured | Decision it needs | Effort |
| --- | --- | --- | --- | --- |
| Q1 | features lane | −0.0910 at w=0.8 (primary), −0.0021 (fleet) | retrain on **titles** as well as bodies | 5 h |
| Q2 | features postings | no IDF weighting | add `df` per feature, weight at query time | 3 h |
| Q3 | dense lane | +0.0017; 3.7× BM25 cross-disciplinary (n=6) | keep 0.0 until Q6, or drop | 2 h |
| Q4 | concept lane | −0.0114; 3.9× cross-disciplinary (n=6) | the instrument is the blocker, not the lane | — |
| Q5 | rerank / router | both off; rerank −0.021 with prior weights | fit rerank on a held-out half; router recall@20 | 9 h |
| Q6 | cross-disciplinary set | **6 cases** (one ≈ 0.017 nDCG) | grow to 50: concept weight ≥0.8 → zero word overlap → read the text | 10 h |
| Q7 | fusion `c` | 60, never swept | sweep 20/60/120 after Q6 | 4 h |

### 4.4 Latency

| # | Path | Measured | Target | Effort |
| --- | --- | --- | --- | --- |
| L1 | `engine:true` p50 / p95 | **620 ms / 2,541 ms** | <150 ms p95 | 8 h |
| L2 | legacy `/search` p50 / p95 | 34 / 97 ms | <50 ms p95 — **met** | — |
| L3 | `/read` per result | not measured at N=40 | cache path lookups, per-root connections | 3 h |
| L4 | per-stage attribution | `trace` exists, never tabulated | the table before any change | 2 h |

### 4.5 Instruments

| # | Item | State | Effort |
| --- | --- | --- | --- |
| I1 | body-span set run | 600 cases exist, run pending | 2 h |
| I2 | cross-disciplinary 50 | 6 cases | (= Q6) |
| I3 | source recall@10 | no instrument | 4 h |
| I4 | lane recall probe | `agent0 features probe` exists | — |

### 4.6 Operations

| # | Item | State | Effort |
| --- | --- | --- | --- |
| O1 | systemd drop-ins | 5 additive files (`answer`, `federate`, `fusion`, `planner`, `search-llm`), order-dependent | 2 h |
| O2 | boot-time env audit | absent; the boot log does not state the effective `FusionPolicy` | (= O1) |
| O3 | `agent0 index audit` | **done** (2026-11-02) — prints index vs expected per root, exits non-zero below 95% | — |
| O4 | `agent0 matrix report` | absent (every number here was a one-off script) | 4 h |
| O5 | rebuild drill + runbook | script exists, drill not executed with rollback | 3 h |
| O6 | rollback storage | 8 × `tantivy.fulltext-<ts>` = 19,647 MB | 1 h |
| O7 | `text/` deletion gate | 18.2 GB retained; gate blocked by R1 | decision |

### 4.7 Scale — only when a measurement asks

| # | Item | Effort |
| --- | --- | --- |
| S1 | 10M-document projection | 3 h |
| S2 | brute-force vector limit → ANN decision | 4 h |
| S3 | portable pack integrity | 4 h |
| S4 | distributed partitioning | not planned |

---

## 5. Order to execute

1. **R2 + O3** (audit verb) — every later gate needs a trustworthy count.
2. **R1** (backfill) — restores ranking signal the rebuild removed.
3. **M1** (lexicon) — coverage is the matrices' weakest dimension; unblocks M3/M4.
4. **Q1 + Q2 + I1** — cheapest path to a semantic lane that earns a weight.
5. **Q6** — unlocks Q3/Q4/Q5/Q7.
6. **L4 + L1** — latency on the path real clients use.
7. **M5** — irreversible: decide before any blob policy is revisited.
8. **O1/O4/O5/O6** — keeps the numbers above reproducible.
9. The rest.

**Total ≈115 hours**, of which ≈35 h is measurement and 80 h is build.

## 6. Blocked, and on what

| Blocked | On |
| --- | --- |
| any fusion weight change (Q3/Q4/Q5/Q7) | Q6 |
| source profiles (M4) | M1 |
| passage layer (M6) | M1 + M5 |
| deleting `text/` (O7) | R1, and a decision that excerpts suffice |
| ANN / distributed (S2/S4) | a measurement showing brute force misses budget |

## 7. What is *not* work

- Restoring `body` to the index: it would undo a measured 11.5× reduction and the architecture's core
  claim. A feature that needs full text reads `text/<hash>.txt` or batches a derived artifact.
- Raising a semantic weight because a lane is new or expensive to build. Eleven measurements in this
  repository say 0.0; only a better instrument changes that.
