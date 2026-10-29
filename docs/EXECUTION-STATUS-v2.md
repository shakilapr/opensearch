# v2 execution status — 2026-10-29

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

## Phase C — storage ⏳ code done, rebuild pending

`body_boost` removed, `excerpt` (500 chars, STORED) added. `body` stays `TEXT | STORED` so ranking
and snippet generation are unchanged — verified by the existing snippet tests passing.

The schema hash changed, so **the four indices must be rebuilt** before this takes effect. That
rebuild is deliberately blocked behind Phase B: the fleet is currently the only copy of the extracted
text, and PLAN-v2 orders "build excerpt → verify → delete" for exactly this reason.

Current fleet storage: **41 GB** (tantivy 17.3, text 15.6, meta 7.2).

## Phase D — learned sparse features ❌ not started

No `crates/features`, `feature`/`doc_feature` empty, no feature postings in the schema. This is the
v2 core (§5.2/§9/§14) and cannot start until dense vectors exist, because Model B trains on them.

## Gates

| Gate | Target | Actual |
|---|---|---|
| Concept lane served | present in `lane_ranks` | **yes, 1–2 of 40** |
| Cross-disciplinary acceptance | oxytocin paper for a romance query | **fails on corpus** — 1.34M docs, 2 with "oxytocin" in the title |
| Storage ≤15 GB | after Phase C rebuild | 41 GB (rebuild pending) |
| Dense lane covers corpus | all shards have vectors | 0 of 4 (in progress) |
| nDCG@10 semantic > BM25 | strictly better on 604 graded queries | **not measured** — requires Phases B and D |

## Honest summary

Phase A is complete and serving. Phase B is running and will take about a day. Phase C's code is
committed but its effect is gated on a rebuild that must not run before Phase B finishes. Phase D —
the learned sparse memory that is v2's actual core — has not started and cannot until B lands.

The concept layer works and is now reachable, which it was not this morning. It does not yet change
rankings measurably, and the acceptance query still fails because the corpus lacks the literature,
not because the lane is wrong.
