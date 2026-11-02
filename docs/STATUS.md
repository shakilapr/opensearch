# What is left — the remaining work, measured

State at 2026-11-01 22:10, after the IM-3 rebuild and deployments. Every count below was read from
`/var/lib/search` and `/var/lib/search-fleet/shard-*` on this host. Task detail (steps, pitfalls,
gates) lives in `docs/WORK-QUEUE-v2.md`; this file is the list, ordered by what unblocks what.

**Done and not on this list:** excerpt-only index (1.71 GB, 1.59M documents), dense vectors,
feature matrices + trained encoder, concept matrices M3/M4, `/read` resolution, migration 0009
(`document_derived`), the architecture spec, the 600-case and body-span sets.

---

## 1. Correctness and quality debt introduced or exposed by the rebuild

| # | Item | Measured now | Why it matters | Effort |
| --- | --- | --- | --- | --- |
| R1 | **Headings/entities backfill** | 6,390 of 1,595,771 documents have a `document_derived` row (0.4%) — only documents ingested after migration 9 | the two fields boosted 2.0/3.0 are empty for 99.6% of the corpus; the ranking effect is unmeasured because the graded set is title-derived | 6 h |
| R2 | **Index vs metadata gap** | index 1,592,836 / metadata 1,595,771 (−2,935) | either textless rows or un-indexed documents; today it is checked by hand | 3 h |
| R3 | **Weight-0 lanes still execute** | `source_first` appears in `lane_ranks` despite weight 0.0; concept costs ~1.5 s/query across 5 shards | a lane that cannot change a result should not be called (`lane_hint`) | 2 h |
| R4 | **`/read` fallback is silent** | a document whose `text/<hash>.txt` is gone returns the 500-char excerpt with no signal | honesty: a degraded read must be visible to the caller, not just documented | 1 h |

## 2. Matrices that are still empty or too thin

| # | Matrix | Measured now | Blocker / next step | Effort |
| --- | --- | --- | --- | --- |
| M1 | document × concept | 885,364 rows, **32.5% document coverage**, **15 concepts** | lexicon breadth is the limit; grow to ~300 with per-concept title validation | 8 h |
| M2 | concept × dimension | 1,892,440 rows (2.14 dims/association) | dimensions must be shown independent, not three views of one body count | 3 h |
| M3 | concept × concept (typed) | **0 rows** | import OpenAlex subfields + hand-written edges; bound to 1–2 hops, mark inferred | 6 h |
| M4 | source × concept | **0 rows** | needs M1 coverage first; aggregate normalised by `doc_count`, never a weight sum | 5 h |
| M5 | page → page links (`web_link`) | **0 rows; raw HTML no longer retained** | either capture at ingest from now on (6 h) or accept the graph is gone for the existing corpus and say so | 6 h |
| M6 | passage → concept | **0 rows** | passage split at ingest; only worth doing after M1/M5 | 6 h |
| M7 | entity graph (`graph_edge`) | 130,359 rows on the **primary root only** | build on the fleet or drop the lane claim | 4 h |

## 3. Retrieval quality — every semantic lane ships at 0.0

| # | Item | Measured | Decision it needs | Effort |
| --- | --- | --- | --- | --- |
| Q1 | features lane | −0.0910 nDCG at w=0.8 (primary), −0.0021 (fleet) | retrain on **titles** as well as bodies, then re-measure | 5 h |
| Q2 | features postings | no IDF weighting | add `df` per feature and weight at query time | 3 h |
| Q3 | dense lane | +0.0017 on the 600 set, 3.7× BM25 on cross-disciplinary (n=6) | keep at 0.0 until Q6; or drop the lane | 2 h |
| Q4 | concept lane | −0.0114 on the 600 set, 3.9× on cross-disciplinary (n=6) | same: the instrument is the blocker, not the lane | — |
| Q5 | rerank / router | both off; rerank −0.021 with the prior weights | fit rerank weights on a held-out half; measure router recall@20 | 9 h |
| Q6 | **cross-disciplinary set** | **6 cases** (one case ≈ 0.017 nDCG) | grow to 50 with the screen: concept weight ≥0.8 → zero word overlap → read the text | 10 h |
| Q7 | fusion `c` | 60, never swept | sweep 20/60/120 **after** Q6 | 4 h |

Q6 blocks every weight change; Q1/Q2 are the cheapest chance of a lane that earns weight.

## 4. Latency

| # | Path | Measured | Target | Effort |
| --- | --- | --- | --- | --- |
| L1 | `engine:true` p50 / p95 | **620 ms / 2,541 ms** | <150 ms p95 | 8 h |
| L2 | legacy `/search` p50 / p95 | 34–35 ms / 97 ms | <50 ms p95 — **met** | — |
| L3 | `/read` per result | not measured at N=40 | cache path lookups; keep per-root read-only connections | 3 h |
| L4 | per-stage attribution | `trace` exists, never tabulated | produce the table before changing anything | 2 h |

## 5. Instruments the decisions depend on

| # | Item | State | Effort |
| --- | --- | --- | --- |
| I1 | body-span set run | set exists (600 cases), **never run** | 2 h |
| I2 | cross-disciplinary 50 | 6 cases | (Q6) |
| I3 | source-recall@10 | no instrument | 4 h |
| I4 | lane `probe` for recall@k | exists (`agent0 features probe`), used once | — |

## 6. Operations hygiene

| # | Item | State | Effort |
| --- | --- | --- | --- |
| O1 | systemd drop-ins | **5 files** (`answer.conf`, `federate.conf`, `fusion.conf`, `planner.conf`, `search-llm.conf`); additive, order-dependent | 2 h |
| O2 | env-drift check at boot | absent; the boot log does not state the effective `FusionPolicy` | (part of O1) |
| O3 | `agent0 index audit` | absent (R2 done by hand) | 3 h |
| O4 | `agent0 matrix report` | absent (every number in this file was a one-off script) | 4 h |
| O5 | rebuild drill + runbook | script exists, drill not executed end to end with rollback | 3 h |
| O6 | rollback storage | 8 × `tantivy.fulltext-<ts>` = **19,647 MB** held against 214 GB free | 1 h |
| O7 | `text/` deletion gate | 18.2 GB retained deliberately; the gate is "derived layer proven", which R1/M5 fail | decision, not a task |

## 7. Scale — only when a measurement asks

| # | Item | Effort |
| --- | --- | --- |
| S1 | 10M-document projection (1.10 KB/doc index + vectors) | 3 h |
| S2 | brute-force vector limit → ANN decision | 4 h |
| S3 | portable pack integrity with excerpt-only docs | 4 h |
| S4 | distributed partitioning | not planned |

---

## Order to execute

1. **R2 + O3** (audit verb) — every later gate needs a trustworthy count.
2. **R1** (backfill) — restores ranking signal the rebuild removed.
3. **M1** (lexicon) — coverage is the matrices' weakest dimension, and it unblocks M3/M4.
4. **Q1 + Q2 + I1** — the cheapest path to a semantic lane that earns a weight.
5. **Q6** — unlocks Q3/Q4/Q5/Q7.
6. **L4 + L1** — latency on the path real clients use.
7. **M5** — irreversible: decide before any future blob policy is revisited.
8. **O1/O4/O5/O6** — keeps the numbers above reproducible.
9. The rest.

Total: **≈115 hours**, of which ≈35 h is measurement and 80 h is build.

## What is blocked, and on what

| Blocked | On |
| --- | --- |
| any fusion weight change (Q3/Q4/Q5/Q7) | Q6 (50-case cross-disciplinary set) |
| source profiles (M4) | M1 coverage |
| passage layer (M6) | M1 + M5 |
| deleting `text/` (O7) | R1 (headings not recoverable from text) and a decision that excerpts suffice |
| ANN / distributed (S2/S4) | a measurement showing brute force misses the budget |

## What is *not* work

- Restoring `body` to the index: it would undo a measured 11.5× reduction and the architecture's core
  claim. If a feature needs full text, read `text/<hash>.txt` or batch it into a derived artifact.
- Raising a semantic weight because a lane is new or expensive to build. Ten of the eleven measurements
  in this repository say the weight is 0.0; only a better instrument changes that.