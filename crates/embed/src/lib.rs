//! Embedding lane (plan Stage 2, engine §13).
//!
//! The default [`HashEmbedder`] is deterministic and dependency-free so the dense path can
//! be exercised without a model download; the real BGE-small model sits behind
//! `feature = "bge"`. Interfaces are frozen in `docs/STAGE2_SPEC.md`.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// A dense embedding vector. By contract `v` is L2-normalized (`||v|| == 1`) unless the
/// source text was empty, in which case `v` is the zero vector of length `dim`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Embedding {
    /// Number of dimensions. Always equals `v.len()`.
    pub dim: usize,
    /// The embedding values (L2-normalized, or all-zero for empty input).
    pub v: Vec<f32>,
}

impl Embedding {
    /// The zero vector for `dim` dimensions (used for empty text).
    pub fn zeros(dim: usize) -> Self {
        Embedding {
            dim,
            v: vec![0.0; dim],
        }
    }
}

/// Anything that maps text to a fixed-dimension dense vector. `Send + Sync` so embedders can
/// be shared across lanes/threads.
pub trait Embedder: Send + Sync {
    /// Dimension of every vector this embedder produces.
    fn dim(&self) -> usize;
    /// Embed `text`. Empty/whitespace/punctuation-only input yields the zero vector.
    fn embed(&self, text: &str) -> Embedding;
}

// ---------------------------------------------------------------------------------------------
// HashEmbedder — deterministic hashed bag-of-words (unigrams + bigrams), no model, no deps.
// ---------------------------------------------------------------------------------------------

/// FNV-1a offset basis and prime (64-bit). Fixed constants; the seed is folded in on top.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Domain-separation tags so a unigram and a bigram that happen to spell the same bytes land in
/// different buckets.
const KIND_UNIGRAM: u8 = 0x01;
const KIND_BIGRAM: u8 = 0x02;

/// Owned default seed. Changing it changes every vector, so it is frozen.
const DEFAULT_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// Unique separator inserted between the two tokens of a bigram before hashing. Tokens come from
/// `tokenize`, which only emits `char::is_alphanumeric` characters, so this byte can never occur
/// inside a token — bigrams of different token pairs can never collide at the string level.
const BIGRAM_SEP: char = '\u{1f}';

/// Deterministic, dependency-free embedder: a hashed bag-of-words of unigrams (weight 1.0) and
/// adjacent bigrams (weight 0.5), projected into `dim` buckets with sublinear tf and L2
/// normalization. Same input always produces byte-identical output.
///
/// Exact scheme:
/// 1. **Tokenize**: lowercase each char; split on any non-alphanumeric char; drop empties.
/// 2. **Count**: for every token add `1.0` to `bucket(token, unigram)`; for every adjacent token
///    pair add `0.5` to `bucket(a, b, bigram)`. Counts accumulate in the same `dim`-length array,
///    so a unigram and a bigram can share a bucket.
/// 3. **Sublinear tf**: a bucket with accumulated weight `c > 0` becomes `1 + ln(c)` (`0` stays `0`).
/// 4. **L2-normalize** into `f32`. Empty token list -> zero vector (never a division by zero).
///
/// Bucketing uses FNV-1a over the UTF-8 bytes of the token (or of `a \u{1f} b` for a bigram),
/// prefixed with a per-kind domain byte and seeded with the embedder's seed; the bucket is
/// `hash % dim`.
pub struct HashEmbedder {
    dim: usize,
    seed: u64,
}

impl HashEmbedder {
    /// Create an embedder of the given dimension using the frozen default seed.
    pub fn new(dim: usize) -> Self {
        HashEmbedder {
            dim,
            seed: DEFAULT_SEED,
        }
    }

    /// Create an embedder with an explicit seed (mostly for experimentation; the seed is part of
    /// the embedding identity, so different seeds give different vectors).
    pub fn with_seed(dim: usize, seed: u64) -> Self {
        HashEmbedder { dim, seed }
    }
}

/// FNV-1a over `bytes` with a per-kind prefix and the owner's seed folded into the state.
fn hash_bytes(kind: u8, bytes: &[u8], seed: u64) -> u64 {
    let mut h = FNV_OFFSET ^ seed;
    h ^= kind as u64;
    h = h.wrapping_mul(FNV_PRIME);
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

fn bucket(kind: u8, bytes: &[u8], seed: u64, dim: usize) -> usize {
    (hash_bytes(kind, bytes, seed) % dim as u64) as usize
}

/// Lowercase, split on non-alphanumeric, drop empties.
fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                cur.push(lc);
            }
        } else if !cur.is_empty() {
            tokens.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

impl Embedder for HashEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, text: &str) -> Embedding {
        let dim = self.dim;
        if dim == 0 {
            return Embedding::zeros(0);
        }
        let tokens = tokenize(text);
        if tokens.is_empty() {
            return Embedding::zeros(dim);
        }

        let mut counts = vec![0.0f64; dim];
        for t in &tokens {
            counts[bucket(KIND_UNIGRAM, t.as_bytes(), self.seed, dim)] += 1.0;
        }
        for pair in tokens.windows(2) {
            let bigram = format!("{}{}{}", pair[0], BIGRAM_SEP, pair[1]);
            counts[bucket(KIND_BIGRAM, bigram.as_bytes(), self.seed, dim)] += 0.5;
        }

        // Sublinear tf, then L2-normalize.
        let mut v = vec![0.0f32; dim];
        let mut sumsq = 0.0f64;
        for (i, &c) in counts.iter().enumerate() {
            if c > 0.0 {
                let val = 1.0 + c.ln();
                v[i] = val as f32;
                sumsq += val * val;
            }
        }
        if sumsq > 0.0 {
            let inv = (1.0 / sumsq.sqrt()) as f32;
            for x in v.iter_mut() {
                *x *= inv;
            }
        }
        Embedding { dim, v }
    }
}

/// Cosine similarity. Returns `0.0` if either vector is the zero vector (no division by zero).
/// Inputs are expected to be L2-normalized (as [`Embedder::embed`] produces); the ratio is
/// computed defensively with the norms anyway, so it stays correct for non-normalized inputs,
/// and the result is clamped to `[-1, 1]`. Vectors of differing length are compared over their
/// common prefix.
pub fn cosine(a: &Embedding, b: &Embedding) -> f32 {
    let n = a.v.len().min(b.v.len());
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for i in 0..n {
        let x = a.v[i] as f64;
        let y = b.v[i] as f64;
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= 0.0 || nb <= 0.0 {
        return 0.0;
    }
    let c = dot / (na.sqrt() * nb.sqrt());
    c.clamp(-1.0, 1.0) as f32
}

/// Score every doc against `query` with [`cosine`], return the best `k` sorted by descending
/// score. Ties are broken by ascending `doc.0` (doc id) so the result is deterministic. If
/// `k` exceeds the number of docs, all docs are returned; `k == 0` returns an empty vec.
pub fn top_k(query: &Embedding, docs: &[(String, Embedding)], k: usize) -> Vec<(String, f32)> {
    let mut scored: Vec<(String, f32)> = docs
        .iter()
        .map(|(id, e)| (id.clone(), cosine(query, e)))
        .collect();
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    scored.truncate(k);
    scored
}

// ---------------------------------------------------------------------------------------------
// BGE-small — real model behind a feature. NOT the default lane.
// ---------------------------------------------------------------------------------------------

/// Real BGE-small embedding lane.
///
/// **UNVERIFIED / PLACEHOLDER.** This build does *not* include the ~67 MB BGE-small weights or
/// any tokenizer/model runtime, so this type is intentionally a stub: it exists only to mark the
/// interface and to prove the default build never references model files. It is not compiled by
/// default (behind `feature = "bge"`) and cannot be exercised in this environment.
#[cfg(feature = "bge")]
pub mod bge {
    use super::{Embedder, Embedding};

    /// BGE-small-en produces 384-dimensional vectors.
    pub const BGE_SMALL_DIM: usize = 384;

    /// Error constructing the real embedder.
    #[derive(Debug, thiserror::Error)]
    pub enum BgeError {
        #[error("BGE-small weights are not bundled; the real model path is unverified in this build")]
        WeightsMissing,
    }

    /// Placeholder for the real BGE-small embedder. [`BgeEmbedder::new`] always fails here
    /// because no weights/runtime are shipped; the vector layout is stated but never produced.
    pub struct BgeEmbedder {
        dim: usize,
    }

    impl BgeEmbedder {
        /// A real implementation would load the bundled BGE-small weights here. This stub
        /// returns [`BgeError::WeightsMissing`] — it is a marker, not a working model.
        pub fn new() -> Result<Self, BgeError> {
            Err(BgeError::WeightsMissing)
        }
    }

    impl Embedder for BgeEmbedder {
        fn dim(&self) -> usize {
            self.dim
        }

        fn embed(&self, _text: &str) -> Embedding {
            // Unreachable in practice: `new()` never succeeds without weights. Returning the
            // zero vector keeps the stub panic-free and dependency-free.
            Embedding::zeros(self.dim)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Tests (hermetic: no files, no network, no model).
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn emb(dim: usize, text: &str) -> Embedding {
        HashEmbedder::new(dim).embed(text)
    }

    fn norm(e: &Embedding) -> f32 {
        e.v.iter().map(|x| x * x).sum::<f32>().sqrt()
    }

    #[test]
    fn determinism_same_input_byte_identical() {
        // Fresh embedders, two calls: must be byte-identical, not merely "close".
        let text = "The Quick Brown Fox jumps over the lazy dog, 42 times!";
        let a = HashEmbedder::new(256).embed(text);
        let b = HashEmbedder::new(256).embed(text);
        assert_eq!(a, b);
        let ab: Vec<u32> = a.v.iter().map(|x| x.to_bits()).collect();
        let bb: Vec<u32> = b.v.iter().map(|x| x.to_bits()).collect();
        assert_eq!(ab, bb, "floats must match bit-for-bit");

        // A clone must also reproduce exactly.
        let c = HashEmbedder::new(256);
        assert_eq!(a, c.embed(text));
    }

    #[test]
    fn dim_is_respected() {
        for dim in [1usize, 8, 64, 384, 4096] {
            let e = emb(dim, "some reasonably long piece of text with words");
            assert_eq!(e.dim, dim);
            assert_eq!(e.v.len(), dim);
            assert_eq!(HashEmbedder::new(dim).dim(), dim);
        }
    }

    #[test]
    fn l2_norm_is_one_for_nonempty_text() {
        for dim in [8usize, 64, 384] {
            for text in ["a", "hello world", "one two three four five six seven eight"] {
                let e = emb(dim, text);
                assert!((norm(&e) - 1.0).abs() < 1e-5, "dim {dim} text {text:?}");
            }
        }
    }

    #[test]
    fn empty_text_is_zero_vector_and_zero_cosine() {
        let e = emb(64, "   \t\n  ,,, ... !!! ");
        assert_eq!(e.dim, 64);
        assert_eq!(e.v, vec![0.0f32; 64], "empty tokens -> zero vector");
        assert_eq!(cosine(&e, &e), 0.0, "zero vector -> cosine 0 (guarded)");
        let other = emb(64, "hello");
        assert_eq!(cosine(&e, &other), 0.0);
        assert_eq!(cosine(&other, &e), 0.0);
    }

    #[test]
    fn similar_text_scores_higher_than_unrelated() {
        let q = emb(1024, "the cat sat on the warm mat");
        let similar = emb(1024, "a cat was sitting on a warm mat today");
        let unrelated = emb(1024, "quantum chromodynamics renormalization lattice");
        let rel = cosine(&q, &similar);
        let unrel = cosine(&q, &unrelated);
        assert!(
            rel > unrel,
            "similar {rel} should beat unrelated {unrel}"
        );
        assert!((cosine(&q, &q) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn top_k_orders_and_handles_k_greater_than_docs() {
        let query = emb(1024, "rust programming language memory safety");
        let docs = vec![
            ("d_similar".to_string(), emb(1024, "rust is a programming language focused on memory safety")),
            ("d_somewhat".to_string(), emb(1024, "rust memory")),
            ("d_unrelated".to_string(), emb(1024, "banana bread recipe with walnuts")),
        ];

        let top = top_k(&query, &docs, 2);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].0, "d_similar");
        assert!(top[0].1 >= top[1].1, "sorted descending");
        assert_ne!(top[0].0, "d_unrelated");

        // k > docs: return everything, still sorted.
        let all = top_k(&query, &docs, 99);
        assert_eq!(all.len(), docs.len());
        for w in all.windows(2) {
            assert!(w[0].1 >= w[1].1);
        }

        // k == 0: empty.
        assert!(top_k(&query, &docs, 0).is_empty());
    }

    #[test]
    fn top_k_ties_break_by_doc_id() {
        // Same doc embedding for every id -> all scores equal -> deterministic id order.
        let e = emb(64, "identical text for every document");
        let docs: Vec<(String, Embedding)> = vec![
            ("c".to_string(), e.clone()),
            ("a".to_string(), e.clone()),
            ("b".to_string(), e.clone()),
        ];
        let ids: Vec<String> = top_k(&e, &docs, 3).into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn bigram_order_changes_the_vector() {
        // "red blue green" and "green blue red" have the SAME unigram multiset {red,blue,green},
        // so the unigram contribution is bucket-for-bucket identical. They differ only in their
        // adjacent bigrams:
        //   red blue green -> {red|blue, blue|green}
        //   green blue red -> {green|blue, blue|red}
        // Under our scheme those are distinct hashed strings (BIGRAM_SEP guarantees the string
        // forms differ), so with a large enough dim the bucket sets differ and the L2-normalized
        // vectors are not equal — i.e. bigram order genuinely contributes signal. (This is a
        // fixed, deterministic fact for this scheme/seed/dim; the assertion is true by construction
        // once the chosen dim avoids those two specific bigram hashes colliding.)
        let a = emb(4096, "red blue green");
        let b = emb(4096, "green blue red");
        assert_ne!(a.v, b.v, "shuffling token order must change the vector");
        assert!(
            cosine(&a, &b) < 1.0 - 1e-6,
            "reordered bigrams must not be perfectly identical: {}",
            cosine(&a, &b)
        );
        assert!(cosine(&a, &b) > 0.0, "they still share unigrams");

        // Sanity: identical text (same bigrams) IS identical.
        assert_eq!(emb(4096, "red blue green"), a);
    }

    #[test]
    fn zero_dim_is_safe() {
        let e = emb(0, "anything at all");
        assert_eq!(e.dim, 0);
        assert!(e.v.is_empty());
        assert_eq!(cosine(&e, &e), 0.0);
    }

    #[test]
    fn cosine_guards_and_clamps() {
        // Zero vectors.
        assert_eq!(cosine(&Embedding::zeros(4), &emb(4, "hi")), 0.0);
        // Mismatched dims are compared over the common prefix without panicking.
        let _ = cosine(&emb(8, "hello world"), &emb(64, "hello world"));
    }
}
