# Hexxica v2 — the architecture, complete

**Revision 2.1 — 2026-11-01.** Normative for this repository.
**Scope:** what the engine is, what it stores, how a query is answered, and which parts are real.
Every number marked *measured* was read from this host on 2026-11-01; the commands that produce
them are in §13.

Reading order for anyone picking this up:

| Document | Role |
| --- | --- |
| **this file** | the design, the data model, the invariants. If code and this file disagree, one of them is wrong and must be fixed |
| `docs/PLAN-v2.md` | sequencing and gates (what to do next, and what measurement decides it) |
| `docs/EXECUTION-STATUS-v2.md` | measurements, including the ones that failed |
| `docs/WORK-QUEUE-v2.md` | the delegable backlog (hours-sized tasks for coding agents) |
| `docs/architecture-update-v2.md` | the original target design, kept as rationale; superseded where this file differs |

---

## 1. Objective

Identify relevant articles across the web **without storing the articles**, return their addresses in
milliseconds, and rank them by learned semantic association rather than exact words.

The design constraint that shapes everything else: **the expensive work happens at indexing time.**
The online path may not call a hosted LLM, scan a whole corpus, read article content, or traverse a
graph without a bound (§15 of the original design, restated as invariant I-7).

Non-goals, until a measurement demands them: distributed indexing, training a transformer, trillion
edge materialisation, Matryoshka/ANN retrieval, and any learned router before a rule-based one.

---

## 2. Principles and the rules they impose

| # | Principle | Rule it imposes on the code |
| --- | --- | --- |
| P1 | Intelligence is built offline | a query path may not do inference; batches (`concepts build`, `features build`, `embed build`) do |
| P2 | The index stores addresses and evidence, not documents | `body` is not a Tantivy field (§5). Full text lives on disk as an ingest source and is read on demand |
| P3 | Associations are sparse and typed | only activated features / supported concepts are materialised; nothing allocates a dense document×concept matrix (§6) |
| P4 | Every learned number names its model | `model_id`+`version`+`space_id` travel with the association; a rebuild purges its own version before writing (I-4) |
| P5 | Direct evidence and inference stay distinguishable | `doc_concept.relation_kind` = `direct` \| `inferred`; never summed into one number |
| P6 | The measurement sets the weight | a lane ships at weight 0.0 until a graded run shows a gain; "it is new" or "it took work" is not evidence |
| P7 | Nothing is silently degraded | missing vectors/text/model ⇒ an explicit count or error, never a plausible substitute |

---

## 3. System map

```text
OFFLINE (CPU, hours, resumable, checkpointed)          ONLINE (per query, ~35 ms p50)

 fetch ─► extract ─► identity ─► ┌ lexical index  (title/headings/entities/excerpt)
                                  ├ dense vectors   (384-d, unit norm)
                                  ├ concept matrix  (doc × concept × dimension)
                                  ├ feature matrix  (doc × 4096, 16 active) + postings
                                  └ graph (concept_relation, entity edges)

 query ─► normalise ─► route ─► lanes ─► fuse (RRF) ─► rerank? ─► resolve ─► result rows
                                                                     └ /read ─► text/<hash>.txt
```

Five roots are served as one corpus (`SEARCH_FEDERATE_DIRS`): the 19K-document primary plus four
fleet shards. Identity is globally stable (`resource_id`), so federation needs no id rewriting.

**Measured fleet:** 1,558,543 resources, 1,557,504 indexed documents, 5 roots, 37 crates, 66,248
lines of Rust.

---

## 4. Storage layers

Only one layer is authoritative: **metadata (SQLite) + extracted text on disk**. Everything else is
derived and rebuildable, and is treated as such by every tool here.

| Layer | Contents | Store | Measured size | Authoritative? |
| --- | --- | --- | --- | --- |
| L0 identity | domain, resource, document_version, content, sitemap, frontier, fetch journal | `meta/meta.sqlite` per root | 11.7 GB (5 roots) | **yes** |
| L1 evidence | title, headings, entities, excerpt, doc_type, language, timestamps, ids | Tantivy `tantivy/` | **1,710 MB** | derived from L0 |
| L2 dense | 384-d unit vectors keyed by resource | `vectors.bin` per root | 2.14 GB, 1.39M docs | derived |
| L3 sparse features | forward `doc_feature` + inverted `features/postings.bin` | SQLite + file per root | 22.2M rows, 180 MB postings | derived from L2 |
| L4 concepts | `concept`, `concept_alias`, `doc_concept`, `doc_concept_dim` | SQLite | 885,364 / 1,892,440 rows, 15 concepts | derived from L1 text |
| L5 source/graph | `source_concept`, `concept_relation`, `web_link`, `graph_edge`, `entity` | SQLite | mostly **empty** (see §6.5) | derived from L1/L0 |
| L6 source bytes | extracted text `text/<h2>/<hash>.txt` (raw HTML only if opted in) | filesystem | 18.2 GB | **ingest source** |

L6 is the only layer that can be deleted, and only after L1–L5 are verified: `text/` is what makes
`/read`, embedding and graph building possible. Deleting it is a one-way operation
(`docs/PLAN-v2.md` Phase 2) and is **not** part of the current model.

---

## 5. The index model

### 5.1 Why the article left the index

Three models were measured, in this order:

| Model | Fields | Fleet index | Per doc | Problem |
| --- | --- | --- | --- | --- |
| IM-1 | `body` TEXT\|STORED, `body_boost` | 19,624 MB | 12.7 KB | the page indexed *and* stored; `body_boost` duplicated title+headings+body |
| IM-2 | IM-1 + `excerpt`, keep `body` stored | — | — | reversible half-step; still two copies of every article |
| **IM-3 (current)** | `excerpt` (500 chars) instead of `body`; `headings`/`doc_type`/`language` STORED | **1,710 MB** | **1.10 KB** | none for retrieval; `/read` needs L6 |

IM-3 is 11.5× smaller than IM-1 for the same corpus and the same 600-query quality (a title-derived
query set that never used body prose — see §9 for what that instrument can and cannot judge).

The rule that made this safe: **a schema edit and its rebuild are one operation.** Changing the
field set changes the schema hash, and `Indexer` refuses to open an index whose hash differs — so
"change the schema" without "rebuild every root" leaves the fleet unreadable, which is exactly what
forced the revert in `8655408`. `scripts/rebuild-fleet-index.sh` is the atomic form: stop writers →
move the old directory aside → build → restart. Rebuild cost measured: 12.1 minutes for 1.56M documents (206 s per 385K-document shard,
~1,870 docs/s).

### 5.2 Tantivy fields (frozen)

| Field | Type | Purpose |
| --- | --- | --- |
| `doc_id` | STRING, STORED | delete term; the *version* id (`res_<hash>:N`) |
| `version` | u64, INDEXED+FAST | overlay/base precedence |
| `resource_id`, `work_id`, `content_id` | STRING, STORED | identity; `content_id` resolves the text file |
| `url` | TEXT, STORED | resolution; indexed at low weight |
| `title` | TEXT, STORED | display |
| `title_boost` | TEXT | the indexed copy of the title, boosted 3.0 (Tantivy cannot boost inside a field) |
| `headings` | TEXT, **STORED** | organising model's H1/H2 summary; boost 2.0; readable by export |
| `excerpt` | TEXT, **STORED** | ~500 chars of the document — the only article text in the index; boost 1.0; snippet source |
| `entities` | TEXT, STORED | key entities from the organising model; boost 3.0 |
| `source_id` | STRING, STORED | site identity |
| `doc_type`, `language` | STRING, **STORED** | facets/export (were indexed-only and silently lost) |
| `published_at`, `modified_at`, `fetched_at` | i64 | freshness |
| `visible_generation` | u64 FAST | watermark gating |
| `tombstone` | u64 FAST | deletion without a re-index |

Deliberately absent: `body`, `body_boost`, anchors, link targets, full URL lists, concept names.
A posting list never carries a URL string or article text — it carries ids and quantised weights.

**Rebuild fidelity.** Extracted text can rebuild the excerpt, the vectors and the concept tags; it
cannot rebuild `headings` or `entities`, which the parser and organising model produce while the page
is in hand. They are therefore persisted at ingest in `document_derived(docver_id, headings,
entities)` (migration 0009, same commit as the document) and read back by `agent0 index build`.
Without that row the IM-3 rebuild emptied both fields — measured on 1.56M documents, which is why the
table exists. Excerpts are deliberately *not* stored there: they are a pure function of the text and
a copy could drift from it.

### 5.3 Size, measured after the IM-3 rebuild

| Root | Documents | Tantivy |
| --- | --- | --- |
| shard-0 | 385,024 | 427 MB |
| shard-1 | 461,312 | 510 MB |
| shard-2 | 464,896 | 514 MB |
| shard-3 | 227,328 | 239 MB |
| primary | 18,944 | 20 MB |
| **total** | **1,557,504** | **1,710 MB** |

0 missing text, 0 empty per root — every indexed document had a readable source file at build time.

---

## 6. The matrices

Six matrices, one per association kind. "Density" is non-zeros ÷ cells; the point of the sparse
design is that every one of them is far below 1%.

### M1 — document × learned feature (forward index)

`doc_feature(resource_id, feature_id, model_id, version, space_id, activation)`.

| Property | Value |
| --- | --- |
| Shape | 1,387,584 documents × 4,096 features |
| Non-zeros | **22,201,352** (16.0 per document) |
| Density | 0.39% |
| Producer | `agent0 features build` (encoder `z = TopK(ReLU(W e_d + b))`, 16 active, tied decoder) |
| Consumers | feature rerank (candidate features without a postings scan), update/retraction |
| Provenance | `model_id`, `version`, `space_id` on every row |

### M2 — feature postings (inverted index)

`features/postings.bin`, magic `ANSEPOST1`: `features × { count u32, count × { doc u32, q u8 } }`,
plus a per-document id table. 8-bit quantised activation, as the design requires.

| Property | Value |
| --- | --- |
| Shape | 4,096 features × 1,387,584 documents, sparse |
| Size | 180 MB across 5 roots (13.5 MB primary, 40/49/51/26 MB shards) |
| Score | `Σ_f q_f · z_df · idf_f`, `q = TopK(ReLU(W e_q + b))` |
| Query side | same encoder, so query and document features share a vocabulary by construction |
| Measured behaviour | lane recall@100 is the instrument (`features probe`); fusion weight 0.0 (§8.3) |

### M3 — document × concept (scalar associations)

`doc_concept(resource_id, concept_id, weight, relation_kind, evidence, passage_id, via_concept_id,
model_version)`.

| Property | Value |
| --- | --- |
| Rows | **885,364** over 506,363 distinct documents and 15 concepts |
| Coverage | **32.5%** of documents carry at least one association (1.75 concepts each, on average) |
| Producer | `agent0 concepts build` — deterministic lexicon tagging at 749–802 docs/s per shard |
| Consumers | `ConceptLane`, `FederatedConceptLane`; source profiles (not yet built) |
| Kind | `direct` (the page states it) vs `inferred` (through `concept_relation`) |

### M4 — document × concept × dimension (R(d,c))

`doc_concept_dim(resource_id, concept_id, dim, value, model_version)` — the vector the original
design asks for at §6, so one article can be strong on `mechanism` for one query and strong on
`contextual` for another without reindexing.

| Property | Value |
| --- | --- |
| Rows | **1,892,440** (2.14 dimensions per association) |
| Dimensions in use | `direct`, `contextual`, `mechanism`, plus `evidence`/`confidence` where recorded |
| Evidence rule | each dimension must have independent evidence, or it is decoration: when all three were derived from the same body counts, `mechanism` was a strict subset of `direct` and could change no ranking |
| Key | the dimension is a string, not an ordinal: a new model may discover a dimension without a migration |

### M5 — concept × concept (typed graph)

`concept_relation(src, dst, relation, strength, evidence, model_version)` with SKOS relations
(`equivalent`, `broader`, `narrower`, `related`) and free typed relations (`mechanism_of`, …).
**0 rows — schema only.** Traversal is bounded by design; inferred associations must be marked
`inferred` and are never allowed to compound across hops.

### M6 — source × concept, and document-level graphs

| Matrix | Table | Rows | State |
| --- | --- | --- | --- |
| source × concept (normalised) | `source_concept` | 0 | **not built**; the column `doc_count` exists so the normalisation is auditable |
| page → page links | `web_link` | 0 | **not captured**; derivable only from raw HTML, so it must be built before any blob deletion |
| entity co-occurrence | `graph_edge` | 130,359 (primary only) | present on the primary root, absent on the fleet |
| passage → concept | `passage`, `passage_concept` | 0 | schema only |

### How they combine

$$\text{score}(d,q) = w_{bm25}\,S_{bm25} + w_{dense}\,S_{dense} + w_{concept}\,S_{concept} + w_{features}\,S_{features}$$

with the shipped engine using reciprocal-rank fusion (`w_l/(c+rank_l)`, `c=60`) rather than the
linear form — RRF needs no calibration, and the linear form is only legitimate once its coefficients
are fitted (§11 of the original design). Measured consequence of `c=60`: a document found by one
lane scores 0.0138 at rank 1, one found by two lanes 0.0303, so **a single extra lane can re-rank
but cannot outvote the lexical pair** until `c` or the weights are tuned against graded data.

---

## 7. Identity, resolution and "no article stored"

| Identifier | Meaning |
| --- | --- |
| `resource_id` | canonical fetchable URL (`res_<hash>`) |
| `docver_id` | one version of a resource (`res_<hash>:N`) — the index's `doc_id` |
| `content_id` | one byte-representation (`cnt_<hash>`) — resolves `text/<h2>/<hash>.txt` |
| `work_id` / `content hash` | logical article / dedupe across mirrors |
| `concept_id`, `feature_id` | stable taxonomy id / model-scoped learned id (**never interchangeable**) |

Resolution is now honest end to end: `/read?doc_id=` accepts either the version id `/search`
returns or the resource id, and consults every served root's metadata when a federated result lives
on a shard. Verified live: a shard-3 result returned 15,375 chars from disk while the index held
none of them.

When L6 is eventually deleted, `/read` degrades to the stored excerpt — a real fragment, not an
invented document. That is a deliberate, documented state, not an error path.

---

## 8. Online retrieval

### 8.1 Stages

```text
query ─► prune/normalise ─► (optional planner: cached, never blocking)
      ─► lanes: bm25 · source_first · dense · graph · concept · features
      ─► candidate set (ids only) ─► docmap (lookup_doc per id)
      ─► RRF fusion ─► optional stage-3 rerank (top 50) ─► result rows
```

The engine builds its document map from the lexical lanes' retained hits, so **any lane whose hits
are not resolvable via `Searcher::lookup_doc` can only re-rank, never retrieve**. Any new lane must
therefore be index-backed and cheap to look up (`Indexer::doc_by_id`: term lookup, no `reload()`,
no stored body).

### 8.2 Lanes and their measured weights

| Lane | Backing | Weight shipped | Evidence |
| --- | --- | --- | --- |
| `bm25` | Tantivy fields above | 1.0 | baseline of every measurement |
| `source_first` | same index, source-scoped | **0.0** | identical to BM25 on 64/64 results — a duplicate lane, not an accelerator |
| `dense` | `vectors.bin`, 384-d cosine | 0.0 | +0.0017 nDCG on the 600-set; 3.7× BM25 on cross-disciplinary (n=6) |
| `graph` | `graph_edge` | 0.0 | ±0.0000; empty on the fleet |
| `concept` | M3/M4 | 0.0 | −0.0114 on the 600-set (monotonic in weight); 3.9× BM25 on cross-disciplinary (n=6) |
| `features` | M1/M2 | 0.0 | −0.0910 at w=0.8 on the primary root, −0.0021 at fleet scale |

A weight-0 lane is still *executed* unless it implements `lane_hint`; the concept lane costs ~1.5 s
per query across five shards for output that cannot change a result, so the hint is mandatory for
any lane that ships off.

### 8.3 Latency, measured at 1,557,504 documents

| Path | p50 | p95 |
| --- | --- | --- |
| legacy `/search` (lexical, federated) | **35 ms** | **97 ms** |
| `engine:true` (all lanes, planner, nav) | 620 ms | 2,541 ms |

The target (<50 ms p95, §15) is met on the lexical path with the smaller index; the engine path is
**not** within budget and is the reason routing exists. `SEARCH_ROUTE=1` removes 3 of 5 lane calls on
the median query for −0.0007 nDCG, and ships off only because that cost must be chosen.

### 8.4 Budget and degradation

Every request has a candidate cap and a time budget; exceeding it returns partial results with
`degraded: true` rather than blocking. Zero-weight lanes are skipped before execution. A lane that
errors is reported in `failures` and does not fail the request.

---

## 9. Evaluation

Three instruments, each blind to something:

| Set | Shape | Size | What it can judge | What it cannot |
| --- | --- | --- | --- | --- |
| `queries.corpus.jsonl` | query = the document's own title | 600 | lexical correctness, latency, regressions | semantic ability: a title query shares its words with the answer |
| `queries.body.jsonl` | query = a sentence from the document's body | 600 | whether a lane can map prose to its source | cross-topic association; still one known-relevant doc |
| `queries.cross_disciplinary.jsonl` | query shares **no content word** with the answer's title | 6 | the capability the semantic lanes exist for | nothing statistical at n=6 (one case ≈ 0.017 nDCG) |

Headline results (600 cases, Fast mode, one process per configuration):

| Configuration | nDCG@10 |
| --- | --- |
| federated BM25 baseline | 0.2481 |
| + dense w=0.8 | +0.0017 (primary-root run: 0.4830 vs 0.4813) |
| + concept w=0.8 | −0.0114 |
| + features w=0.8 | −0.0021 at fleet scale (−0.0910 primary) |
| BM25, cross-disciplinary (n=6) | 0.0645 |
| concept, cross-disciplinary (n=6) | **0.2500** |
| dense, cross-disciplinary (n=6) | 0.2384 |

Both facts are true and must be reported together: the semantic lanes are noise-to-harm on the set
that cannot represent their capability, and ~4× the baseline on the set that can. Neither justifies
a fusion weight until the cross-disciplinary set reaches ~50 hand-judged cases.

Rules for any future measurement: one process per configuration, `env -u` every `SEARCH_FUSION_*`
first, `--mode fast` always, and reconcile the deployed systemd drop-in in the same step (§13.3).

---

## 10. Models

| Model | Role | State | Cost | Gate |
| --- | --- | --- | --- | --- |
| A — teacher | BGE-small-en-v1.5, 384-d | **in use**, 1.39M documents embedded | 13.2 docs/s at 2,000 chars under crawl contention | retrieval quality, not reconstruction |
| B — sparse encoder | `TopK(ReLU(We+b))`, 4,096 features, 16 active, tied decoder | **trained** (`features/encoder.bin`, 283K samples, 4 epochs, 775 s) | CPU, minutes/epoch; matrix is 6.0 MiB | candidate recall ≥ BM25 at equal candidates; **not met** |
| C — concept predictor | multi-label over the lexicon | **deterministic lexicon** replaces it (`concepts build`) | ~750–800 docs/s | coverage and false-positive rate by reading titles |
| D — query encoder | projection through the same `W` | **in use** (same encoder, max-normalised) | ~1 ms | end-to-end p95 |
| E — router/ranker | rule router + feature rerank | **built, off** (`SEARCH_ROUTE`, `SEARCH_RERANK`) | 67 ms saved p50; rerank +2 ms | router: recall@20 not reduced; rerank: nDCG improves |

The trained encoder is the design's core and currently earns no weight. The next honest experiment
is training it on titles as well as bodies, and IDF-weighting the postings — not raising its weight.

---

## 11. Invariants

Each is enforced somewhere a future change can be caught.

| # | Invariant | Where |
| --- | --- | --- |
| I-1 | The index never stores the article | `crates/indexer/src/schema.rs` (no `body` field), test `all_docs_...: body == ""` |
| I-2 | A schema change ships with a rebuild | `_schema_hash` guard + `scripts/rebuild-fleet-index.sh` |
| I-3 | `body` is never passed off as a document | `Pipeline::all_docs` leaves it empty and counts the misses |
| I-4 | Every association names its model; a rebuild purges its own version first | `doc_feature`/`doc_concept` columns; `concepts build --rebuild` |
| I-5 | Direct and inferred never merge | `doc_concept.relation_kind` in the primary key |
| I-6 | No byte-offset slicing of text | `excerpt_of`, prefix compares, stemmers (asserted with a CJK/Russian test) |
| I-7 | The online path calls no LLM, scans no corpus, reads no article, traverses no unbounded graph | engine budgets, `lane_hint`, `doc_by_id` cost rules |
| I-8 | A weight-0 lane costs nothing | `LaneSearcher::lane_hint` |
| I-9 | Federation reports a non-zero generation and never overstates freshness | `FederatedSearcher::manifest` |
| I-10 | A concept covering >20% of the corpus is downweighted, never deleted | ubiquity suppression in `concept_tag` |

---

## 12. Implementation status

Honest, by layer. "Partial" means it exists for some roots or some paths only.

| Capability | State | Evidence |
| --- | --- | --- |
| Identity, migration ledger, watermark, tombstones | **done** | 8 migrations on all roots |
| Excerpt-only index (IM-3) | **done**, all 5 roots | 1,557,504 docs, 1.71 GB |
| Rebuild fidelity (`document_derived`) | **done**, migration 9 on all roots | new ingests persist headings/entities; pre-existing documents stay empty until re-crawled |
| `/read` resolution incl. federation | **done** | live 15,375-char read of a shard doc |
| Dense vectors | **done**, 1.39M docs | 2.14 GB across 5 roots |
| Feature encoder + postings + forward index | **done**, weight 0.0 | 22.2M rows, 180 MB postings |
| Concept matrix M3/M4 | **done**, weight 0.0 | 885,364 / 1,892,440 rows, 15 concepts |
| Concept vocabulary breadth | **weak** | 15 concepts over 1.56M docs; a lexicon, not a taxonomy |
| Concept graph M5 | **absent** | 0 rows |
| Source profiles M6 | **absent** | 0 rows |
| Link graph (`web_link`) | **absent** | 0 rows; raw HTML no longer retained, so it is only recoverable from a re-crawl |
| Passage layer | **absent** | 0 rows |
| Router, rerank | **built, off** | flags + tests |
| Query planner / answers | **opt-in** | `SEARCH_PLANNER_ENABLED=1`, answers off |
| Cross-disciplinary set at usable size | **absent** | n=6 |
| Distributed index | **absent, by design** | single node |

---

## 13. Operations

### 13.1 Deploy

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo test -p search-indexer -p search-agent0 -p search-api -p search-engine -p search-features
cargo build --release -p search-agent0          # default features include bge
cp /opt/search/bin/agent0 /opt/search/bin/agent0.rollback-$(date +%Y%m%d-%H%M)
install -m 0755 target/release/agent0 /opt/search/bin/agent0
systemctl restart search-api && curl -s http://127.0.0.1:8090/health
```

### 13.2 Rebuild an index (required with any schema change)

`scripts/rebuild-fleet-index.sh` under `systemd-run` (a plain tool-call process is OOM-killed at
~4 GB). It stops the crawlers and the API, moves each `tantivy/` to `tantivy.fulltext-<ts>`, rebuilds
from metadata + text, restarts both. Measured 12.1 min for the fleet.

### 13.3 Measurement hygiene

* `systemctl show search-api -p Environment` must agree with the weights this document claims;
  drop-ins are additive and a stale `fusion.conf` silently overrode a measured 0.0 once.
* `eval run` must use `--mode fast` and federate exactly as `serve` does.
* Quote a rate with its window and contention, and a storage number with the root it was measured on.

### 13.4 Monitoring signals that actually distinguish states

| Signal | Healthy | Failure it exposes |
| --- | --- | --- |
| ingest count per 10 min per shard | > 0 | alive-but-idle crawler (lock zombie, rate-limit starvation) |
| `generation` across shards | advancing | nothing is being published |
| shard log mtime | recent | a crawler spinning at 99% CPU with a frozen log |
| `doc_count` vs `document_version` | equal (minus textless) | index behind the corpus |
| lane `docs=N` at startup | matches corpus | lane built on one root, served from another |

---

## 14. What would change this architecture

Named so the design is falsifiable:

1. **Sparse features beat BM25 on a 50-case cross-disciplinary set** → give them weight, enable the
   router, and the index-of-associations claim becomes measured rather than asserted.
2. **Dense recovers recall the sparse vocabulary misses** → then, and only then, evaluate an MRL
   teacher and compressed vectors (§10 of the original design).
3. **A single node cannot hold the corpus** → partitioned posting lists and a coordinator; today
   1.56M documents occupy 1.7 GB of index and 2.2 GB of vectors on one host.
4. **Source-level retrieval is requested** → build `source_concept` from `doc_concept` normalised by
   `doc_count`, not by summing article weights.