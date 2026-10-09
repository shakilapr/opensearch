# Stage-2 benchmarks — ablation harness

A small, **committed** measuring stick for hybrid retrieval: a fixture corpus, two
hand-labelled query sets, and an ablation binary that reports nDCG@10 / MRR / recall@10 next to
a resource budget. This is a **smoke set, not a research benchmark** — the schema is what a real
set would use; the numbers below are what this 30-doc corpus actually produces.

## Files

| path                       | what it is                                                            |
|----------------------------|-----------------------------------------------------------------------|
| `corpus/<source>/<doc>.html` | 30 short HTML docs across 5 sources (`arxiv`, `github`, `docs`, `community`, `blog`), LLM/tech themed, < 20 KB total. Doc id = corpus-relative path without extension, e.g. `github/tokio`. Source = first path component. |
| `queries.example.jsonl`    | 25 hand-labelled cases in the `search_eval::QueryCase` schema (`id`, `query`, `class`, `relevant`, `graded`, `observed_at`). Lexically *easy*: the query usually contains the target's own terms (mean content-word overlap with the target doc: **0.79**). |
| `queries.paraphrase.jsonl` | 25 hand-labelled cases that are *paraphrases* of their target — deliberately little shared vocabulary (mean overlap: **0.18**). This is the set that can distinguish a semantic model from a bag of words. |
| `results.json`             | machine-readable output of the last run (per-ablation metrics + budget + per-case diagnostic). Committed run = the default (`--embedder hash`) on `queries.example.jsonl`. |

Labels were written by reading the corpus; they are **not** derived from any retrieval run.
Source-qualified queries name a source in natural language (`github`, `docs`, `community`,
`blog`); the routing lane uses those hints, plain BM25 cannot.

The overlap figures above are measured, not asserted — tokenise the query and the target doc,
drop stopwords, and divide the shared content words by the query's content words.

## Run it

From the repo root:

```sh
cargo run -p search-bench --release                                  # default: hash embedder, example set
cargo run -p search-bench --release -- --queries benchmarks/queries.paraphrase.jsonl
cargo run -p search-bench --release --features bge -- --embedder bge  # real BGE-small, 384-d
```

That builds a temp Tantivy index from `corpus/`, runs every ablation over the chosen query set,
prints the table, and writes `benchmarks/results.json`. Add `--per-case` for a per-query
bm25-vs-hybrid diagnostic. Overrides: `--corpus DIR`, `--queries FILE`, `--out FILE`, `--k N`,
`--dim N`, `--ce-depth N`, `--embedder hash|bge`.

`--embedder bge` needs a binary built with `--features bge`; without it the harness fails with an
actionable message rather than silently falling back. The first `bge` run downloads ~128 MB of
weights (see *Real embedding model* in `docs/OPERATIONS.md`).

Tests (drive the same library function the binary uses):

```sh
cargo test -p search-bench                       # 15 tests
cargo test -p search-bench --features bge        # 14 (+1 ignored: needs the weights present)
```

## Ablations

* `bm25` — global lexical BM25 over the Tantivy index.
* `bm25+dense` — RRF(bm25, selected-embedder top-k over title+body).
* `bm25+dense+sparse` — RRF(bm25, dense, source-routed BM25 lane).
* `source-routing` — source-routed BM25 only (no global lane; empty when the query has no source hint).
* `source-routing+global` — RRF(source-routed, global BM25).
* `bm25+ce-on` — bm25 top-50 reranked by the lexical cross-encoder.
* `bm25+dense+sparse+ce-on` — full hybrid top-50 reranked by the lexical cross-encoder.

The `ce-off` counterpart of each `ce-on` row is its parent hybrid row.

## Result A — lexical query set (`queries.example.jsonl`, overlap 0.79)

| ablation                  | nDCG@10 hash | nDCG@10 bge |  MRR hash |  MRR bge | recall@10 | candidates |
|---------------------------|-------------:|------------:|----------:|---------:|----------:|-----------:|
| bm25                      |       0.9702 |      0.9702 |     0.960 |    0.960 |    1.0000 |        9.9 |
| bm25+dense                |       0.9702 |  **0.9713** |     0.960 |    0.960 |    1.0000 |       21.2 |
| bm25+dense+sparse         |   **0.9850** |  **0.9860** | **0.980** | **0.980** | 1.0000 |   21.2 |
| source-routing            |       0.6400 |      0.6400 |     0.640 |    0.640 |    0.6400 |        1.3 |
| source-routing+global     |       0.9842 |      0.9842 |     0.980 |    0.980 |    1.0000 |        7.5 |
| bm25+ce-on                |       0.9412 |      0.9412 |     0.920 |    0.920 |    1.0000 |        9.9 |
| bm25+dense+sparse+ce-on   |       0.9412 |      0.9412 |     0.920 |    0.920 |    1.0000 |       21.2 |

On lexically easy queries BM25 is already near-ceiling and the embedding choice barely moves the
dense lane (0.9702 → 0.9713). The gain over plain BM25 (0.9702 → 0.9850) comes from the
**source-routing** lane on source-qualified queries, not from the dense lane.

## Result B — paraphrase query set (`queries.paraphrase.jsonl`, overlap 0.18)

| ablation                  | nDCG@10 hash | nDCG@10 bge |  MRR hash |  MRR bge | recall@10 hash | recall@10 bge |
|---------------------------|-------------:|------------:|----------:|---------:|---------------:|--------------:|
| bm25                      |       0.4238 |      0.4238 |    0.3082 |   0.3082 |         0.8000 |        0.8000 |
| bm25+dense                |       0.4798 |  **0.5842** |    0.3390 | **0.4824** |        0.8400 |    **0.9200** |
| bm25+dense+sparse         |       0.4798 |  **0.5842** |    0.3390 | **0.4824** |        0.8400 |    **0.9200** |
| source-routing            |       0.0000 |      0.0000 |    0.0000 |   0.0000 |         0.0000 |        0.0000 |
| source-routing+global     |       0.7134 |      0.7134 |    0.6378 |   0.6378 |         0.9200 |        0.9200 |
| bm25+ce-on                |       0.6185 |      0.6185 |    0.5329 |   0.5329 |         0.8600 |        0.8600 |
| bm25+dense+sparse+ce-on   |       0.6066 |      0.6068 |    0.5364 |   0.5368 |         0.8200 |        0.8200 |

This is the set where the embedder matters. When the query and the target share almost no
vocabulary, BM25 collapses (0.4238) and the hashed bag-of-words embedding cannot rescue it
(only 0.4798 — it is lexical too). **The real BGE-small model lifts nDCG@10 to 0.5842 (+38%
relative over BM25), MRR from 0.3082 to 0.4824, and recall@10 from 0.80 to 0.92.** Same corpus,
same query set, same fusion code — the only change is the embedder.

## What this shows

Hybrid retrieval is not "always better"; it is better exactly where the query and the document
do not share words. On the lexical set the dense lane is a near no-op; on the paraphrase set it
is the difference between finding the answer and not. A **lexical** embedding (hash) cannot do
this — which is why the earlier version of this benchmark reported `bm25+dense == bm25` and could
not tell a semantic lane from a bag of words. It can now.

## Honest limitations

* **30 docs / 25 queries is far too small to support any generalisation.** Read the tables as
  "the harness works, the lanes are wired, and the semantic lane moves the right set", not as
  "+0.16 nDCG for BGE anywhere".
* **A trained cross-encoder is UNAVAILABLE.** `LexicalCrossEncoder` (deterministic lexical
  overlap, no model) is the honest proxy. It *hurts* the lexical set (0.9850 → 0.9412) and *helps*
  the paraphrase set (0.4238 → 0.6185): reranking by whole-query token coverage demotes docs that
  are lexically broad but topically right. Both are real findings about the proxy, not tuning
  targets.
* **The two BM25 lanes are different implementations.** The `bm25` row scores through the Tantivy
  index; the `source-routing`/`source-routing+global` lanes score through an in-memory searcher
  (`MemoryLaneSearcher`). They can therefore disagree, which is why `source-routing+global`
  (0.7134) can exceed `bm25` (0.4238) on the paraphrase set even though the global lane conceptually
  retrieves the same surface. Do not read a cross-row difference as a single-scorer effect.
* **Source routing is lexical.** It uses source hints read from the query text; it never consults
  the query `class` or the relevance labels. A query with no source hint has no route
  (`source-routing` = 0.0 on the paraphrase set, none of which name a source).
* **The `bge` lane costs ~10× the memory** in-process: peak RSS 18–26 MB with the hash embedder
  vs ~237 MB with BGE-small/ONNX Runtime loaded. That is the price of the +0.16 nDCG.
