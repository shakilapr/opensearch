# Benchmarks — every number the docs quote, and the command that produced it

Run on this host (16 vCPU, 62 GB RAM), 2026-11-02, while four crawlers were active. That is stated
because it changes every latency number: a federated eval reads five indices that the crawlers are
writing, so readers reload constantly. Numbers measured with the crawlers stopped are marked as such.

Corpus under test: 5 roots federated, **1,592,836** indexed documents (primary 18,944 + four fleet
shards).

---

## 1. How to run a graded evaluation

```bash
export PATH="$HOME/.cargo/bin:$PATH"
export SEARCH_DATA_DIR=/var/lib/search
export SEARCH_READ_ONLY=1
export SEARCH_FEDERATE_DIRS=/var/lib/search-fleet/shard-0:/var/lib/search-fleet/shard-1:/var/lib/search-fleet/shard-2:/var/lib/search-fleet/shard-3

# one process per configuration; never let one config inherit another's weights
env -u SEARCH_FUSION_BM25 -u SEARCH_FUSION_CONCEPT -u SEARCH_FUSION_DENSE \
    -u SEARCH_FUSION_GRAPH -u SEARCH_FUSION_FEATURES -u SEARCH_FUSION_SOURCE_FIRST \
  ./target/release/agent0 eval run --mode fast --limit 10 --out ops/<name>.json
```

Rules that make a run trustworthy, each learned by breaking it:

* `--mode fast` **always**. The default `Auto` performs live fetches with no network path; three
  queries took 280 s and the run wrote no metrics at all.
* `--limit` bounds results per query, **not** the number of cases: a 600-case file runs all 600.
* One process per configuration, with every `SEARCH_FUSION_*` unset first.
* The eval opens each root read-only, so it never takes the crawler's writer lock.

## 2. Sets

| Set | Cases | Shape | Labels |
| --- | --- | --- | --- |
| `benchmarks/queries.corpus.jsonl` | 600 | query = the document's own title | corpus-derived, one relevant doc, graded 3 |
| `benchmarks/queries.body.jsonl` | 600 | query = a sentence from the document's body | corpus-derived, one relevant doc |
| `benchmarks/queries.cross_disciplinary.jsonl` | 6 | query shares no content word with the answer's title | hand-checked |
| `benchmarks/queries.paraphrase.jsonl` | see file | paraphrase pairs | corpus-derived |

## 3. Retrieval quality

### 3.1 600-case corpus set — semantic lanes at weight 0.8 (2026-11-01, primary root)

| Configuration | nDCG@10 | Δ vs BM25 | recall@10 | MRR |
| --- | --- | --- | --- | --- |
| BM25 only | 0.4813 | — | 0.4983 | 0.4757 |
| dense w=0.8 | 0.4830 | +0.0017 | 0.4933 | 0.4794 |
| graph w=0.8 | 0.4813 | ±0.0000 | 0.4983 | 0.4757 |
| concept w=0.8 | 0.4699 | −0.0114 | 0.4983 | 0.4604 |
| features w=0.8 | 0.3903 | **−0.0910** | 0.4817 | 0.3611 |
| features only | 0.0677 | −0.4136 | 0.1000 | 0.0575 |

Federated re-run at full scale (1,512,850 documents):

| Configuration | nDCG@10 | recall@10 | MRR |
| --- | --- | --- | --- |
| federated BM25 | 0.2481 | 0.4450 | 0.1870 |
| federated + features w=0.8 | 0.2460 | 0.4467 | 0.1843 |

The baseline falling from 0.4813 to 0.2481 when the corpus grows 78× is the instrument's property,
not a regression: a title query is answered almost by construction on 19K documents and is not on
1.5M.

### 3.2 Cross-disciplinary set (n=6)

| Lane | nDCG@10 | recall@10 | vs BM25 |
| --- | --- | --- | --- |
| BM25 | 0.0645 | 0.1667 | — |
| concept w=0.8 | **0.2500** | 0.3333 | 3.9× |
| dense w=0.8 | 0.2384 | 0.3333 | 3.7× |

At n=6 one case moves nDCG by ~0.017: this establishes capability, not a weight.

### 3.3 Concept-first ranking (2026-11-02)

Corpus-derived relatedness graph built with `agent0 concepts relate` on all five roots, then:

Measured 2026-11-02 with four crawlers active, five roots federated — so the latency columns
describe a contended host, not capacity.

| Set | Ranking | nDCG@10 | recall@10 | MRR | p50 | p95 |
| --- | --- | --- | --- | --- | --- | --- |
| 600 corpus | BM25 | 0.2112 | 0.4200 | 0.1476 | 294 ms | 5,101 ms |
| 600 corpus | concept-only | **0.1328** | 0.2567 | 0.0950 | 135 ms | 4,814 ms |
| cross-disciplinary (n=6) | BM25 | 0.1052 | 0.1667 | 0.0833 | 2,506 ms | 5,627 ms |
| cross-disciplinary (n=6) | **concept-only** | **0.2718** | **0.3333** | **0.2500** | 2,031 ms | 4,594 ms |

The two sets say opposite things, and both are true. On the shape the concept layer exists for,
concept-only ranking is **2.6× BM25** (0.2718 vs 0.1052) and finds twice the documents. On the
title-derived set it **loses** (0.1328 vs 0.2112), because those labels are the document's own words
and the concept layer deliberately does not rank on words. It is also ~2× faster at p50 (135 ms vs
294 ms) because it does not scan the lexical index on the ranking path.

That is the honest trade: concepts decide order better exactly where words fail, and worse where the
query *is* a title. A production default therefore needs a router (title-like query → lexical;
conceptual query → concepts), which is a later task, not a weight to tune. The cross-disciplinary baseline here (0.1052) differs from the 0.0645 recorded on
2026-11-01 because the corpus and the index both changed (1.59M documents, rebuilt index, crawlers
committed more pages); compare within a run, not across days.

Command:

```bash
env -u SEARCH_FUSION_* SEARCH_FUSION_BM25=0 SEARCH_FUSION_SOURCE_FIRST=0 SEARCH_FUSION_CONCEPT=1 \
  ./target/release/agent0 eval run --mode fast --lanes concept --limit 10 --out ops/concept.json
```

## 4. Latency

Measured over 40 queries against the live service (`POST /search`), 1,557,504 documents, crawlers
running:

| Path | p50 | p95 |
| --- | --- | --- |
| legacy `/search` (lexical, federated) | 34 ms | 97 ms |
| `engine:true` (all lanes, planner, nav) | 620 ms | 2,541 ms |

The design target is <50 ms p95. The legacy path meets it at p50 and **not at p95** (97 ms); the
engine path is an order of magnitude over.

## 5. Storage, measured after the IM-3 rebuild

| Root | Documents | Tantivy |
| --- | --- | --- |
| shard-0 | 385,024 | 427 MB |
| shard-1 | 461,312 | 510 MB |
| shard-2 | 464,896 | 514 MB |
| shard-3 | 227,328 | 239 MB |
| primary | 18,944 | 20 MB |
| **total** | **1,557,504** | **1,710 MB (1.10 KB/doc)** |

Before IM-3: 19,624 MB for 1.54M documents (12.7 KB/doc). Text on disk: 18.2 GB. Metadata: 11.6 GB.
Vectors: 2.14 GB (1,387,584 documents). Feature postings: 180 MB.

## 6. Matrices, measured

| Matrix | Rows | Distinct documents | Coverage |
| --- | --- | --- | --- |
| `doc_feature` | 22,201,352 | 1,387,584 | 16.0 features/doc |
| `doc_concept` | 885,364 | 506,363 | 32.5% of documents |
| `doc_concept_dim` | 1,892,440 | — | 2.14 dims/association |
| `concept_relation` | 222 | — | `cooccur` edges, top-8/concept, 15 concepts |
| `source_concept`, `web_link`, `passage` | 0 | — | not built |
| `graph_edge` | 130,359 | — | primary root only |

## 7. Throughput (crawl and builds)

| Job | Measured |
| --- | --- |
| full index rebuild (5 roots, 1.56M docs) | **12.1 min** (206 s per 385K-doc shard, ~1,870 docs/s) |
| concept tagging | 749–802 docs/s per shard |
| embedding (BGE-small, 2,000 chars, under crawl contention) | 13.2 docs/s |
| crawl (4 shards, 250 ms origin interval) | ~5,800 attempts/min, ~290K docs/h peak window |

## 8. Caveats that travel with these numbers

* All quality numbers except §3.1's primary-root table were measured on **title-derived labels**,
  which reward lexical matching by construction. They cannot judge semantic retrieval.
* The cross-disciplinary set is n=6. It proves a capability exists; it cannot set a weight.
* Latency was measured with four crawlers running and five federated roots; the same queries in one
  process against one root are much faster (`372 ms vs 17.3 s` for one measured query), so compare
  like with like.
* Rounded numbers in prose are not re-rounded here: if a table says 1,710 MB it is the sum of the
  per-root measurements in §5.