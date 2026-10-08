# Stage-2 benchmarks — ablation harness

A small, **committed** measuring stick for hybrid retrieval: a fixture corpus, a
hand-labelled query set, and an ablation binary that reports nDCG@10 / MRR / recall@10 next to
a resource budget. This is a **smoke set, not a research benchmark** — the schema is what a real
set would use; the numbers below are what this 30-doc corpus actually produces.

## Files

| path                       | what it is                                                            |
|----------------------------|-----------------------------------------------------------------------|
| `corpus/<source>/<doc>.html` | 30 short HTML docs across 5 sources (`arxiv`, `github`, `docs`, `community`, `blog`), LLM/tech themed, < 20 KB total. Doc id = corpus-relative path without extension, e.g. `github/tokio`. Source = first path component. |
| `queries.example.jsonl`    | 25 hand-labelled cases in the `search_eval::QueryCase` schema (`id`, `query`, `class`, `relevant`, `graded`, `observed_at`). |
| `results.json`             | machine-readable output of the last run (per-ablation metrics + budget + per-case diagnostic). |

Labels were written by reading the corpus; they are **not** derived from any retrieval run.
Source-qualified queries name a source in natural language (`github`, `docs`, `community`,
`blog`); the routing lane uses those hints, plain BM25 cannot.

## Run it

One documented command (from the repo root):

```sh
cargo run -p search-bench --release
```

That builds a temp Tantivy index from `corpus/`, runs every ablation over
`queries.example.jsonl`, prints the table, and writes `benchmarks/results.json`. Add
`--per-case` for a per-query bm25-vs-hybrid diagnostic. Overrides: `--corpus DIR`,
`--queries FILE`, `--out FILE`, `--k N`, `--dim N`, `--ce-depth N`.

Tests (drive the same library function the binary uses):

```sh
cargo test -p search-bench
```

## Ablations

* `bm25` — global lexical BM25 over the Tantivy index.
* `bm25+dense` — RRF(bm25, HashEmbedder top-k over title+body).
* `bm25+dense+sparse` — RRF(bm25, dense, source-routed BM25 lane).
* `source-routing` — source-routed BM25 only (no global lane; empty when the query has no source hint).
* `source-routing+global` — RRF(source-routed, global BM25).
* `bm25+ce-on` — bm25 top-50 reranked by the lexical cross-encoder.
* `bm25+dense+sparse+ce-on` — full hybrid top-50 reranked by the lexical cross-encoder.

The `ce-off` counterpart of each `ce-on` row is its parent hybrid row.

## Result (actual, this smoke set — 30 docs, 25 queries)

| ablation                  | nDCG@10 |  MRR  | recall@10 | p50 ms | p95 ms | candidates |
|---------------------------|--------:|------:|----------:|-------:|-------:|-----------:|
| bm25                      |  0.9702 | 0.960 |    1.0000 |      0 |      0 |        9.9 |
| bm25+dense                |  0.9702 | 0.960 |    1.0000 |      1 |      1 |       21.2 |
| bm25+dense+sparse         |  **0.9850** | **0.980** | 1.0000 | 1 | 1 |   21.2 |
| source-routing            |  0.6400 | 0.640 |    0.6400 |      0 |      0 |        1.3 |
| source-routing+global     |  0.9842 | 0.980 |    1.0000 |      0 |      0 |        7.5 |
| bm25+ce-on                |  0.9412 | 0.920 |    1.0000 |     11 |     21 |        9.9 |
| bm25+dense+sparse+ce-on   |  0.9412 | 0.920 |    1.0000 |     23 |     25 |       21.2 |

**Does hybrid beat plain BM25? Yes, on this set** — nDCG@10 0.9702 → **0.9850** and MRR
0.960 → **0.980** for `bm25+dense+sparse`, at the cost of a larger candidate pool
(9.9 → 21.2 mean). The improvement comes entirely from the **source-routing** lane on
source-qualified queries (e.g. `github tokio async runtime`: BM25 ranks the doc at 2, routing
puts it at 1). The dense lane alone changes nothing here (`bm25+dense` == `bm25`), because the
default `HashEmbedder` is itself a lexical bag-of-words model with no stemmer — it is not a
semantic model, so it cannot rescue a query that paraphrase alone would need. Routing-only
(`source-routing`) is weak (0.64) because non-source-qualified queries have no route and it
then returns nothing; the global lane is what carries the rest.

## Honest limitations

* **Real BGE-small weights are UNAVAILABLE** in this environment (`feature = "bge"` is not
  enabled and no weights are bundled). The dense lane uses the deterministic `HashEmbedder`;
  it proves the dense path and its fusion plumbing, but it is **not** a semantic embedding.
  A real BGE lane is the lane most likely to move the paraphrase-sensitive queries.
* **A trained cross-encoder is UNAVAILABLE.** `LexicalCrossEncoder` (deterministic lexical
  overlap, no model) is the honest proxy. On this set it *hurts* (0.9850 → 0.9412): reranking
  the top-50 by whole-query token coverage demotes docs that are lexically broad but topically
  right. That is a real finding about the proxy, not a tuning target.
* Routing uses lexical source hints read from the query text; it never consults the query
  `class` or the relevance labels. A query with no source hint has no route.
* 30 docs / 25 queries is far too small to support any generalisation. Read the table as
  "the harness works and the lanes are wired", not "hybrid retrieval is +1.5 nDCG".
