# The "brain in the index": multi-axis semantic weights

Response to: *build a bigger vector model inside the index; a page about chemicals that affect love
should carry weight on both the chemistry side and the love side, so a query about one reaches the
other.*

The idea is right and has a precise name: **a semantic index with multi-axis document weights** —
each document is placed on several topical axes with a weight, and a query is matched against the
axes rather than against words. Everything below was measured on this host on 2026-10-24.

---

## 1. The part that already exists — and is switched off

The dense lane is built, embedded and queried today: BGE-small-en-v1.5, 384-d, 19,255 vectors,
brute-force cosine. What it is *not* is **weighted**: `FusionPolicy::default()` gives BM25 1.0,
source-first 0.8, and dense/graph **0.0**.

So the semantic judgement is made and then discarded. Live example:

```
query: "chemical reactions that affect romance"
  bm25=1  dense=·   List of datasets for machine-learning research — Wikipedia
  bm25=2  dense=·   dsfb_chemical_engineering — Rust
  bm25=3  dense=1   Chemistry — Simple English Wikipedia
  bm25=4  dense=·   Cloud Computing Services — Amazon Web Services
```

The dense lane *did* rank the Chemistry page first. The fused order ignored it, because its weight
is zero. **Giving dense a non-zero weight is a one-line change and the first real step of this
design** — before any bigger model.

## 2. The part nobody can fix with a bigger model: there is nothing to point at

Your example needs a page that talks about the chemistry of attraction. The corpus does not contain
one. Counted in titles, shard 1 (367,045 resources):

| term | titles containing it |
| --- | --- |
| `oxytocin` | 0 |
| `romantic love` | 0 |
| `phenylethylamine` | 0 |
| `limerence` | 1 |
| `dopamine` | 7 |

A better embedding model cannot retrieve a document that was never crawled. With fetch-on-demand
(the model we agreed on), the index only has to know *that such a page exists* — but it must have
seen it once. Coverage is the binding constraint here, not model size. That is what the 9,050,755
already-discovered-but-unfetched frontier URLs are for.

## 3. What "a bigger brain" costs, measured

Embedding throughput on this 16-vCPU box (BGE-small, 2,000 characters per document):

```
agent0 embed build --limit 500 --embedder bge   →  62.5 s   =  8 docs/s
```

| Corpus | Time at 8 docs/s |
| --- | --- |
| 19,255 (serving) | 40 min |
| 200,000 (a curated subset) | ~7 h |
| 501,247 (published fleet) | ~17 h |
| 1,090,184 (every resource) | **~38 h** |

A 1024-d model (bge-large / Qwen3-embedding class) is roughly 3× the compute and 2.7× the storage
per vector: **4.5 GB** for 1.09M docs instead of 1.6 GB. And BGE truncates to 2,000 characters, so
a long article is embedded from its opening — the exact failure mode your example describes (the
topic appearing in paragraph 20).

## 4. The design that delivers your example, cheaply

Store the *axes* explicitly instead of hoping one vector carries everything:

**Offline (once per corpus change)**
1. Embed documents (as today).
2. Cluster the embeddings into **k topical hubs** (k ≈ 20,000 for 1.09M docs; k-means or
   spherical k-means, deterministic seed).
3. For each document keep its **top 6 hubs with weights**: `{hub 4211: 0.71, hub 8802: 0.63, …}`.
   That is literally "0.7 chemistry, 0.6 love".
   Storage: 6 × 12 B ≈ **72 B/doc → 78 MB** for the whole fleet. Hubs themselves:
   20,000 × 1024 × 4 B ≈ 82 MB.

**At query time**
1. Embed the query (one BGE pass, ~10 ms).
2. Score the hubs, take the top ~20.
3. Pull their posting lists and sum the weighted overlap per document.
4. Feed that as a ranked lane into the existing fusion — with a weight that is **not zero**.
5. Fetch the top pages live (measured: p50 0.30 s) and extract passages.

Why this shape and not "just a bigger vector":

* it is **explainable** — the API can say *matched on chemistry 0.63 + romance 0.58*, which is
  exactly what "add weight to both sides" means, and it is auditable;
* it is **fast at 1M+ documents**, because retrieval is posting-list intersection, not a full scan
  of 1.09M × 1024 floats per query;
* it is **cheap to store** (78 MB for the weights vs 4.5 GB for raw vectors), and the vectors can
  stay int8 (~1.1 GB) for recall;
* it composes with the index-only model: hubs + weights + excerpt are all *derived* data, so
  nothing "downloaded" needs to be kept.

## 5. Code that has to change

| Change | Size | Note |
| --- | --- | --- |
| dense weight > 0 in `FusionPolicy` | one line | today it is 0.0; without this nothing semantic ranks |
| `topic` lane: `doc_hub` table + postings + query→hubs | new module, ~1–2 days | mirrors the graph lane's shape (`crates/api/src/graph_lane.rs`) |
| ANN wiring for >100K vectors | `crates/ann` exists, unwired | brute force at 1.09M × 384 = 419M flops/query |
| hub builder | small offline tool | k-means over the vector sidecar |
| evaluation | already there | 600 graded queries in `benchmarks/queries.corpus.jsonl` |

## 6. The honest risk

Measured on a random 40-title sample of the fleet: the corpus is heavily spam, adult and SEO
content (`xvideo…`, `jav.guru`, `Kalyan Panel Chart`, casino pages). A semantic index ranks that
content *confidently*. Topic weights amplify whatever the corpus is; with 92% noise they amplify
noise.

So the order is: **fix the weight → widen coverage toward real sources → then grow the brain.**
Doing it in the other order produces a very good semantic index of a very poor corpus.
