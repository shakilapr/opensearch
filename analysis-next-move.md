# Analysis of `next-move.md` — checked against the running system

**Measured 2026-10-24 on the serving host.** Every number here was read from the running
service, the four fleet data roots, or the repository — not from the proposal under review.
Where the proposal and the measurement disagree, the measurement is quoted.

The question this file answers: *is `next-move.md` a sound next move for this system, given what
the system actually is today?*

---

## 1. Verdict

**The strategic argument is right; three of its premises are wrong, and those three change the
order of work.**

Right: the engine's distinguishing capability should be *knowing where information is and going
to get it*, not *holding a copy of everything*. Right: federation of the four existing shards is
the fastest route to coverage, and it is technically ready. Right: measure before reweighting.

Wrong, and consequential:

1. **It believes the "source-first" lane works.** It does not. On every query measured, the
   source-first lane returns the *identical ranking* to BM25, so its 0.8 weight multiplies every
   score by a constant and changes no order. The proposal treats this lane as a real signal to be
   down-weighted from 0.8 to 0.4. There is nothing there to down-weight.
2. **It believes the storage problem is "a large traditional document index".** The index is
   **6.0 GB** for 501K documents. The **284 GB** is raw HTML blobs, and **47 GB** of that data is
   already duplicated a second time in `raw-dataset/`. The proposal's own principle ("you do not
   need full HTML for every URL") is correct and points at a 284 GB lever the proposal never names.
3. **It believes source knowledge is the project's "strongest element".** The source registry is
   an empty shell: **190,851 `source_profile` rows across the fleet, zero with `topics`,
   zero with `entities`, zero with `url_patterns`.** The proposal builds its entire centrepiece
   (Source Intelligence Layer, hierarchical retrieval) on data that does not exist yet.

One more thing it misses entirely: **every `engine: true` query today pays a model call and then
throws the answer away**, which is why engine latency is 5–47 s against a 200 ms–3 s budget.
That is not a Phase-1 nicety; it is the current user-facing behaviour.

---

## 2. What the proposal gets right — verified

| Claim in `next-move.md` | Check | Result |
| --- | --- | --- |
| Serving index is 3.84% of the fleet; 26× gap | 19,255 vs 501,161 manifest docs | **26.03×, 3.84%** ✓ |
| Dense and graph lanes contribute nothing to ranking | `FusionPolicy::default()` weights | BM25 1.0, SourceFirst 0.8, dense/graph **absent → 0.0** ✓ |
| The navigation policy has no operational transport | `crates/nav` + `ProviderPolicy` | Only `ModelClient` fakes; no HTTP transport ✓ |
| Evidence checking is token-based, not constraint-aware | `TextVerifier` | Unit supported when its tokens appear in the body ✓ |
| A real reranker is absent from production | `search-rerank` in `Cargo.toml` files | Depended on by `bench` and `nav` only — **not** by `api`/`agent0` ✓ |
| No OCR queue | `ocr_decision` consumers | Only `crates/pdf/tests/` — no runtime caller ✓ |
| LLM structuring of "every page" is expensive | 45,786 structured records | 4.2% of 1,090,184 resources — already selective ✓ |
| Federation is the cheap way to coverage | each shard opened read-only | shard 0–3 answered the same query in **112–230 ms** standalone ✓ |
| "Do not add raw BM25 scores from separate indexes" | `fuse()` is rank-based | already rank fusion; no score mixing ✓ |
| Embeddings are cheap to store | 501,161 × 384 × 4 | **734 MB**, not the ~770 MB quoted — close enough ✓ |
| The full-scan cost grows linearly | 19,255 → 192.45M dims at 501K | arithmetic correct ✓ |

---

## 3. Where the proposal is wrong, with evidence

### 3.1 "Source-first" is not a source lane — it is BM25 counted twice

`crates/api/src/engine.rs` builds both index lanes over the **same** `Searcher`, with a **shared
request-scoped cache keyed on `(query, limit, source_id)`**, and calls both with `source_id = None`
in round 1. The second lane therefore returns the first lane's cached hits, stamped with a
different lane name.

Measured on 8 queries × 8 results: `bm25` rank == `source_first` rank for **64 of 64 results**.
The fusion arithmetic then reduces to

```
score(d) = 1.0/(60+rank) + 0.8/(60+rank) = 1.8/(60+rank)
```

which is a monotone transform of BM25 rank. Live confirmation: the top result scored
`0.02950819581747055`, and `1.8/61 = 0.0295081967`.

**Consequence:** served ordering today is *pure BM25*. Not "lexical with a weak semantic
contribution" — literally BM25, with three decorative lanes attached. Both "fusion weight" rows in
the proposal's table (source-first 0.8 → 0.4, dense 0.0 → 0.7) are describing a system that does not
rank the way the table implies.

### 3.2 The storage critique targets the wrong object

| Store | Size | Per published doc |
| --- | --- | --- |
| raw HTML blobs (`shard-*/blobs`) | **284 GB** | 594 KB |
| extracted text (`shard-*/text`) | 13 GB | 27 KB |
| Tantivy index (`shard-*/tantivy`) | **6.0 GB** | 12.6 KB |
| metadata SQLite (`shard-*/meta`) | 4.8 GB | 10 KB |

The index the proposal wants to stop building is 1.9% of the storage. The raw cache is 92%.

Two measured levers the proposal does not mention:

* **`raw-dataset/` duplicates the fleet.** 47 GB of gzipped HTML whose SHA-256 hashes are
  **100% present** in the fleet's own `content.exact_hash` column (174,219/174,219 on shard 0,
  333,480/333,480 on shard 2). It is a second copy of pages already held.
* **The raw blobs compress ~5.4×.** On a 40-file sample: 10.94 MB raw → 2.02 MB gzip. Applied to
  284 GB that is **~52 GB** for the same information, if raw HTML is worth keeping at all.

Together with build artifacts (`target` 87 GB, `target-main` 76 GB, `target-browser` 4 GB) and
`opencode.db` 4 GB, **~200 GB is recoverable without deleting a single document**, well beyond the
proposal's 100 GB target.

### 3.3 The source registry is empty, so the centrepiece does not exist yet

```
fleet source_profile rows : 190,851
  with topics    != []    : 0
  with entities  != []    : 0
  with url_patterns != [] : 0
serving source_profile    : 249 rows, same zeros
```

The API knows this and says so in a comment (`crates/api/src/query.rs:213`: *"Stage 1 has no
populated `SourceProfile` registry in the API's dependency"*). What the API calls
`source_authority` is a hostname heuristic: `0.5 + 0.1 (https) + 0.1 (host) + 0.15 (.edu/.gov/.org)`,
clamped at 0.98.

So "expand the source registry substantially" is not an expansion — it is the first construction.
The good news the proposal should have found: the raw material is all there.

```
sitemaps discovered across fleet : 274,996
feeds discovered across fleet    :  18,061
distinct domains with resources  : 129,153
frontier URLs already known      : 9,050,755 over 338,609 domains
URLs with a stored title         : 369,506 (shard 2 alone)
```

That is most of the L1 URL catalog and much of the L0 navigation graph, already crawled and paid
for. The proposal's Phase 4 (URL catalog) is closer to *extraction* than to *collection*.

### 3.4 Engine latency is dominated by a model call it discards

Measured on the live service, `POST /search`:

| Request | Wall time |
| --- | --- |
| legacy path (`engine` absent) | **71–92 ms** |
| `engine:true, mode:fast` (200 ms budget) | **5,408–6,892 ms** |
| `engine:true, mode:auto` (3 s budget) | **5,636–16,662 ms** |
| `engine:true, mode:deep` (20 s budget) | 8,599 ms |
| worst observed | **46,667 ms** |

The cause is visible in the response trace on every single query:

```
answer failed: io error: No space left on device (os error 28)
llm: openrouter nvidia/nemotron-3-ultra-550b-a55b:free ok
llm: tokens in=1385 out=400 cached=0
```

The model is called, billed, and the answer is dropped because the cache write is fallible and
`?`-propagated. So "auto mode can exceed its 3 s budget" (the proposal's §5) understates it: auto
mode exceeds it by 2–5× on a **successful** model call, and the budget was never a bound on wall
time in the first place because the answer stage runs after the deadline check.

### 3.5 The "589,023 unrepresented resources" is a real backlog, and its state is not recorded

The proposal is right to demand a status audit. The audit is worse than it expects:

```
resource.status across all four shards : NEW  (1,090,184 of 1,090,184)
fetch_observation.http_status          : 200 × 180,476, 202 × 11  (shard 0)
document_version rows with text_path   : 1,090,819 — 100%
documents in a published index         : 501,247
```

Every resource claims `NEW`, including ones that were fetched, parsed, indexed and are being
served. The status column carries no information. The truth is in `ingest_op` (181,132 rows on
shard 0) and `ingest_event` (1,258,255 rows) — an event log the audit must join against, not a
state machine to read.

Two hard numbers the proposal should use instead of its estimate:

* **589,572 documents have extracted text on disk and are not in any published index.** The text
  files exist — a 200-file sample per shard found 200/200 present.
* **Why they are not published is not recorded anywhere I can verify.** Shard 2 has 371,071
  document versions but its index holds 124,677 documents across 6 segments with **0 deleted
  documents**. The gap is not tombstones and not superseded versions (only 618 resources in the
  whole fleet have more than one version). This needs the audit before any backfill job is
  designed.

### 3.6 Coverage arithmetic the proposal should adopt

```
fleet published (manifest)            : 501,161
serving index                         :  19,255
serving ∩ fleet (by resource_id)      :   2,224
reachable today                       :  19,255
reachable after federation            : 501,161 + 17,031 = 518,192   (26.9× today)
distinct resource ids, all four shards: 1,078,953
distinct ids, fleet ∪ serving         : 1,095,984
```

The proposal's warning not to add 19,255 + 501,161 is correct, and the measured overlap is small
but non-zero: **2,224 serving documents are also in the fleet**, and the four shards duplicate
11,231 resource rows among themselves (0.9% of rows; 706 ids appear in all four). Deduplication is
therefore a real but small problem — consistent with "host-disjoint shards" being mostly true.

---

## 4. What the proposal misses

1. **The frontier is carrying crash residue.** 8,603,912 entries are `NEW` (never attempted) and
   **328,712 are stuck `FETCHING`** with expired leases — the signature of crawlers killed
   mid-run. Restarting the crawl without lease recovery would leak capacity against entries no
   worker will ever complete.
2. **`robots_cache` is empty on all four shards (0 rows).** `docs/ARCHITECTURE.md` §6 specifies
   "one centralized policy cache"; the running crawler re-derives the policy per run. Any resumption
   plan should decide whether that cache is supposed to exist.
3. **Federation is not free of identity work.** It is *cheap* because resource ids are already
   globally stable — verified: same URL → same `resource_id` across shards, 0 mismatches in a
   20,000-URL sample per pair. The proposal lists "stable global identity" as a task; it is
   already true and should be stated as a precondition met, not as work.
4. **The shards have no graph.** `entity = 0`, `graph_edge = 0`, `work = 0` on all four, and no
   `vectors.bin`. So federation is lexical-only on day one — the proposal says this, but should
   also note that the serving index's graph (14,659 entities / 130,359 edges) covers only 19,255
   documents and cannot be extended to the fleet without a rebuild.
5. **Nothing about the deployment.** The running binary is from **2026-10-13 08:51**, HEAD is
   **2026-10-18**, and 23 files are uncommitted (the LLM provider migration). Any plan that starts
   by "rebuild and deploy" must first reconcile those, and cannot build at all until disk is freed.
6. **The answer-delivery failure is a code bug, not a disk symptom.** Freeing 200 GB does not fix
   it; the next `ENOSPC`, read-only mount, or full inode table reproduces it. The fix is one
   property: a cache write must never fail a retrieval.

---

## 5. A revised order of work

The proposal's phases are the right shape. Three changes to the order, each justified by a
measurement above.

**Do first, before any new subsystem (hours, not phases):**

1. Make the answer-cache write best-effort and test it by simulating `ENOSPC`. *Removes the
   current 5–47 s latency and 100% answer loss on cache miss.*
2. Free disk: build artifacts (~167 GB), `opencode.db` 4 GB, duplicate `raw-dataset` 47 GB,
   journals 1.7 GB. *Unblocks building, crawling and merging.* No corpus content touched.
3. Recover ~330K expired frontier leases so a resumed crawl does not leak attempts.

**Then the proposal's own Phase 2 (federation), unchanged in intent** — it is the only change here
that moves reachable documents from 19,255 to ~518,278, and it is already proven to work
read-only in 112–230 ms per shard.

**Then re-order two of its later phases:**

4. **Fix the ranking before building the source directory** (proposal has this in Phase 4/5).
   Two concrete defects are one-line-scale: give the source-first lane a real source scope (or
   delete it), and give dense its weight. Without this, every source-first idea the project
   builds afterwards sits on a lane that provably returns BM25 order. The benchmark the proposal
   asks for is the gate — and `benchmarks/queries.corpus.jsonl` already holds **600 graded
   queries** (title-as-query, label = that document) plus 25 hand-judged paraphrases. That is a
   usable starting point, not something to be built from scratch.
5. **Build the source directory from data already held** (sitemaps, feeds, frontier, titles),
   not by new crawling. 274,996 sitemaps and 9.05M known URLs are the catalog; the missing part is
   only topical representation, which is where a model legitimately helps — and can be done on the
   **106,142-entry** LLM cache that already exists.

**Keep from the proposal as-is:** the six agent operations; "deterministic operations before LLM
calls"; keeping the full-text index as a fallback so the migration stays reversible; measure
source recall / page recall / evidence coverage / time-to-evidence rather than document count;
and the refusal to start distributed infrastructure.

---

## 6. Risks in the proposal as written

| Risk | Why it matters here |
| --- | --- |
| Treating source-first as an existing signal | The reweighting experiment would "discover" that 0.4 and 0.8 behave identically, and the result would be misread as "source routing does not help" |
| Building L0/L1 before fixing ranking | The new layers inherit the same no-op fusion, so their value cannot be measured against the old ones |
| Assuming a 100 GB recovery target | Actual disposable space is ~200 GB; under-shooting leaves the merge and rebuild starved |
| "Do not enforce a rigid source-first filter" | Correct, but the fallback path it relies on is the *global lexical* path — which is currently the only path, for every query |
| Deferring semantic ranking to Phase 4 | Defensible only because no benchmark exists yet; but the fix is small and the measurement set (600 graded queries) is already committed |

---

## 7. Bottom line

`next-move.md` is a good architectural direction built on a partly wrong model of the current
system. Its most valuable insight — *the durable asset is knowledge about sources, not copies of
pages* — is reinforced by the measurements: 284 GB of raw HTML for 6 GB of index, 590K documents
whose text is already on disk and unreachable, and 9M URLs already discovered.

But it would spend its first phases building source knowledge and federated retrieval on top of a
ranking layer that is provably BM25-only, while the live service loses every generated answer to
a cache-write error and answers any engine query in 5–47 s instead of 200 ms–3 s.

Fix the two one-line defects, free the 200 GB, then federate. That is the same plan, with the
parts that are actually free moved to the front.
