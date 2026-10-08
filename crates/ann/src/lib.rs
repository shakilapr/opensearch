//! ANN indexes (plan Stage 9): int8 quantization, IVF, HNSW-lite.
//! Honest-limit rules are in `docs/STAGE9_SPEC.md`.
//!
//! Everything here is **deterministic**: k-means uses a fixed-seed k-means++ init, all tie
//! breaks are insertion-order stable, and no `HashMap` iteration order is ever observed.
//!
//! Honest limits: these are classical ANN structures over caller-supplied vectors. No learned
//! embedding model, SPLADE/BGE weights or trained cross-encoder is consulted here (see the
//! Stage-2 bench/Stage-9 spec); the indexes only re-order vectors they are given.

#![forbid(unsafe_code)]

use std::cmp::Ordering;

/// Distance / similarity function used to score vectors. For every metric a **larger score is
/// better** (so the nearest neighbour is the largest score), which keeps ranking code uniform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    /// Cosine similarity in `[-1, 1]`.
    Cosine,
    /// Raw inner product.
    Dot,
    /// Negated squared L2 distance (larger = closer).
    L2,
}

impl Metric {
    /// Score `a` against `b`; larger is better. Lengths are truncated to the shorter slice.
    pub fn score(self, a: &[f32], b: &[f32]) -> f32 {
        match self {
            Metric::Cosine => cosine(a, b),
            Metric::Dot => dot(a, b),
            Metric::L2 => -l2_sq(a, b),
        }
    }
}

/// Inner product of two vectors (truncated to the shorter length).
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Cosine similarity; returns `0.0` when either vector has zero norm (never NaN).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let na = dot(a, a).sqrt();
    let nb = dot(b, b).sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot(a, b) / (na * nb)
    }
}

/// Squared L2 distance.
pub fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| {
            let d = x - y;
            d * d
        })
        .sum()
}

/// An int8-quantized vector: `scale = max|v| / 127`, codes in `[-127, 127]`.
#[derive(Debug, Clone, PartialEq)]
pub struct QuantizedVector {
    pub codes: Vec<i8>,
    pub scale: f32,
}

/// Symmetric int8 quantization. The scale is `max|v| / 127`; a zero vector yields `scale == 0`
/// and all-zero codes (no NaN). Per-element round-trip error is at most `scale / 2`.
pub fn quantize_int8(v: &[f32]) -> QuantizedVector {
    let max_abs = v.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    if max_abs == 0.0 || !max_abs.is_finite() {
        // Zero (or degenerate) vector: scale 0, zero codes. Never divides by zero.
        return QuantizedVector {
            codes: vec![0i8; v.len()],
            scale: 0.0,
        };
    }
    let scale = max_abs / 127.0;
    let codes = v
        .iter()
        .map(|x| {
            let q = (x / scale).round();
            // Clamp defensively: rounding can only reach 127 for the max element.
            q.max(-127.0).min(127.0) as i8
        })
        .collect();
    QuantizedVector { codes, scale }
}

/// Reconstruct an approximate `f32` vector from an int8 encoding. A zero-scale encoding
/// (the zero vector) dequantizes back to exact zeros with no NaN.
pub fn dequantize(q: &QuantizedVector) -> Vec<f32> {
    q.codes.iter().map(|&c| c as f32 * q.scale).collect()
}

/// Count code-of-conduct for the int8 round-trip bound: `scale / 2` plus a tiny epsilon for
/// f32 rounding. Exposed so tests and callers assert against the *stated* bound, not a magic
/// number.
pub fn int8_error_bound(scale: f32) -> f32 {
    scale / 2.0 + f32::EPSILON
}

// ---------------------------------------------------------------------------
// Deterministic PRNG (fixed constants; no external `rand` version drift)
// ---------------------------------------------------------------------------

/// A tiny SplitMix64 PRNG. Fully specified constants, so the same seed yields the same stream
/// on every platform and crate version.
#[derive(Debug, Clone)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    fn next_f32(&mut self) -> f32 {
        // 24 high bits -> [0,1).
        ((self.next_u64() >> 40) as f32) / ((1u64 << 24) as f32)
    }
}

/// Fixed k-means++ seed. Chosen once and never varied, so training is reproducible.
const KMEANS_SEED: u64 = 0x5EED_9A17_0000_0001;

// ---------------------------------------------------------------------------
// IVF
// ---------------------------------------------------------------------------

/// Inverted-file index: a k-means coarse quantizer plus an `nprobe` search.
///
/// Vectors are assigned to their nearest centroid (L2 for the coarse quantizer, independent of
/// the ranking metric); a query probes the `nprobe` nearest centroids and scores only their
/// buckets. With `nprobe >= nlist` every bucket is probed, so the result is exactly the brute
/// force ranking.
#[derive(Debug, Clone)]
pub struct IvfIndex {
    nlist: usize,
    nprobe: usize,
    metric: Metric,
    centroids: Vec<Vec<f32>>,
    /// One bucket per centroid; entries are `(insertion_index, vector)`.
    lists: Vec<Vec<(usize, Vec<f32>)>>,
    ids: Vec<String>,
    dim: usize,
}

impl IvfIndex {
    /// A new, untrained index. `nlist` coarse centroids, probe the `nprobe` nearest at query
    /// time. `nlist == 0` is treated as 1.
    pub fn new(nlist: usize, nprobe: usize, metric: Metric) -> Self {
        let nlist = nlist.max(1);
        Self {
            nlist,
            nprobe,
            metric,
            centroids: Vec::new(),
            lists: vec![Vec::new(); nlist],
            ids: Vec::new(),
            dim: 0,
        }
    }

    /// Train the coarse quantizer with k-means (deterministic k-means++ init, `iters` Lloyd
    /// updates). Training on fewer vectors than `nlist` yields one centroid per vector.
    pub fn train(&mut self, vecs: &[Vec<f32>], iters: usize) {
        if vecs.is_empty() {
            self.centroids.clear();
            return;
        }
        self.dim = vecs[0].len();
        let k = self.nlist.min(vecs.len());
        let mut centroids = kmeans_pp_init(vecs, k);

        for _ in 0..iters {
            let mut sums = vec![vec![0.0f32; self.dim]; centroids.len()];
            let mut counts = vec![0usize; centroids.len()];
            for v in vecs {
                let c = nearest_centroid(&centroids, v);
                counts[c] += 1;
                for (acc, x) in sums[c].iter_mut().zip(v.iter()) {
                    *acc += x;
                }
            }
            for (ci, c) in centroids.iter_mut().enumerate() {
                if counts[ci] > 0 {
                    for (j, x) in c.iter_mut().enumerate() {
                        *x = sums[ci][j] / counts[ci] as f32;
                    }
                }
                // Empty bucket: leave the centroid where it was (deterministic).
            }
        }
        self.centroids = centroids;
        // A re-train invalidates existing bucket assignments; keep ids/vectors but re-assign.
        let mut rebalanced: Vec<Vec<(usize, Vec<f32>)>> = vec![Vec::new(); self.nlist];
        for list in &self.lists {
            for (idx, v) in list {
                let c = nearest_centroid(&self.centroids, v);
                rebalanced[c].push((*idx, v.clone()));
            }
        }
        self.lists = rebalanced;
    }

    /// Add a vector under `id`. Called before [`IvfIndex::train`], everything lands in bucket 0.
    pub fn add(&mut self, id: impl Into<String>, vec: Vec<f32>) {
        if self.dim == 0 {
            self.dim = vec.len();
        }
        let idx = self.ids.len();
        self.ids.push(id.into());
        let bucket = if self.centroids.is_empty() {
            0
        } else {
            nearest_centroid(&self.centroids, &vec)
        };
        self.lists[bucket].push((idx, vec));
    }

    /// Number of vectors added.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Whether the index holds no vectors.
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Search the `nprobe` nearest buckets for the top-`k` vectors. Deterministic: ties break
    /// by insertion order, then id.
    pub fn search(&self, q: &[f32], k: usize) -> Vec<(String, f32)> {
        if k == 0 || self.ids.is_empty() {
            return Vec::new();
        }
        let probes = self.buckets_to_probe(q);
        let mut scored: Vec<(usize, f32)> = Vec::new();
        for b in probes {
            for (idx, v) in &self.lists[b] {
                scored.push((*idx, self.metric.score(q, v)));
            }
        }
        rank(scored, &self.ids, k)
    }

    fn buckets_to_probe(&self, q: &[f32]) -> Vec<usize> {
        if self.centroids.is_empty() {
            // Untrained: everything is in bucket 0.
            return vec![0];
        }
        // When nprobe covers every bucket, short-circuit to brute force (exact).
        if self.nprobe >= self.centroids.len() {
            return (0..self.lists.len()).collect();
        }
        let mut dist: Vec<(usize, f32)> = self
            .centroids
            .iter()
            .enumerate()
            .map(|(i, c)| (i, l2_sq(q, c)))
            .collect();
        // Nearest centroid first; ties by centroid index.
        dist.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        dist.into_iter()
            .take(self.nprobe.max(1))
            .map(|(i, _)| i)
            .collect()
    }
}

/// k-means++ init with a fixed seed. First centroid is `vecs[0]`; each subsequent centroid is
/// sampled with probability proportional to squared distance to the nearest chosen centroid.
fn kmeans_pp_init(vecs: &[Vec<f32>], k: usize) -> Vec<Vec<f32>> {
    let mut centroids: Vec<Vec<f32>> = Vec::with_capacity(k);
    centroids.push(vecs[0].clone());
    let mut rng = SplitMix64::new(KMEANS_SEED);

    while centroids.len() < k {
        let mut d2: Vec<f32> = Vec::with_capacity(vecs.len());
        let mut total = 0.0f32;
        for v in vecs {
            let c = nearest_centroid(&centroids, v);
            let d = l2_sq(v, &centroids[c]);
            d2.push(d);
            total += d;
        }
        let pick = if total <= 0.0 {
            // All remaining points coincide with chosen centroids: take the first unchosen.
            (0..vecs.len())
                .find(|&i| d2[i] <= 0.0 && !centroids.iter().any(|c| c == &vecs[i]))
                .unwrap_or(centroids.len().min(vecs.len().saturating_sub(1)))
        } else {
            let target = rng.next_f32() * total;
            let mut acc = 0.0f32;
            let mut chosen = vecs.len() - 1;
            for (i, d) in d2.iter().enumerate() {
                acc += *d;
                if acc >= target {
                    chosen = i;
                    break;
                }
            }
            chosen
        };
        centroids.push(vecs[pick].clone());
    }
    centroids
}

fn nearest_centroid(centroids: &[Vec<f32>], v: &[f32]) -> usize {
    let mut best = 0usize;
    let mut best_d = f32::INFINITY;
    for (i, c) in centroids.iter().enumerate() {
        let d = l2_sq(v, c);
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    best
}

/// Sort `(index, score)` by score desc, then index asc, and map to `(id, score)`.
fn rank(scored: Vec<(usize, f32)>, ids: &[String], k: usize) -> Vec<(String, f32)> {
    let mut scored = scored;
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    scored
        .into_iter()
        .take(k)
        .map(|(i, s)| (ids[i].clone(), s))
        .collect()
}

// ---------------------------------------------------------------------------
// HNSW-lite
// ---------------------------------------------------------------------------

/// A bounded HNSW-lite: a single layer-0 graph with at most `m` neighbours per node and an
/// `ef_search` beam. Insertion uses an `ef_construction` beam to pick neighbours. The graph is
/// kept connected by construction (each new node links to an existing node), so a beam with
/// `ef_search >= n` recovers the exact brute-force ranking on the tiny fixture.
#[derive(Debug, Clone)]
pub struct HnswLite {
    m: usize,
    ef_construction: usize,
    ef_search: usize,
    metric: Metric,
    nodes: Vec<Vec<f32>>,
    ids: Vec<String>,
    /// Symmetric adjacency, kept sorted ascending for determinism.
    adj: Vec<Vec<usize>>,
    entry: Option<usize>,
}

impl HnswLite {
    /// `m` neighbours per node; `ef_construction` beam during insert; `ef_search` beam during
    /// search. `m == 0` is treated as 1.
    pub fn new(m: usize, ef_construction: usize, ef_search: usize, metric: Metric) -> Self {
        Self {
            m: m.max(1),
            ef_construction: ef_construction.max(1),
            ef_search: ef_search.max(1),
            metric,
            nodes: Vec::new(),
            ids: Vec::new(),
            adj: Vec::new(),
            entry: None,
        }
    }

    /// Number of vectors inserted.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the graph is empty.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Insert a vector under `id`.
    pub fn insert(&mut self, id: impl Into<String>, vec: Vec<f32>) {
        let idx = self.nodes.len();
        self.nodes.push(vec);
        self.ids.push(id.into());
        self.adj.push(Vec::new());

        let entry = match self.entry {
            None => {
                self.entry = Some(idx);
                return;
            }
            Some(e) => e,
        };

        // Find candidates for connection via an ef_construction beam over the existing graph.
        let candidates = self.greedy_search(idx, entry, self.ef_construction);
        let neighbours: Vec<usize> = candidates
            .into_iter()
            .filter(|&c| c != idx)
            .take(self.m)
            .collect();
        for &nb in &neighbours {
            self.link(idx, nb);
        }
        for &nb in &neighbours {
            self.prune(nb);
        }
    }

    /// Search for the top-`k` nearest vectors under an `ef_search` beam.
    pub fn search(&self, q: &[f32], k: usize) -> Vec<(String, f32)> {
        if k == 0 || self.nodes.is_empty() {
            return Vec::new();
        }
        let entry = self.entry.expect("non-empty graph has an entry point");
        let beam = self.ef_search.max(k);
        let hits = self.beam_search_vec(q, entry, beam);
        rank(hits, &self.ids, k)
    }

    /// Mean recall@`k` against a set of `(query, ground_truth_ids)` pairs. Ground truth is the
    /// exact nearest-neighbour list (e.g. from brute force). Returns `0.0` for no queries.
    pub fn recall_at_k(&self, queries: &[(Vec<f32>, Vec<String>)], k: usize) -> f32 {
        if queries.is_empty() || k == 0 {
            return 0.0;
        }
        let mut total = 0.0f32;
        for (q, truth) in queries {
            let got = self.search(q, k);
            let got_ids: std::collections::BTreeSet<&str> =
                got.iter().map(|(id, _)| id.as_str()).collect();
            let hit = truth
                .iter()
                .filter(|id| got_ids.contains(id.as_str()))
                .count();
            let denom = truth.len().min(k).max(1);
            total += hit as f32 / denom as f32;
        }
        total / queries.len() as f32
    }

    /// Add an undirected edge `a <-> b` (no self loops, no duplicates).
    fn link(&mut self, a: usize, b: usize) {
        if a == b {
            return;
        }
        if let Err(pos) = self.adj[a].binary_search(&b) {
            self.adj[a].insert(pos, b);
        }
        if let Err(pos) = self.adj[b].binary_search(&a) {
            self.adj[b].insert(pos, a);
        }
    }

    /// Cap a node's degree at `m`, keeping its nearest neighbours. Edges to neighbours that
    /// would otherwise become isolated (degree 1) are protected so the graph stays connected.
    fn prune(&mut self, node: usize) {
        if self.adj[node].len() <= self.m {
            return;
        }
        let mut neighbours: Vec<(usize, f32)> = self.adj[node]
            .iter()
            .map(|&nb| (nb, self.metric.score(&self.nodes[node], &self.nodes[nb])))
            .collect();
        // Best (largest score) first; ties by index for determinism.
        neighbours.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));

        let mut keep: Vec<usize> = Vec::with_capacity(self.m);
        let mut dropped: Vec<usize> = Vec::new();
        for (nb, _) in neighbours {
            if keep.len() < self.m || self.adj[nb].len() <= 1 {
                keep.push(nb);
            } else {
                dropped.push(nb);
            }
        }
        keep.sort_unstable();
        self.adj[node] = keep;
        for d in dropped {
            if let Ok(pos) = self.adj[d].binary_search(&node) {
                self.adj[d].remove(pos);
            }
        }
    }

    /// Beam search from `entry` for the vector at `nodes[query_idx]`, returning up to `ef`
    /// `(index, score)` pairs, best first, with deterministic tie-breaks.
    fn greedy_search(&self, query_idx: usize, entry: usize, ef: usize) -> Vec<usize> {
        self.beam_search_vec(&self.nodes[query_idx].clone(), entry, ef)
            .into_iter()
            .map(|(i, _)| i)
            .collect()
    }

    fn beam_search_vec(&self, q: &[f32], entry: usize, ef: usize) -> Vec<(usize, f32)> {
        let n = self.nodes.len();
        let mut visited = vec![false; n];
        visited[entry] = true;
        let mut results: Vec<(usize, f32)> = vec![(entry, self.metric.score(q, &self.nodes[entry]))];
        let mut frontier: Vec<(usize, f32)> = results.clone();

        while !frontier.is_empty() {
            // Pop the best-scoring frontier node (score desc, index asc).
            frontier.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            let (c, c_score) = frontier.remove(0);
            // If the frontier's best is worse than the worst kept result and the beam is full,
            // stop expanding.
            if results.len() >= ef {
                let worst = results
                    .iter()
                    .map(|(_, s)| *s)
                    .fold(f32::INFINITY, f32::min);
                if c_score < worst {
                    break;
                }
            }
            for &nb in &self.adj[c] {
                if visited[nb] {
                    continue;
                }
                visited[nb] = true;
                let s = self.metric.score(q, &self.nodes[nb]);
                frontier.push((nb, s));
                results.push((nb, s));
                if results.len() > ef {
                    // Drop the single worst result (score asc, index desc).
                    results.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
                    results.truncate(ef);
                }
            }
        }
        results.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        results.truncate(ef);
        results
    }
}

/// Tie-break helper used by [`rank`] so callers can compare candidate lists the same way.
pub fn cmp_score_desc(a: &(String, f32), b: &(String, f32)) -> Ordering {
    b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Tiny fixture: 12 four-dimensional vectors. Brute force is the oracle.
    // -----------------------------------------------------------------
    fn fixture() -> Vec<(String, Vec<f32>)> {
        let rows: [&[f32]; 12] = [
            &[1.0, 0.0, 0.0, 0.0],
            &[0.0, 1.0, 0.0, 0.0],
            &[0.0, 0.0, 1.0, 0.0],
            &[0.0, 0.0, 0.0, 1.0],
            &[1.0, 1.0, 0.0, 0.0],
            &[0.0, 1.0, 1.0, 0.0],
            &[1.0, 0.0, 0.0, 1.0],
            &[2.0, 0.0, 0.0, 0.0],
            &[0.0, 0.0, 2.0, 0.0],
            &[1.0, 1.0, 1.0, 1.0],
            &[3.0, 1.0, 0.0, 0.0],
            &[0.0, 2.0, 1.0, 0.0],
        ];
        rows.iter()
            .enumerate()
            .map(|(i, r)| (format!("v{i}"), r.to_vec()))
            .collect()
    }

    /// Brute-force oracle using the crate's own scoring and the same tie-break rule as the
    /// indexes (score desc, insertion index asc).
    fn brute(q: &[f32], data: &[(String, Vec<f32>)], k: usize, metric: Metric) -> Vec<(String, f32)> {
        let mut scored: Vec<(usize, f32)> = data
            .iter()
            .enumerate()
            .map(|(i, (_, v))| (i, metric.score(q, v)))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        scored
            .into_iter()
            .take(k)
            .map(|(i, s)| (data[i].0.clone(), s))
            .collect()
    }

    fn queries() -> Vec<Vec<f32>> {
        vec![
            vec![1.0, 0.5, 0.0, 0.0],
            vec![0.0, 1.0, 0.5, 0.0],
            vec![0.2, 0.2, 0.2, 0.9],
            vec![1.0, 1.0, 1.0, 1.0],
        ]
    }

    #[test]
    fn int8_round_trip_within_bound() {
        let v = vec![0.0, 0.5, -1.0, 3.25, -2.0, 0.125];
        let q = quantize_int8(&v);
        let max_abs = v.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert_eq!(q.scale, max_abs / 127.0);
        let bound = int8_error_bound(q.scale);
        let back = dequantize(&q);
        assert_eq!(back.len(), v.len());
        for (a, b) in v.iter().zip(back.iter()) {
            assert!(
                (a - b).abs() <= bound,
                "round-trip error {} exceeded bound {}",
                (a - b).abs(),
                bound
            );
        }
    }

    #[test]
    fn int8_zero_vector_is_finite_and_zero_scaled() {
        let q = quantize_int8(&[0.0, 0.0, 0.0]);
        assert_eq!(q.scale, 0.0);
        assert_eq!(q.codes, vec![0, 0, 0]);
        let back = dequantize(&q);
        assert!(back.iter().all(|x| *x == 0.0 && x.is_finite()));
        // A zero-scale bound is just the epsilon.
        assert_eq!(int8_error_bound(0.0), f32::EPSILON);
    }

    #[test]
    fn dot_matches_hand_computed() {
        assert_eq!(dot(&[1.0, 2.0, 3.0], &[4.0, 5.0, 6.0]), 32.0);
        assert_eq!(dot(&[1.0, 2.0], &[3.0]), 3.0); // truncates to shorter
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0); // no NaN
    }

    #[test]
    fn ivf_with_full_probe_recovers_brute_force() {
        let data = fixture();
        let mut ivf = IvfIndex::new(3, 3, Metric::L2); // nprobe == nlist
        let vecs: Vec<Vec<f32>> = data.iter().map(|(_, v)| v.clone()).collect();
        ivf.train(&vecs, 20);
        for (id, v) in &data {
            ivf.add(id.clone(), v.clone());
        }
        assert_eq!(ivf.len(), data.len());
        for q in queries() {
            let got = ivf.search(&q, 5);
            let want = brute(&q, &data, 5, Metric::L2);
            assert_eq!(got, want, "IVF (full probe) must equal brute force for q={q:?}");
        }
    }

    #[test]
    fn ivf_recovers_brute_force_for_each_metric() {
        for metric in [Metric::Cosine, Metric::Dot, Metric::L2] {
            let data = fixture();
            let mut ivf = IvfIndex::new(4, 4, metric);
            let vecs: Vec<Vec<f32>> = data.iter().map(|(_, v)| v.clone()).collect();
            ivf.train(&vecs, 15);
            for (id, v) in &data {
                ivf.add(id.clone(), v.clone());
            }
            for q in queries() {
                assert_eq!(
                    ivf.search(&q, 3),
                    brute(&q, &data, 3, metric),
                    "metric {metric:?} must match brute force"
                );
            }
        }
    }

    #[test]
    fn hnsw_lite_recovers_brute_force_and_recall_is_one() {
        let data = fixture();
        let mut hnsw = HnswLite::new(4, 32, 32, Metric::L2); // ef >= n
        for (id, v) in &data {
            hnsw.insert(id.clone(), v.clone());
        }
        let qs = queries();
        for q in &qs {
            assert_eq!(
                hnsw.search(q, 5),
                brute(q, &data, 5, Metric::L2),
                "HNSW-lite must equal brute force for q={q:?}"
            );
        }
        // recall_at_k against the exact oracle.
        let ground: Vec<(Vec<f32>, Vec<String>)> = qs
            .iter()
            .map(|q| {
                let truth = brute(q, &data, 5, Metric::L2)
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect();
                (q.clone(), truth)
            })
            .collect();
        let recall = hnsw.recall_at_k(&ground, 5);
        assert_eq!(recall, 1.0, "recall@5 on the fixture must be exactly 1.0");
    }

    #[test]
    fn indexes_are_deterministic() {
        let data = fixture();
        let build = || {
            let mut ivf = IvfIndex::new(3, 2, Metric::Cosine);
            let vecs: Vec<Vec<f32>> = data.iter().map(|(_, v)| v.clone()).collect();
            ivf.train(&vecs, 12);
            for (id, v) in &data {
                ivf.add(id.clone(), v.clone());
            }
            let mut hnsw = HnswLite::new(4, 16, 8, Metric::Cosine);
            for (id, v) in &data {
                hnsw.insert(id.clone(), v.clone());
            }
            (ivf, hnsw)
        };
        let (ivf1, h1) = build();
        let (ivf2, h2) = build();
        for q in queries() {
            assert_eq!(ivf1.search(&q, 4), ivf2.search(&q, 4));
            assert_eq!(h1.search(&q, 4), h2.search(&q, 4));
        }
    }

    #[test]
    fn empty_indexes_return_nothing() {
        let ivf = IvfIndex::new(2, 2, Metric::L2);
        assert!(ivf.search(&[1.0, 2.0], 5).is_empty());
        assert_eq!(ivf.len(), 0);
        let hnsw = HnswLite::new(4, 8, 8, Metric::L2);
        assert!(hnsw.search(&[1.0, 2.0], 5).is_empty());
        assert_eq!(hnsw.recall_at_k(&[], 5), 0.0);
    }
}
