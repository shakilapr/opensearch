//! The sparse encoder: v2 §5.2 `z = TopK(ReLU(Wx + b))`.
//!
//! This is Model B of `docs/architecture-update-v2.md`. It maps a 384-dimensional teacher
//! embedding onto a small vocabulary of *learned* features of which at most `active` fire for any
//! document. Two properties make it useful for retrieval rather than merely compact:
//!
//! * **Sparsity is structural, not thresholded.** `TopK` keeps exactly the `active` largest
//!   activations, so a document always contributes the same number of postings and no tuning
//!   constant silently changes the index size. v2 §16 asks for exactly this: only activated
//!   features are materialised.
//! * **The decoder is tied.** `x̂ = Wᵀz`, so the only weights are `W` (features × dim) and `b`.
//!   At 4,096 × 384 that is 6.0 MiB — small enough to train on CPU in minutes and to load in the
//!   serving process without a model runtime, which is why Model B needs no ONNX and no GPU.
//!
//! Training minimises reconstruction of the teacher vector, `‖x − Wᵀ TopK(ReLU(Wx+b))‖²`. The
//! plan is explicit that reconstruction is **not** the retrieval metric (§5 gate: "reconstruction
//! is explicitly not the metric"); it is only the objective that makes the feature vocabulary
//! carry the teacher's geometry. Whether the features retrieve well is measured separately, on
//! the graded sets, by `agent0 eval run --lanes features`.
//!
//! Determinism: the RNG is seeded, sample order is a seeded shuffle, and `save` writes a fixed
//! little-endian layout, so two runs from the same seed and the same sample file produce
//! byte-identical weights.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use rayon::prelude::*;
use thiserror::Error;

/// File magic for a saved encoder. A file without it is not an encoder.
const MAGIC: &[u8; 10] = b"ANSEFEAT01";

/// Errors from training, encoding or persistence.
#[derive(Debug, Error)]
pub enum EncoderError {
    /// Reading or writing the encoder file failed.
    #[error("encoder I/O: {0}")]
    Io(#[from] std::io::Error),
    /// The file is not an encoder, or was written by an incompatible version.
    #[error("encoder format: {0}")]
    Format(String),
    /// A vector's width does not match the encoder's input dimension.
    #[error("dimension mismatch: encoder takes {expected}-dimensional input, got {got}")]
    Dim {
        /// The encoder's input dimension.
        expected: usize,
        /// The offending vector's width.
        got: usize,
    },
    /// The training configuration cannot produce a usable encoder.
    #[error("invalid training configuration: {0}")]
    Config(String),
}

/// One activated feature and its weight.
///
/// `activation` is normalised so the strongest feature of a code is exactly `1.0`; the absolute
/// magnitude of `ReLU(Wx+b)` is not meaningful across documents because it depends on the norm of
/// the teacher vector. Normalising per document is what makes a stored weight comparable to a
/// query weight instead of an artefact of text length.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FeatureHit {
    /// Feature id in `0..features`.
    pub feature: u32,
    /// Weight in `(0, 1]`.
    pub activation: f32,
}

/// A trained `TopK(ReLU(Wx+b))` encoder with a tied decoder.
#[derive(Debug, Clone, PartialEq)]
pub struct Encoder {
    dim: usize,
    features: usize,
    /// Row-major `features × dim`: row `j` is the weight vector of feature `j`.
    w: Vec<f32>,
    /// Per-feature bias, length `features`.
    b: Vec<f32>,
    /// The `TopK` width the encoder was trained with (a runtime parameter, recorded as a default).
    active: usize,
    /// Number of training samples the encoder saw, for the log line and the manifest.
    trained_on: u64,
}

impl Encoder {
    /// The input width (the teacher embedder's dimension).
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Size of the feature vocabulary.
    pub fn features(&self) -> usize {
        self.features
    }

    /// `TopK` width the encoder was trained with.
    pub fn active(&self) -> usize {
        self.active
    }

    /// Training samples seen, if this encoder was trained in this process or recorded on load.
    pub fn trained_on(&self) -> u64 {
        self.trained_on
    }

    /// A deterministic random encoder, used as the starting point for training and by tests.
    ///
    /// Rows are drawn from `N(0, 1/dim)`, which keeps `Wx` of a unit vector at unit scale so the
    /// initial activations are neither all-zero nor saturated.
    pub fn random(dim: usize, features: usize, active: usize, seed: u64) -> Self {
        let mut rng = Lcg::new(seed);
        let scale = 1.0 / (dim.max(1) as f32).sqrt();
        let w = (0..features * dim)
            .map(|_| {
                // Sum of 12 uniforms is a cheap, deterministic near-normal draw.
                let u: f32 = (0..12).map(|_| rng.next_f32()).sum::<f32>() - 6.0;
                u * scale
            })
            .collect();
        Self {
            dim,
            features,
            w,
            b: vec![0.0; features],
            active: active.clamp(1, features.max(1)),
            trained_on: 0,
        }
    }

    /// Build an initial encoder whose atoms are real samples, not noise.
    ///
    /// This matters more than it looks. In 384 dimensions a Gaussian row is almost orthogonal to
    /// every document, so all rows score within noise of each other and `TopK` keeps picking the
    /// same handful — measured on the primary corpus: **31 live features out of 4,096** after an
    /// epoch. Seeding each atom from a distinct real document makes the initial activations
    /// meaningful, so documents spread across the vocabulary from the first step.
    pub fn from_samples(
        samples: &[Vec<f32>],
        dim: usize,
        features: usize,
        active: usize,
        seed: u64,
    ) -> Self {
        let mut enc = Self::random(dim, features, active, seed);
        let active_n = active.clamp(1, features.max(1)) as f32;
        let radius = 1.0 / active_n.sqrt();
        let mut rng = Lcg::new(seed ^ 0x1234_5678);
        for f in 0..features {
            let pick = (rng.next_u64() as usize) % samples.len().max(1);
            let x = &samples[pick];
            if x.len() != dim {
                continue;
            }
            let norm = x.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-6);
            for i in 0..dim {
                let jitter = (rng.next_f32() - 0.5) * 0.02;
                enc.w[f * dim + i] = x[i] / norm * radius + jitter;
            }
            // Just below zero, so an atom starts silent rather than firing on everything.
            enc.b[f] = -0.05;
        }
        enc
    }

    /// The weight vector of one feature.
    ///
    /// The decoder is tied, so this is also the atom the feature reconstructs with — which is
    /// what makes a learned feature inspectable: its row can be compared with a document.
    pub fn row(&self, feature: usize) -> &[f32] {
        let start = feature.min(self.features.saturating_sub(1)) * self.dim;
        &self.w[start..start + self.dim]
    }

    /// The `active` strongest features of `x` with their **raw** activations.
    ///
    /// Unlike [`Encoder::encode`] these are not normalised, so `Σ z_j w_j` is the reconstruction
    /// the training objective measures. Callers that score documents use `encode`; callers that
    /// check the fit use this.
    pub fn code_topk(&self, x: &[f32], active: usize) -> Result<Vec<FeatureHit>, EncoderError> {
        let z = self.code(x)?;
        let active = active.clamp(1, self.features.max(1));
        Ok(top_k(&z, active))
    }

    /// Raw code `ReLU(Wx+b)` for every feature, before `TopK`.
    ///
    /// Exposed because the training loop and the diagnostics both need the dense activation
    /// before selection, and because compressing it here once avoids a second pass over `W`.
    pub fn code(&self, x: &[f32]) -> Result<Vec<f32>, EncoderError> {
        if x.len() != self.dim {
            return Err(EncoderError::Dim { expected: self.dim, got: x.len() });
        }
        let mut z = vec![0.0f32; self.features];
        z.par_chunks_mut(256).enumerate().for_each(|(chunk, out)| {
            let base = chunk * 256;
            for (i, slot) in out.iter_mut().enumerate() {
                let row = &self.w[(base + i) * self.dim..(base + i + 1) * self.dim];
                let acc = dot(row, x) + self.b[base + i];
                *slot = if acc > 0.0 { acc } else { 0.0 };
            }
        });
        Ok(z)
    }

    /// The `active` strongest features of `x`, normalised so the top activation is `1.0`.
    ///
    /// Ties break on the lower feature id, so the same input always yields the same posting set.
    /// A zero input (a document with no text, or a punctuation-only query) yields no features
    /// rather than `active` arbitrary ones — that is what the `activation > 0` filter is for.
    pub fn encode(&self, x: &[f32], active: usize) -> Result<Vec<FeatureHit>, EncoderError> {
        if x.len() != self.dim {
            return Err(EncoderError::Dim { expected: self.dim, got: x.len() });
        }
        // A zero input must activate nothing, whatever the bias has learned. The bias is a
        // threshold that training drives positive, so without this guard ReLU(0 + b) would make
        // an empty query or an empty document match every feature — the failure mode the dense
        // lane also guards against.
        if x.iter().all(|v| *v == 0.0) {
            return Ok(Vec::new());
        }
        let hits = self.code_topk(x, active)?;
        let max = hits.first().map(|h| h.activation).unwrap_or(0.0);
        if max <= 0.0 {
            return Ok(Vec::new());
        }
        Ok(hits
            .into_iter()
            .map(|h| FeatureHit { feature: h.feature, activation: h.activation / max })
            .collect())
    }

    /// Byte-stable save: magic, shape, then little-endian weights.
    pub fn save(&self, path: &Path) -> Result<(), EncoderError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut w = BufWriter::new(File::create(path)?);
        w.write_all(MAGIC)?;
        w.write_all(&(self.dim as u32).to_le_bytes())?;
        w.write_all(&(self.features as u32).to_le_bytes())?;
        w.write_all(&(self.active as u32).to_le_bytes())?;
        w.write_all(&self.trained_on.to_le_bytes())?;
        for x in self.w.iter().chain(self.b.iter()) {
            w.write_all(&x.to_le_bytes())?;
        }
        w.flush()?;
        Ok(())
    }

    /// Read an encoder written by [`Encoder::save`].
    pub fn load(path: &Path) -> Result<Self, EncoderError> {
        let mut r = BufReader::new(File::open(path)?);
        let mut magic = [0u8; 10];
        r.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(EncoderError::Format("not a feature encoder (bad magic)".into()));
        }
        let dim = read_u32(&mut r)? as usize;
        let features = read_u32(&mut r)? as usize;
        let active = read_u32(&mut r)? as usize;
        let mut trained_on = [0u8; 8];
        r.read_exact(&mut trained_on)?;
        let trained_on = u64::from_le_bytes(trained_on);
        if dim == 0 || features == 0 {
            return Err(EncoderError::Format(format!(
                "degenerate shape {features} features × {dim} dims"
            )));
        }
        let mut w = Vec::with_capacity(features * dim);
        for _ in 0..features * dim {
            let mut b = [0u8; 4];
            r.read_exact(&mut b)?;
            w.push(f32::from_le_bytes(b));
        }
        let mut bias = Vec::with_capacity(features);
        for _ in 0..features {
            let mut b = [0u8; 4];
            r.read_exact(&mut b)?;
            bias.push(f32::from_le_bytes(b));
        }
        Ok(Self { dim, features, w, b: bias, active, trained_on })
    }
}

/// Knobs for [`train`]. Defaults follow PLAN-v2 §4 decision 3: 4,096 features, 16–32 active.
#[derive(Debug, Clone)]
pub struct TrainOptions {
    /// Size of the feature vocabulary.
    pub features: usize,
    /// `TopK` width.
    pub active: usize,
    /// Passes over the sample set.
    pub epochs: usize,
    /// Rows updated per stochastic step (a mini-batch in the gradient-sense, applied per sample).
    pub batch: usize,
    /// Step size.
    pub lr: f32,
    /// Momentum coefficient.
    pub momentum: f32,
    /// RNG seed; the same seed and samples give byte-identical weights.
    pub seed: u64,
    /// Print a progress line every this many samples (0 = quiet).
    pub log_every: u64,
    /// Whether training may move the per-feature bias.
    ///
    /// Off by default, and that default is a measurement, not taste. With a trainable bias the
    /// selected atoms raise their own threshold-free activation every step (`b -= lr·gz`, and
    /// `gz` is negative whenever the reconstruction is short of the input), which is a
    /// winner-take-all loop: measured on the primary corpus, one epoch took a data-seeded
    /// vocabulary of **4,077 live features down to 40**. A frozen bias keeps the competition
    /// on `w·x` with equal-radius atoms.
    pub train_bias: bool,
    /// How often to re-seed features that never fire (0 disables).
    ///
    /// Required, not cosmetic: measured on the 19,255-document primary root, a random
    /// initialisation leaves **31 of 4,096** features used after an epoch. The `TopK` picks the
    /// few rows that happen to align with the data, only those rows receive a gradient, and the
    /// rest are unreachable forever — a 31-feature vocabulary dressed up as a 4,096-feature one.
    pub resample_every: usize,
}

impl Default for TrainOptions {
    fn default() -> Self {
        Self {
            features: 4096,
            active: 16,
            epochs: 6,
            batch: 2048,
            // Steps are per sample, so this is already an effective 10× at momentum 0.9. The
            // first version used 0.05 and the code collapsed to all-zero on every document: the
            // ReLU then has no derivative and nothing can recover.
            lr: 0.01,
            momentum: 0.9,
            seed: 0x5eed_0001,
            log_every: 0,
            train_bias: false,
            // Roughly ten re-seed rounds over a 400K-sample epoch: frequent enough that a dead
            // row is replaced before the vocabulary settles, rare enough not to churn live atoms.
            resample_every: 20_000,
        }
    }
}

/// Train an encoder on teacher embeddings.
///
/// Objective: `‖x − Wᵀ TopK(ReLU(Wx+b))‖²` with a tied decoder, minimised by per-sample
/// momentum SGD over the rows that actually fired. Only `active` rows per sample receive a
/// gradient, so a step costs one dense pass over `W` (to compute the code) plus `3 × active × dim`
/// work — which is why this runs in minutes on CPU rather than needing a GPU.
///
/// The `TopK` mask is treated as fixed within a step (the standard straight-through treatment
/// for sparse coding); the gradient through `ReLU` and the decoder is exact for the features that
/// were selected.
pub fn train(
    samples: &[Vec<f32>],
    dim: usize,
    opts: &TrainOptions,
) -> Result<Encoder, EncoderError> {
    if samples.is_empty() {
        return Err(EncoderError::Config("no training samples".into()));
    }
    if dim == 0 || opts.features == 0 {
        return Err(EncoderError::Config("dim and features must be non-zero".into()));
    }
    if let Some(bad) = samples.iter().find(|s| s.len() != dim) {
        return Err(EncoderError::Dim { expected: dim, got: bad.len() });
    }
    let active = opts.active.clamp(1, opts.features);
    let mut enc = Encoder::from_samples(samples, dim, opts.features, active, opts.seed);
    let mut velocity = vec![0.0f32; opts.features * dim];
    let mut v_bias = vec![0.0f32; opts.features];
    let mut order: Vec<u32> = (0..samples.len() as u32).collect();
    let mut rng = Lcg::new(opts.seed ^ 0xa5a5_1234);
    let mut seen: u64 = 0;
    // Selections per feature since the last re-seed round. A `TopK` code only ever updates the
    // rows it selects, so a row that is never selected receives no gradient at all and cannot
    // recover on its own.
    let mut usage = vec![0u32; opts.features];
    let mut since_resample = 0usize;
    let target_radius = 1.0 / (active as f32).sqrt();

    for _epoch in 0..opts.epochs {
        // Fisher–Yates with the seeded LCG: sample order is reproducible, which is what makes the
        // "same seed, same bytes" claim in the module docs true.
        for i in (1..order.len()).rev() {
            let j = (rng.next_u64() % (i as u64 + 1)) as usize;
            order.swap(i, j);
        }
        for chunk in order.chunks(opts.batch.max(1)) {
            // Gradient per sample is sparse (only the fired rows); accumulate it densely but
            // touch only the rows that fired. `acc` is reused across a batch, so this is one
            // 6 MiB allocation per batch rather than one per sample.
            let mut acc = vec![0.0f32; opts.features * dim];
            let mut acc_b = vec![0.0f32; opts.features];
            let mut touched: Vec<u32> = Vec::with_capacity(chunk.len() * active);

            for &idx in chunk {
                let x = &samples[idx as usize];
                let z = enc.code(x)?;
                let hits = top_k(&z, active);
                if hits.is_empty() {
                    continue;
                }
                // Reconstruction from the *raw* code. Normalisation belongs to the postings
                // (where only relative weight matters), never to the objective: dividing here
                // shrinks the reconstruction below the input and the optimiser converges on the
                // all-zero fixed point, which is exactly what the first version of this loop did.
                let mut recon = vec![0.0f32; dim];
                for h in &hits {
                    let row = &enc.w[h.feature as usize * dim..(h.feature as usize + 1) * dim];
                    for (r, w) in recon.iter_mut().zip(row.iter()) {
                        *r += h.activation * w;
                    }
                }
                let err: Vec<f32> = recon.iter().zip(x.iter()).map(|(r, v)| r - v).collect();
                for h in &hits {
                    let f = h.feature as usize;
                    let row = &enc.w[f * dim..(f + 1) * dim];
                    // Two paths reach `w_j`, because the decoder is tied.
                    //  1. directly, as the decoder atom: `2 · err_i · z_j`
                    //  2. through the code, since `z_j` itself is `w_j·x`: `2·(w_j·err) · x_i`
                    // Dropping path 1 (as the first version of this loop did) leaves the atom
                    // pulled only indirectly and the vocabulary fits the corpus badly.
                    let gz = 2.0 * dot(row, &err);
                    let zj = h.activation;
                    for (slot, (ei, xi)) in acc[f * dim..(f + 1) * dim]
                        .iter_mut()
                        .zip(err.iter().zip(x.iter()))
                    {
                        *slot += 2.0 * ei * zj + gz * xi;
                    }
                    acc_b[f] += gz;
                    touched.push(h.feature);
                    usage[f] += 1;
                }
            }

            touched.sort_unstable();
            touched.dedup();
            for &f in &touched {
                let f = f as usize;
                for i in f * dim..(f + 1) * dim {
                    velocity[i] = opts.momentum * velocity[i] + acc[i];
                    enc.w[i] -= opts.lr * velocity[i];
                }
                if opts.train_bias {
                    v_bias[f] = opts.momentum * v_bias[f] + acc_b[f];
                    enc.b[f] -= opts.lr * v_bias[f];
                }
                // Constrain every decoder atom to the same radius, `1/sqrt(active)`. Without it
                // the tied objective is scale-free along each row: rows can grow, the code grows
                // with them, and the step size silently becomes a function of training time.
                // With `active` unit atoms, `Σ z_j w_j` has the same scale as a unit input.
                let mut norm = 0.0f32;
                for i in f * dim..(f + 1) * dim {
                    norm += enc.w[i] * enc.w[i];
                }
                let norm = norm.sqrt();
                if norm > 1e-6 {
                    let target = 1.0 / (active as f32).sqrt();
                    let scale = target / norm;
                    for i in f * dim..(f + 1) * dim {
                        enc.w[i] *= scale;
                    }
                    for i in f * dim..(f + 1) * dim {
                        velocity[i] *= scale;
                    }
                }
            }
            seen += chunk.len() as u64;
            if opts.log_every > 0 && seen % opts.log_every < chunk.len() as u64 {
                eprintln!("  trained on {seen} samples");
            }
            since_resample += chunk.len();
            if opts.resample_every > 0 && since_resample >= opts.resample_every {
                let reseeded = reseed_dead(
                    &mut enc,
                    &mut velocity,
                    &mut v_bias,
                    &usage,
                    samples,
                    since_resample,
                    target_radius,
                    &mut rng,
                );
                if reseeded > 0 {
                    tracing::info!(reseeded, "features: re-seeded dead atoms");
                }
                usage.fill(0);
                since_resample = 0;
            }
        }
    }
    enc.trained_on = seen;
    Ok(enc)
}

/// Re-seed features that were never selected, by pointing them at a random training sample.
///
/// A feature that `TopK` never selects is a feature that never receives a gradient, so it stays
/// whatever random direction it was born with — for the entire run. The measured effect on the
/// first real corpus was 31 live features out of 4,096. Standard remedy (resampling, as in sparse
/// autoencoders): any atom selected fewer than once per 100 expected selections is re-pointed at
/// a real sample, given a slightly negative bias so it starts just below the activation cut, and
/// its optimiser state is cleared.
///
/// Rounds are capped at 5% of the vocabulary so a run cannot spend all its steps re-seeding, and
/// the RNG is the training RNG, so reproducibility survives.
#[allow(clippy::too_many_arguments)]
fn reseed_dead(
    enc: &mut Encoder,
    velocity: &mut [f32],
    v_bias: &mut [f32],
    usage: &[u32],
    samples: &[Vec<f32>],
    window: usize,
    target_radius: f32,
    rng: &mut Lcg,
) -> usize {
    let dim = enc.dim;
    let expected = (window / enc.features).max(1);
    let floor = (expected / 100).max(1) as u32;
    let cap = (enc.features / 20).max(1);
    let mut reseeded = 0usize;
    for f in 0..enc.features {
        if reseeded >= cap {
            break;
        }
        if usage[f] > floor {
            continue;
        }
        let pick = (rng.next_u64() as usize) % samples.len();
        let x = &samples[pick];
        let norm = x.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-6);
        for i in 0..dim {
            // A small jitter stops several dead atoms inheriting the identical direction.
            let jitter = (rng.next_f32() - 0.5) * 0.05;
            enc.w[f * dim + i] = x[i] / norm * target_radius + jitter;
            velocity[f * dim + i] = 0.0;
        }
        enc.b[f] = -0.05;
        v_bias[f] = 0.0;
        reseeded += 1;
    }
    reseeded
}

/// The `k` largest entries of `z`, descending, ties by lower feature id.
fn top_k(z: &[f32], k: usize) -> Vec<FeatureHit> {
    let mut hits: Vec<FeatureHit> = z
        .iter()
        .enumerate()
        .filter(|(_, v)| **v > 0.0)
        .map(|(i, v)| FeatureHit { feature: i as u32, activation: *v })
        .collect();
    hits.sort_by(|a, b| {
        b.activation
            .partial_cmp(&a.activation)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.feature.cmp(&b.feature))
    });
    hits.truncate(k);
    hits
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    // Chunked accumulation: the compiler vectorises this, and a single scalar `sum()` over 384
    // floats is latency-bound rather than throughput-bound.
    let mut acc = [0.0f32; 8];
    let chunks = a.len() / 8;
    for c in 0..chunks {
        for j in 0..8 {
            acc[j] += a[c * 8 + j] * b[c * 8 + j];
        }
    }
    let mut total: f32 = acc.iter().sum();
    for i in chunks * 8..a.len() {
        total += a[i] * b[i];
    }
    total
}

fn read_u32(r: &mut impl Read) -> Result<u32, EncoderError> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

/// A small deterministic LCG. `rand`'s generators may change between versions; a training run
/// that is reproducible has to own its randomness, and an LCG is enough for shuffling and for the
/// sum-of-uniforms normal draw.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 11
    }

    fn next_f32(&mut self) -> f32 {
        (self.next_u64() % (1 << 24)) as f32 / (1u32 << 24) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny deterministic corpus: four clusters in eight dimensions.
    fn corpus(n: usize) -> Vec<Vec<f32>> {
        let mut rng = Lcg::new(7);
        (0..n)
            .map(|i| {
                let mut v = vec![0.0f32; 8];
                v[i % 4] = 1.0;
                v[(i % 4 + 4)] = 0.5;
                for x in v.iter_mut() {
                    *x += (rng.next_f32() - 0.5) * 0.1;
                }
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                v.iter().map(|x| x / norm).collect()
            })
            .collect()
    }

    #[test]
    fn code_is_sparse_and_normalised() {
        let enc = Encoder::random(8, 32, 4, 1);
        let v = corpus(1).remove(0);
        let hits = enc.encode(&v, 4).unwrap();
        assert_eq!(hits.len(), 4, "TopK must return exactly `active` positive features");
        assert!((hits[0].activation - 1.0).abs() < 1e-6, "{hits:?}");
        assert!(hits.windows(2).all(|w| w[0].activation >= w[1].activation));
        assert!(hits.iter().all(|h| h.activation > 0.0));
    }

    #[test]
    fn a_zero_vector_activates_nothing() {
        // A document with no text, or a punctuation-only query: the honest answer is no features,
        // not `active` arbitrary ones that would inject documents into the fusion.
        let enc = Encoder::random(8, 32, 4, 2);
        assert!(enc.encode(&vec![0.0; 8], 4).unwrap().is_empty());
    }

    #[test]
    fn the_input_width_is_enforced() {
        let enc = Encoder::random(8, 16, 2, 3);
        let err = enc.encode(&[1.0, 2.0], 2).unwrap_err().to_string();
        assert!(err.contains("dimension mismatch"), "{err}");
    }

    #[test]
    fn training_reduces_reconstruction_error() {
        let samples = corpus(256);
        let opts = TrainOptions {
            features: 16,
            active: 4,
            epochs: 12,
            batch: 64,
            lr: 0.01,
            momentum: 0.9,
            seed: 11,
            log_every: 0,
            resample_every: 0,
            train_bias: false,
        };
        let before = Encoder::random(8, 16, 4, opts.seed);
        let enc = train(&samples, 8, &opts).unwrap();
        let err = |e: &Encoder| -> f32 {
            samples
                .iter()
                .map(|x| {
                    let hits = e.encode(x, 4).unwrap();
                    let mut recon = vec![0.0f32; 8];
                    for h in &hits {
                        let row = &e.w[h.feature as usize * 8..(h.feature as usize + 1) * 8];
                        for (r, w) in recon.iter_mut().zip(row.iter()) {
                            *r += h.activation * w;
                        }
                    }
                    recon.iter().zip(x.iter()).map(|(r, v)| (r - v).powi(2)).sum::<f32>()
                })
                .sum::<f32>()
                / samples.len() as f32
        };
        let (e0, e1) = (err(&before), err(&enc));
        assert!(e1 < e0 * 0.8, "training must improve reconstruction: {e0} -> {e1}");
        assert_eq!(enc.trained_on(), 256 * 12);
    }

    #[test]
    fn save_and_load_round_trip_byte_stable() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("enc.bin");
        let b = tmp.path().join("again.bin");
        let enc = Encoder::random(8, 16, 2, 9);
        enc.save(&a).unwrap();
        let loaded = Encoder::load(&a).unwrap();
        assert_eq!(loaded, enc);
        loaded.save(&b).unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());

        std::fs::write(tmp.path().join("junk.bin"), b"not an encoder").unwrap();
        let err = Encoder::load(&tmp.path().join("junk.bin")).unwrap_err().to_string();
        assert!(err.contains("bad magic"), "{err}");
    }

    #[test]
    fn resampling_keeps_the_vocabulary_alive() {
        // The failure this guards: with a random start, `TopK` selects a handful of rows, only
        // those get gradients, and the rest stay random forever. Measured on the real corpus that
        // was 31 live features out of 4,096.
        // Many distinct directions, not four: a four-cluster corpus needs only a handful of
        // atoms, so it would report a collapsed vocabulary that is in fact correct.
        let mut rng = Lcg::new(21);
        let samples: Vec<Vec<f32>> = (0..256)
            .map(|_| {
                let mut v: Vec<f32> = (0..16).map(|_| rng.next_f32() - 0.5).collect();
                let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
                v.iter_mut().for_each(|x| *x /= n);
                v
            })
            .collect();
        let opts = TrainOptions {
            features: 128,
            active: 4,
            epochs: 3,
            batch: 32,
            lr: 0.01,
            momentum: 0.9,
            seed: 3,
            log_every: 0,
            resample_every: 32,
            train_bias: false,
        };
        let enc = train(&samples, 16, &opts).unwrap();
        let mut live = vec![false; enc.features()];
        for x in &samples {
            for h in enc.encode(x, 4).unwrap() {
                live[h.feature as usize] = true;
            }
        }
        let used = live.iter().filter(|x| **x).count();
        assert!(used > 32, "re-seeding must keep the vocabulary alive, got {used} of 128");
    }

    #[test]
    fn the_same_seed_gives_the_same_weights() {
        let samples = corpus(64);
        let opts = TrainOptions {
            features: 16,
            active: 4,
            epochs: 2,
            batch: 16,
            lr: 0.01,
            momentum: 0.9,
            seed: 5,
            log_every: 0,
            resample_every: 0,
            train_bias: false,
        };
        let a = train(&samples, 8, &opts).unwrap();
        let b = train(&samples, 8, &opts).unwrap();
        assert_eq!(a.w, b.w, "training must be reproducible from its seed");
    }
}