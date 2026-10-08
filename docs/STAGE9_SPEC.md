# Stage 9 Spec — Advanced retrieval & policy distillation

Frozen interfaces. New crates `search-ann` and `search-distill`, plus a sparse-expansion
addition to `search-retrieval`.

**Honest limits are part of the spec.** Real SPLADE/BGE weights and a trained cross-encoder are
NOT available in this environment (documented already in the Stage-2 bench). Any learned
component here must be (a) deterministic, (b) trained only from committed fixtures, and (c) must
print `UNAVAILABLE` for the artifact-backed path instead of inventing numbers.

## crates/ann

```rust
pub enum Metric { Cosine, Dot, L2 }
pub struct QuantizedVector { pub codes: Vec<i8>, pub scale: f32 }
pub fn quantize_int8(v: &[f32]) -> QuantizedVector;     // scale = max|v|/127
pub fn dequantize(q: &QuantizedVector) -> Vec<f32>;
pub fn dot(a: &[f32], b: &[f32]) -> f32;

/// Inverted-file index: k-means (deterministic init) coarse quantizer + nprobe search.
pub struct IvfIndex;
impl IvfIndex {
    pub fn new(nlist: usize, nprobe: usize, metric: Metric) -> Self;
    pub fn train(&mut self, vecs: &[Vec<f32>], iters: usize);
    pub fn add(&mut self, id: impl Into<String>, vec: Vec<f32>);
    pub fn search(&self, q: &[f32], k: usize) -> Vec<(String, f32)>;
    pub fn len(&self) -> usize;
}

/// Bounded HNSW-lite: layer-0 graph with `m` neighbours and `ef_search` beam.
pub struct HnswLite;
impl HnswLite {
    pub fn new(m: usize, ef_construction: usize, ef_search: usize, metric: Metric) -> Self;
    pub fn insert(&mut self, id: impl Into<String>, vec: Vec<f32>);
    pub fn search(&self, q: &[f32], k: usize) -> Vec<(String, f32)>;
    pub fn recall_at_k(&self, queries: &[(Vec<f32>, Vec<String>)], k: usize) -> f32;
}
```

Both indexes must be deterministic (fixed seeds / insertion-order-stable tie-breaks) and must
recover the exact nearest neighbours on a tiny fixture (where brute force is the oracle).

## crates/retrieval addition

```rust
pub struct SparseExpander;            // built from a committed co-occurrence fixture
impl SparseExpander {
    pub fn from_pairs(pairs: &[(String, String)], max_terms: usize) -> Self;
    pub fn expand(&self, terms: &[String]) -> Vec<(String, f32)>;   // learned-ish term expansion
}
```

## crates/distill

```rust
pub struct LinearPolicy { pub weights: Vec<f32>, pub bias: f32 }
impl LinearPolicy {
    pub fn train(samples: &[(Vec<f32>, String)], classes: &[String], epochs: usize, lr: f32) -> Self;
    pub fn predict(&self, x: &[f32]) -> String;
}
pub fn features_from_state(query: &str, hops: u8, candidates: usize, ambiguity: f32) -> Vec<f32>;
/// Distill a policy tier decision from committed traces; report accuracy on the fixture.
pub fn distill_from_trace(path: &std::path::Path) -> Result<f32, DistillError>;
```

Training must be deterministic and converge on the fixture (report the accuracy actually
achieved, even if it is not 1.0).

## Exit gate

IVF and HNSW-lite match brute-force nearest neighbours on the tiny fixture; int8 quantization
round-trips within a stated error bound; `SparseExpander` expands deterministically and improves
a fixture query's recall; `LinearPolicy` trains deterministically and its measured accuracy is
reported honestly.
