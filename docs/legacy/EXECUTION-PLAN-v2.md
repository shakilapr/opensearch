# v2 execution plan — phase by phase

Grounded in measurements taken 2026-10-29. Each phase has a gate that must pass before the next
starts; a phase that cannot show its gate is not kept.

Current state: 4 crawlers running, 1,349,717 document versions across four shards, 438 GB free.
`shard-0` is at migration 8; `shard-1/2/3` are at 7. Every semantic table is empty on every live
shard, the concept lane is not wired into serving, and `vectors.bin` exists only on the 19K
serving root.

Order is by dependency, not by ambition: a lane cannot be served before its table has rows, and
rows cannot be dimensional before the migration that creates the dimension table.

---

## Phase A — Make the semantic layer live (today, ~1 h)

The code exists and is tested, but nothing runs it. This phase turns it on.

1. **Apply migration 0008 to shards 1–3.** Additive DDL (`feature`, `doc_feature`,
   `doc_concept_dim`); no existing table changes. Runs under the schema ledger, so re-running is a
   no-op.
2. **Run `concepts build` on all four shards.** Measured 802 docs/s on a 20K slice, so ~1.35M
   documents is ~28 min of wall time across four parallel runs.
   - The crawlers hold the SQLite *writer*; the tagging pass also writes. Mitigation: small
     batches, `busy_timeout`, and a single writer per shard (no parallel runs against one root).
   - Tagging reads `text/<hash>.txt`, which the crawler writes; a document mid-ingest simply tags
     with slightly stale text and is re-tagged on the next pass (the write is idempotent).
3. **Wire `ConceptLane` into the API fusion.** `crates/api` has no reference to it today. Add it as
   a fifth lane with a configurable weight (`SEARCH_FUSION_CONCEPT`, default off so the change is
   revertible like every other fusion experiment), and report it in `lane_ranks`.

**Gate:** `POST /search {"query":"chemical reactions that affect romance"}` returns a `concept`
entry in its lane reporting, and the top result is a document whose title does not contain
"chemical" or "romance" — or the response explains, with row counts, why the corpus has no such
document.

**Known risk:** the acceptance query is contaminated by polysemy. "romance" reaches light-novel
genre pages (measured: a novelcool.com category page scored 0.60 on `romance`) and "chemical"
reaches the band My Chemical Romance. Phase A therefore also adds sense discrimination — a
`concept_alias` entry alone cannot distinguish them, so the tagger needs a negative-evidence rule
for genre markers (`novel`, `drama`, `episode`, `watch`, `lyrics`, `album`, `tour`).

---

## Phase B — Dense lane becomes real (~10–38 h, background)

`FusionPolicy` weights dense at 0.0 and the fleet has no vectors. Without this, v2 §5.1 is a lane
that cannot answer.

1. **Embed the fleet** with BGE-small-en-v1.5 at batch 32 (measured 98.1 docs/s).
   Published 501K subset: ~1.5 h. All 1.35M: ~3.8 h. Run per shard, memory-capped via
   `systemd-run --property=MemoryMax=16G` (the Hermes worker cgroup kills builds at ~4 GB).
2. **Give dense a non-zero weight only after measuring it.** `SEARCH_FUSION_DENSE` first, on the
   600 graded queries in `benchmarks/queries.corpus.jsonl`.

**Gate:** nDCG@10 on the graded set improves, or dense returns to 0.0 and the result is reported.

---

## Phase C — Shrink the index to what the architecture specifies (Phase 2 of PLAN-v2)

`body: TEXT | STORED` stores every article twice; the fleet's Tantivy indices are 16.6 GB against
a 1–2 GB target, and `text/` holds another 14.8 GB.

1. **Build the excerpt artifact first** (~500 chars) into a STORED `excerpt` field. Index
   `title + headings + excerpt`; keep `body` tokenised but not stored, or drop it once the
   excerpt proves sufficient.
2. **Redirect the three consumers** of the stored body, which are the whole reason this is not a
   one-line schema edit:
   - `doc_text` → `<data_dir>/text/<hash>.txt` (the answer path, `/read`, federation)
   - `all_docs` → export, `graphbuild`, embedding
   - `SnippetGenerator` → the stored excerpt
3. **Verify before deleting**: excerpt byte-equality against the blob text on a sample, `/read`
   unchanged, nDCG unchanged.
4. **Delete `text/`** only after 3 passes.

**Gate:** fleet storage ≤15 GB, `/read` still returns full documents, nDCG does not drop.

This phase is deliberately last among the storage work: deleting source bytes before the derived
artifact is proven is the one mistake that cannot be undone.

---

## Phase D — The v2 core: learned sparse features (multi-week, not started)

v2 §5.2/§9/§14. Nothing here exists: no `crates/features`, no feature postings in the schema,
`feature` and `doc_feature` empty.

1. `crates/features`: Model B, `z = TopK(ReLU(We+b))`, 4,096 features, 16–32 active. The matrix is
   4,096 × 384 — 6.3 MB — so training is minutes per epoch on CPU.
2. Sparse postings in a Tantivy index keyed by feature id, quantised weights, Block-WAND for
   candidate selection.
3. Model D, cheapest version: project the query embedding through the same `W`. No new model.

**Gate:** candidate recall ≥ BM25 at equal candidate counts, and end-to-end p95 < 50 ms at 500K
documents. Reconstruction error is explicitly not the metric.

---

## Phase E — Routing and ranking 3-stage (after D)

1. Rule-based router over query features, with a mandatory lexical fallback.
2. Wire `crates/rerank` (820 real lines, currently called by nothing) into the API as stage 3 over
   bounded candidates.

**Gate:** router never reduces recall@20 versus searching everything; nDCG@10 improves over
fusion, measured.

---

## Explicitly out of scope until measured

§10 Matryoshka/multivector (needs an MRL teacher, 4–8× embedding cost — only if D's gate shows
dense recovers recall the sparse path misses), §12 external associative memory, §16 distributed
partitioning. Adopting any of these before a measurement demands it repeats the mistake that made
this engine a keyword store with a semantic schema.
