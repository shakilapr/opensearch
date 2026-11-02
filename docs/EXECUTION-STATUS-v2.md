# v2 execution status — 2026-11-01

Reporting what is measured, not what is planned. Every number below came from this host.

## Phase A — concept layer live ✅

| Item | State |
|---|---|
| Migration 0008 on all four shards | **done** (shards 1–3 were at 7, now 8) |
| `concepts build` over the fleet | **done** — 491,929 docs tagged |
| `doc_concept` rows | **855,519** |
| `doc_concept_dim` rows | **1,828,002** |
| Tagging rate | **749–802 docs/s** per shard (282 s for shard-3's 87,840 docs) |
| Concept lane served | **yes** — `shards=5 of=5` in the serving log |
| Lane in live results | **1–2 of 40** results carry `concept` in `lane_ranks` |

Four defects had to be fixed to get here, each found by measurement:

1. **The lane was unreachable.** `grep` found no reference to `ConceptLane` outside its own crate;
   `extra_lanes` existed but nothing built one, and it only runs when `req.engine` is true.
2. **Lane-only candidates were dropped.** The engine built its `docmap` from the two *lexical* lanes'
   retained hits, so any document a semantic lane contributed vanished in a `filter_map`. No extra
   lane could ever extend the candidate set — this also silently disabled the dense and graph lanes'
   ability to add anything. Fixed with `Searcher::lookup_doc` + `Indexer::doc_by_id`.
3. **Pagination restarted.** `concepts build` advanced its cursor to `resource_id` while filtering
   on `docver_id`. Progress stalled at ~60K docs/25 min because pages overlapped.
4. **The lane only looked at the primary root**, which has no associations; the matrix is on the
   fleet. Fixed with `FederatedConceptLane`.

### The remaining limitation is arithmetic, not a bug

RRF at `c=60` gives a lane `1/(60+rank)`. A document found by the concept lane alone scores 0.0138
at rank 1; one found by both lexical lanes scores 0.0303. The concept lane therefore **re-ranks
rather than retrieves** until `c` or the weights are tuned. Tuning that by guess would be exactly
the unmeasured change `PLAN-v2.md` forbids, so it waits for the graded set.

### Sense discrimination added

Two contaminating senses were measured, not guessed:
- a light-novel category page scored **0.60 on `romance`**
- "My Chemical Romance announce 2026 tour" scored **0.49 on `chemistry`**

Pages carrying ≥2 markers of the other sense are scaled to 0.15. Two tests assert the band page and
the genre listing now rank far below real articles.

## Phase B — dense vectors ⏳ running

Embedding every shard, serial and CPU-pinned (`vecbuild.service`, 600% quota) so the four crawlers
keep their cores.

Measured on this host: **13.2 docs/s** at 2000 chars, **21.7** at 300 chars — not the 98 docs/s
previously recorded, because four crawlers are competing for the same 16 cores. At 13.2 docs/s the
1.35M-document fleet is **~28 hours**; pinned serial, expect longer.

`vectors.bin` is not yet present on any shard, so the dense lane still serves only the 19K primary
root.

## Phase C — storage ✅ done (2026-11-01, index model IM-3)

The first attempt (48198c4) was reverted (8655408) because it changed the schema without rebuilding,
leaving every index unopenable. It is now done as one operation.

`body` and `body_boost` are **gone from the schema**; `excerpt` (500 chars, STORED) replaces them,
`headings`/`doc_type`/`language` are STORED, and the article is not in the index at all. `/read`
reads `text/<hash>.txt` (resolving both `docver_id` and `resource_id`, across every served root);
`Pipeline::all_docs` hydrates full text for export/embed/graph and counts the documents it cannot.

| | Before | After |
|---|---|---|
| Tantivy, all 5 roots | 19,624 MB | **1,710 MB** |
| Per document | 12.7 KB | **1.10 KB** |
| Serving root | 315 MB | 20 MB |
| Indexed documents | 1.54M | 1,557,504 (0 missing text) |
| Legacy `/search` p50 / p95 at 1.56M docs | 71–92 ms at 19K docs | **35 / 97 ms** |

Rebuild cost: 10.4 min for 1.54M documents (~1,850 docs/s per shard). `text/` (17.9 GB) is still on
disk and is still the ingest source; deleting it is the separate gated step in PLAN-v2 Phase 2.

## Phase D — learned sparse features ✅ built, measured, weight 0.0

Built as v2 §5.2 specifies: `z = TopK(ReLU(Wx + b))`, 4096 features, 16 active, tied decoder
`x̂ = Wᵀz`, so the whole model is `W` (4096×384 = 6.0 MiB) plus `b` and trains on CPU.

| Item | State |
|---|---|
| `crates/features` (encoder, postings, lane) | **done** |
| Trained encoder | **`/var/lib/search/features/encoder.bin`**, 282,983 samples, 4 epochs, 775 s |
| Live features after training | **1,930 of 4,096 (47.1%)** on the fleet sample; 3,810 of 4,096 on the 19K primary root |
| Postings, all five roots | **done** — 1.39M documents, 16.0 postings/doc |
| `doc_feature` rows | **22.2M** (primary 308,080; shards 5.1M / 6.6M / 6.8M / 3.4M) |
| Resource coverage | **100%** primary, **87.6–94.0%** per shard (the rest have no vector) |
| Lane reachable from serving and eval | **yes** — `build_features_lane`, `--lanes features` |

Two defects had to be fixed, both found by measurement:

1. **The build died on memory.** The first fleet build was killed at 4.1 GB RSS (cgroup OOM) because
   `SparseIndex::build` held every vector beside the store it came from. Rewritten to stream: sort by
   id, then encode and write 8,192-document chunks with one transaction per chunk. Peak 0.7–1.1 GB,
   29–154 s per shard.
2. **The forward index keyed on the wrong id.** A fleet vector is `res_<hash>:<chunk>` while
   `resource.resource_id` is bare, so the `WHERE EXISTS` guard silently dropped most rows — shard-1
   landed **27,815 of 411,815** documents. The mirror now normalises to the base id and merges chunks
   by max activation: **411,783 of 411,815**.

### The measurement decides the weight, and it says zero

600 graded queries, Fast mode, one process per configuration, primary root:

| config | nDCG@10 | Δ vs baseline | recall@10 | MRR | p50 |
|---|---|---|---|---|---|
| bm25_only (baseline) | 0.4813 | — | 0.4983 | 0.4757 | 156 ms |
| features, weight 0.0 (lane built) | 0.4813 | +0.0000 | 0.4983 | 0.4757 | 172 ms |
| features, weight 0.4 | 0.4221 | **−0.0592** | 0.4983 | 0.3974 | 177 ms |
| features, weight 0.8 | 0.3903 | **−0.0910** | 0.4817 | 0.3611 | 175 ms |
| features only (no lexical lanes) | 0.0677 | −0.4136 | 0.1000 | 0.0575 | 204 ms |

The lane is wired and returns documents — the weight-0 run reproduces the baseline exactly while
costing 16 ms, which is what proves the 0.4/0.8 runs are the lane's doing and not noise. It simply
does not rank well yet on this set, whose labels are title→document: BM25 answers a title query by
scoring its words, while the lane has never seen the title tokens and cannot.

### The same measurement at fleet scale changes the size of the answer, not its sign

`eval run` used to open only the primary root even with `SEARCH_FEDERATE_DIRS` set, so every number
above described 1.4% of the served corpus. Fixed (`evalrun` now federates exactly as `serve` does)
and re-measured on **1,512,850 documents**:

| config | nDCG@10 | Δ | recall@10 | MRR | p50 |
|---|---|---|---|---|---|
| federated lexical baseline | 0.2481 | — | 0.4450 | 0.1870 | 329 ms |
| federated + features w=0.8 | 0.2460 | **−0.0021** | 0.4467 | 0.1843 | 386 ms |

Two things this measurement says. First, at fleet scale the lane costs 0.002 nDCG instead of 0.091 —
it is close to neutral rather than destructive, so the primary-root number was overstating the harm.
Second, the *baseline itself* falls from 0.4813 to 0.2481 nDCG@10 when the corpus grows 78×: the
graded set's labels are a document's own title, which a 19K-document corpus answers almost by
construction, and 1.5M documents do not. The 600-query set therefore cannot rank semantic lanes
fairly until it is rebuilt against the delivered corpus.

**`SEARCH_FUSION_FEATURES` ships at 0.0** — the plan's rule is that the measurement sets the weight,
and neither scale shows a gain. What would change it: training Model B on titles as well as bodies,
IDF-weighting the postings, and a graded set whose queries are not near-copies of the target title.

## Phase E — routing and rerank ✅ behind flags

`SEARCH_ROUTE=1` enables a rule-based sparse-expert router (`crates/engine/src/route.rs`): the lexical
expert is **never** removed, every other expert is additive and skippable, and the router is off
unless asked, so the default path is byte-identical. `SEARCH_RERANK=1` enables a stage-3 feature
rerank over the top 50 fused candidates. Both default off and both are gated by tests
(`crates/engine/tests/routing.rs`, `crates/engine/tests/rerank.rs`).

### Routing, measured (600 queries, `--lanes all`, dense weight 0.8)

| | nDCG@10 | MRR | recall@10 | p50 | searcher calls p50 / p95 |
|---|---|---|---|---|---|
| `SEARCH_ROUTE` off | 0.4830 | 0.4794 | 0.4933 | 222 ms | **5 / 5** |
| `SEARCH_ROUTE=1` | 0.4824 | 0.4776 | 0.4967 | 155 ms | **2 / 4** |

The router removes **three of five lane calls** on the median query and 30% of p50 latency
(67 ms) for **0.0007 nDCG** and 0.0018 MRR. p95 calls of 4 show the fallback path working: queries
the router cannot classify still reach nearly every lane rather than being forced down one branch.
It ships off because a 0.0007 quality cost has to be chosen deliberately, not inherited — turning it
on is a one-variable decision (`SEARCH_ROUTE=1`) once a workload's latency budget needs it.

### Rerank, measured (600 queries, primary root, `--lanes none`)

| | nDCG@10 | MRR | recall@10 | p50 |
|---|---|---|---|---|
| `SEARCH_RERANK` off | 0.4813 | 0.4757 | 0.4983 | 156 ms |
| `SEARCH_RERANK=1` | 0.4600 | 0.4468 | 0.4983 | 158 ms |

Recall is identical, which is the expected shape for a reranker: it moved no documents in or out of
the top 10, only their order — and moved them the wrong way. The weights it used are
`DEFAULT_LLM_WEIGHTS`, a documented untuned prior, so this is a measurement of the prior and not of
reranking as an idea. It stays off; the next step is fitting those weights on a held-out half of the
graded set and measuring the other half.

## Graded-set measurements (600 queries, Fast mode, one process per config)

| config | nDCG@10 | Δ vs baseline | recall@10 | MRR | p50 |
|---|---|---|---|---|---|
| bm25_only (baseline) | 0.4813 | — | 0.4983 | 0.4757 | 166 ms |
| dense_only (w=0.8) | 0.4830 | **+0.0017** | 0.4933 | 0.4794 | 206 ms |
| graph_only (w=0.8) | 0.4813 | +0.0000 | 0.4983 | 0.4757 | 305 ms |
| concept_only (w=0.8) | 0.4699 | **−0.0114** | 0.4983 | 0.4604 | 170 ms |

The concept lane's harm is monotonic in its weight, so it is not a tuning artifact:

| weight | nDCG@10 |
|---|---|
| 0.0 | 0.4813 |
| 0.1 | 0.4801 |
| 0.2 | 0.4767 |
| 0.3 | 0.4766 |
| 0.8 | 0.4699 |

**Decision: `SEARCH_FUSION_CONCEPT` stays 0.0.** That is now a measured decision, not caution.

**Why it fails here, and why that is not the whole story.** The graded cases are constructed as
"This document's title is the query" — one known-relevant document per case. A lexical lane is close
to optimal for that shape, and a concept lane that scores semantic overlap displaces exact-title
matches with conceptually adjacent pages. So this set *cannot* reward the capability the concept
lane exists for (find the oxytocin paper for "chemical reactions that affect romance"), because no
such case is in it. The instrument the plan names is the wrong instrument for this specific gate.

The lane needs a hand-judged cross-disciplinary set before it can be judged at all.

## Cross-disciplinary measurement — the result the other set could not see

The corpus-derived set is built as "this document's title is the query", so lexical search is
near-optimal on it by construction. The new set is the opposite shape: a conceptual query whose
answer shares no content word with its title.

| lane | nDCG@10 | recall@10 | vs BM25 |
|---|---|---|---|
| BM25 | 0.0645 | 0.1667 | — |
| **concept** | **0.2500** | **0.3333** | **+0.1855 (3.9×)** |
| dense | 0.2384 | 0.3333 | +0.1739 (3.7×) |

The semantic lanes are 3.9× better on the shape the architecture exists for, and were noise-to-harm
on the set that cannot represent that shape. Both facts are true; only the first justifies the
architecture.

Caveat: n=6. Enough to establish that the lanes do something lexical cannot, not enough to move a
fusion weight — one case shifts nDCG by ~0.017. Needs ~50 cases.

## Gates

| Gate | Target | Actual |
|---|---|---|
| Concept lane served | present in `lane_ranks` | **yes, 1–2 of 40** |
| Cross-disciplinary acceptance | oxytocin paper for a romance query | **fails on corpus** — 1.34M docs, 2 with "oxytocin" in the title |
| Storage ≤15 GB | after Phase C rebuild | 41 GB (rebuild pending) |
| Dense lane covers corpus | all shards have vectors | **4 of 4** (1.39M documents) |
| nDCG@10 semantic > BM25 | strictly better | **measured: dense +0.0017, graph 0.0000, concept −0.0114, features −0.0910 at w=0.8** |

## Honest summary

Phase A is complete and serving. Phase B is complete: 1,387,749 fleet documents have vectors.
Phase C's code is committed and its rebuild is still pending. Phase D — the learned sparse memory
that is v2's actual core — is built end to end: encoder trained, postings written for all five roots,
`doc_feature` mirrored for 1.39M documents, and the lane selectable from serving and eval.

Every semantic lane now exists and none of them yet beats BM25 on the graded set. The features lane
is the clearest case: at weight 0.8 it costs 0.091 nDCG, and it ships at 0.0. That is not a failure
of the architecture, it is the first honest measurement of it — and it names the next task precisely:
the encoder has learned a document-similarity space from body text, while the graded labels reward
matching a title's words. Training on titles too is the cheapest experiment that could move it.
