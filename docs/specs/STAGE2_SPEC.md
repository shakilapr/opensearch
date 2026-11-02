# Stage 2 Spec — Search quality & benchmarks

Frozen interfaces for Stage 2 (see `docs/STAGE1_SPEC.md` for Stage 1). Stop adding volume;
build the measuring stick. Stage-1 crates stay as they are.

## Goal

Hybrid retrieval measurably beats plain BM25 on nDCG@10 / MRR **at a recorded resource
budget**, with the improvement reproducible from a committed benchmark run.

## New crates

| crate dir          | package          | owns                                                  |
|--------------------|------------------|--------------------------------------------------------|
| `crates/eval`      | `search-eval`    | metrics, dataset loading, ablation runner              |
| `crates/embed`     | `search-embed`   | embedding lane (deterministic default + real model behind a feature) |
| `crates/rerank`    | `search-rerank`  | RRF fusion, feature ranker, cross-encoder iface, MMR, disambiguation |

Dependency rule: each depends on `search-core`; `rerank` also on `search-retrieval` and
`search-graph`. Nothing in `core` depends on them. Do not edit crates outside your own.

## Frozen interfaces

### eval (`search-eval`)

```rust
pub struct QueryCase { pub id: String, pub query: String, pub class: QueryClass,
                       pub relevant: Vec<String>,          // doc ids judged relevant
                       pub graded: Vec<(String, u8)>,      // optional graded 0..3
                       pub observed_at: Option<i64> }      // for time-split
pub enum QueryClass { ShortFactual, LongSemantic, AmbiguousEntity, Current, Papers, Docs, Github, Community, MultiHop }
pub struct RunResult { pub case_id: String, pub ranked: Vec<String>, pub latency_ms: u64 }
pub struct Metrics { pub n: usize, pub ndcg_at_10: f64, pub mrr: f64, pub recall_at_10: f64, pub p50_ms: u64, pub p95_ms: u64 }
pub fn ndcg_at_k(ranked: &[String], graded: &[(String,u8)], k: usize) -> f64;
pub fn mrr(ranked: &[String], relevant: &[String]) -> f64;
pub fn recall_at_k(ranked: &[String], relevant: &[String], k: usize) -> f64;
pub fn aggregate(results: &[RunResult], cases: &[QueryCase]) -> Metrics;
/// Load JSONL cases; ignore blank/`#` lines. Reject a case with an empty query.
pub fn load_cases(path: &std::path::Path) -> Result<Vec<QueryCase>, EvalError>;
/// Time-split: cases whose observed_at <= cutoff are "train/static", the rest "dynamic".
pub fn split_by_time(cases: &[QueryCase], cutoff_unix: i64) -> (Vec<QueryCase>, Vec<QueryCase>);
```

`ndcg_at_k` uses graded labels when present (falling back to binary relevance, gain 1);
standard DCG with log2(i+1) discount, ideal DCG from the best available ordering (capped at k).
`mrr` = reciprocal rank of the first relevant result (0.0 if none).

### embed (`search-embed`)

```rust
pub struct Embedding { pub dim: usize, pub v: Vec<f32> }   // L2-normalized
pub trait Embedder: Send + Sync { fn dim(&self) -> usize; fn embed(&self, text: &str) -> Embedding; }
/// Deterministic, dependency-free: hashed bag-of-words (unigrams + bigrams) projected into
/// `dim` buckets with sublinear tf and L2 normalization. Same input -> same vector, always.
pub struct HashEmbedder { /* dim, seed */ }
impl HashEmbedder { pub fn new(dim: usize) -> Self; }
impl Embedder for HashEmbedder { /* ... */ }
pub fn cosine(a: &Embedding, b: &Embedding) -> f32;
pub fn top_k(query: &Embedding, docs: &[(String, Embedding)], k: usize) -> Vec<(String, f32)>;
```

The real BGE-small (~67 MB) model lives behind `feature = "bge"` and must NOT be required to
build or to run the default tests. `HashEmbedder` is the honest default lane: it proves the
dense path works without pretending to be a semantic model.

### rerank (`search-rerank`)

```rust
/// Reciprocal Rank Fusion over lanes. c defaults to 60.
pub struct Rrf { pub c: f64 }
impl Rrf { pub fn new(c: f64) -> Self; pub fn fuse(&self, lanes: &[Vec<String>]) -> Vec<(String, f64)>; }
// fuse: for each lane, rank r (0-based) contributes weight/(c + r + 1); sum per doc;
// return sorted desc by score, ties broken by first-seen order.

pub struct FeatureRanker { pub weights: Vec<f32> }   // linear; input features named below
impl FeatureRanker { pub fn default_llm() -> Self; pub fn score(&self, f: &Features) -> f32; }
pub struct Features { pub bm25: f32, pub dense: f32, pub source_authority: f32,
                      pub freshness: f32, pub entity_match: f32, pub graph_hop: f32, pub title_match: f32 }
pub fn rerank(cands: &[RerankInput], ranker: &FeatureRanker, limit: usize) -> Vec<(String, f32)>;

#[async_trait::async_trait]
pub trait CrossEncoder: Send + Sync { async fn score(&self, query: &str, doc: &str) -> f32; }
/// Default: a deterministic lexical-overlap cross-encoder (no model download).
pub struct LexicalCrossEncoder;
impl CrossEncoder for LexicalCrossEncoder { /* ... */ }

/// Greedy MMR diversification. lambda in [0,1]: 1.0 = pure relevance.
pub fn mmr(ranked: &[(String, f32)], sim: &dyn Fn(&str, &str) -> f32, lambda: f32, limit: usize) -> Vec<(String, f32)>;

/// Entity disambiguation: keep competing interpretations until context resolves.
pub struct Hypothesis { pub entity_id: String, pub label: String, pub score: f32 }
pub struct Disambiguator { pub max_hypotheses: usize }
impl Disambiguator {
    pub fn new(max_hypotheses: usize) -> Self;
    /// Score each candidate interpretation against context; KEEP ALL above `keep_ratio`
    /// of the top score (never collapse to one early). Empty input -> empty output.
    pub fn resolve(&self, mentions: &[String], context: &str, candidates: &[(String, String)]) -> Vec<Hypothesis>;
}
```

### benchmarks/ (data, not a crate)

- `benchmarks/README.md` — schema + how to run.
- `benchmarks/queries.example.jsonl` — a small, **honestly-labelled** seed set (10–30
  cases) tied to the fixture corpus the ablation harness builds. This is a smoke set, not a
  2000-query benchmark; the schema is what a real set would use.
- An ablation binary that: builds a temp index from the fixture corpus, runs each configured
  lane combination, prints a metrics table with a recorded budget (peak RSS if cheap, else
  candidate counts + latency), and writes a machine-readable result file.

## Ablations required

`bm25` · `bm25+dense` · `bm25+dense+sparse` · `source-routing ± global` · `cross-encoder on/off`.

Where a lane cannot be honestly populated in this environment (e.g. real BGE weights, a
trained cross-encoder), the harness must say so in its output rather than fabricate numbers.

## Exit gate

Hybrid retrieval beats plain BM25 on nDCG@10 / MRR on the committed query set, and the
numbers are reproducible via one documented command with the resource budget printed next
to them. Since the seed set is small, the gate is demonstrated on it and the result is
labelled as a smoke result, not a research claim.
