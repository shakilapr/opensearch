//! Embedding lane (plan Stage 2, engine §13).
//!
//! The default [`HashEmbedder`] is deterministic and dependency-free so the dense path can
//! be exercised without a model download; the real BGE-small model sits behind
//! `feature = "bge"`. Interfaces are frozen in `docs/STAGE2_SPEC.md`.

#![forbid(unsafe_code)]

pub mod store;

pub use store::{StoreError, VectorStore};

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

/// Real BGE-small embedding lane (`BAAI/bge-small-en-v1.5`, 384-d).
///
/// Compiled only under `feature = "bge"`, which is the *only* thing that pulls in ONNX Runtime
/// and the model runtime. The default build therefore neither links nor downloads a model, and
/// `cargo test --workspace` stays hermetic.
///
/// The weights are fetched on **first construction** (Hugging Face Hub → cache dir) and loaded
/// from disk afterwards; see [`bge::default_cache_dir`]. Nothing here is bundled in the repo.
#[cfg(feature = "bge")]
pub mod bge {
    use super::{Embedder, Embedding};
    use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// BGE-small-en produces 384-dimensional vectors.
    pub const BGE_SMALL_DIM: usize = 384;

    /// The upstream model this module loads. Part of the embedding identity: different weights
    /// produce different vectors, so it is reported alongside any result.
    pub const BGE_SMALL_MODEL: &str = "BAAI/bge-small-en-v1.5";

    /// The retrieval instruction BGE-small-en-v1.5 is trained to expect on the **query** side.
    /// Upstream guidance is to prepend it to short queries and NOT to passages; it is applied by
    /// [`BgeEmbedder::embed_query`] and never by [`Embedder::embed`].
    pub const BGE_QUERY_INSTRUCTION: &str =
        "Represent this sentence for searching relevant passages: ";

    /// Error constructing or running the real embedder.
    #[derive(Debug, thiserror::Error)]
    pub enum BgeError {
        /// The model could not be fetched or loaded (no network, no cache, bad cache dir).
        #[error("failed to load {model}: {detail}")]
        Load {
            /// Model identifier that failed to load.
            model: &'static str,
            /// Underlying runtime error, as text (the runtime's error type is not `Error`).
            detail: String,
        },
        /// The model loaded but inference failed.
        #[error("inference failed: {0}")]
        Infer(String),
        /// The runtime produced a vector of unexpected width, so the frozen dimension is wrong.
        #[error("model returned {got} dimensions, expected {expected}")]
        Dim {
            /// Width actually produced.
            got: usize,
            /// Width the crate's contract promises.
            expected: usize,
        },
    }

    /// Where the weights live when nothing overrides it: `<workspace root>/models/fastembed`,
    /// i.e. the repo's gitignored `models/` tree that already holds server-only artifacts.
    /// Override with `SEARCH_MODEL_CACHE`.
    ///
    /// The workspace root is *searched for* rather than assumed to be the working directory,
    /// because `cargo test -p <crate>` runs with the CWD set to that crate's directory — a plain
    /// relative path would give every crate its own 130 MB copy of the same model.
    pub fn default_cache_dir() -> PathBuf {
        if let Ok(p) = std::env::var("SEARCH_MODEL_CACHE") {
            if !p.trim().is_empty() {
                return PathBuf::from(p);
            }
        }
        match workspace_root() {
            Some(root) => root.join("models").join("fastembed"),
            None => PathBuf::from("models/fastembed"),
        }
    }

    /// Walk up from the current directory to the nearest `Cargo.toml` declaring `[workspace]`.
    fn workspace_root() -> Option<PathBuf> {
        let mut dir = std::env::current_dir().ok()?;
        loop {
            if let Ok(text) = std::fs::read_to_string(dir.join("Cargo.toml")) {
                if text.contains("[workspace]") {
                    return Some(dir);
                }
            }
            if !dir.pop() {
                return None;
            }
        }
    }

    /// Real BGE-small-en-v1.5 embedder.
    ///
    /// Holds one ONNX session behind a `Mutex` because the underlying `embed` takes `&mut self`
    /// while the [`Embedder`] trait (frozen in `docs/STAGE2_SPEC.md`) is `&self` + `Send + Sync`.
    /// Inference is CPU-bound; the lock serializes calls rather than corrupting them.
    pub struct BgeEmbedder {
        model: Mutex<TextEmbedding>,
        dim: usize,
    }

    impl BgeEmbedder {
        /// Load BGE-small from [`default_cache_dir`], downloading it on first use.
        pub fn new() -> Result<Self, BgeError> {
            Self::with_cache_dir(default_cache_dir())
        }

        /// Load BGE-small with an explicit cache directory (used by tests and by callers that
        /// want the weights somewhere specific), downloading on first use.
        pub fn with_cache_dir(cache_dir: PathBuf) -> Result<Self, BgeError> {
            Self::build(cache_dir, false)
        }

        /// As [`BgeEmbedder::with_cache_dir`], but shows download progress on stderr.
        pub fn with_cache_dir_and_progress(cache_dir: PathBuf) -> Result<Self, BgeError> {
            Self::build(cache_dir, true)
        }

        fn build(cache_dir: PathBuf, progress: bool) -> Result<Self, BgeError> {
            let opts = TextInitOptions::new(EmbeddingModel::BGESmallENV15)
                .with_cache_dir(cache_dir)
                .with_show_download_progress(progress);
            let model = TextEmbedding::try_new(opts).map_err(|e| BgeError::Load {
                model: BGE_SMALL_MODEL,
                detail: e.to_string(),
            })?;
            Ok(BgeEmbedder {
                model: Mutex::new(model),
                dim: BGE_SMALL_DIM,
            })
        }

        /// The upstream model identifier behind this instance.
        pub fn model_id(&self) -> &'static str {
            BGE_SMALL_MODEL
        }

        /// Embed a **query**, prepending the instruction BGE-small-en-v1.5 was trained with.
        /// This is the asymmetric half of the model; [`Embedder::embed`] is the passage half.
        /// The [`Embedder`] trait cannot express the query/passage asymmetry (it is one method),
        /// so callers that rank query→passage should call this for the query.
        pub fn embed_query(&self, text: &str) -> Result<Embedding, BgeError> {
            let prefixed = format!("{BGE_QUERY_INSTRUCTION}{text}");
            self.embed_raw(&prefixed)
        }

        /// Embed several passages in one batched call (much faster than a loop over
        /// [`Embedder::embed`] for corpus-scale input). Empty inputs still come back as zero
        /// vectors, in the same order.
        pub fn embed_batch<S: AsRef<str>>(&self, texts: &[S]) -> Result<Vec<Embedding>, BgeError> {
            // Replace "empty" (no alphanumeric) inputs with a placeholder we never use: the
            // model may reject or mis-handle an empty string, and the contract says zero vector.
            let mut dims = Vec::with_capacity(texts.len());
            let mut payload = Vec::with_capacity(texts.len());
            for t in texts {
                let s = t.as_ref();
                dims.push(has_content(s));
                payload.push(if has_content(s) { s } else { "." });
            }
            let mut guard = self.model.lock().map_err(|_| BgeError::Infer("model lock poisoned".into()))?;
            let raw = guard
                .embed(&payload, None)
                .map_err(|e| BgeError::Infer(e.to_string()))?;
            drop(guard);

            if raw.len() != texts.len() {
                return Err(BgeError::Infer(format!(
                    "model returned {} vectors for {} inputs",
                    raw.len(),
                    texts.len()
                )));
            }
            let mut out = Vec::with_capacity(texts.len());
            for (i, v) in raw.into_iter().enumerate() {
                if !dims[i] {
                    out.push(Embedding::zeros(self.dim));
                    continue;
                }
                out.push(finish(v, self.dim)?);
            }
            Ok(out)
        }

        /// Single-text embedding with **no** instruction prefix (passage side).
        fn embed_raw(&self, text: &str) -> Result<Embedding, BgeError> {
            if !has_content(text) {
                return Ok(Embedding::zeros(self.dim));
            }
            let mut guard = self.model.lock().map_err(|_| BgeError::Infer("model lock poisoned".into()))?;
            let mut raw = guard
                .embed(vec![text], None)
                .map_err(|e| BgeError::Infer(e.to_string()))?;
            drop(guard);
            let v = raw
                .pop()
                .ok_or_else(|| BgeError::Infer("model returned no vector".into()))?;
            finish(v, self.dim)
        }
    }

    impl Embedder for BgeEmbedder {
        fn dim(&self) -> usize {
            self.dim
        }

        /// Passage-side embedding (no query instruction). Empty/whitespace/punctuation-only
        /// input yields the zero vector, matching [`super::HashEmbedder`]'s contract.
        fn embed(&self, text: &str) -> Embedding {
            // The trait is infallible; a runtime failure would otherwise silently look like
            // "nothing matched". Zero is the documented degenerate answer, and `try_embed`
            // exists for callers that need the error.
            self.embed_raw(text).unwrap_or_else(|_| Embedding::zeros(self.dim))
        }
    }

    impl BgeEmbedder {
        /// Fallible [`Embedder::embed`] — use this when a silent zero vector would be a bug
        /// rather than an acceptable answer.
        pub fn try_embed(&self, text: &str) -> Result<Embedding, BgeError> {
            self.embed_raw(text)
        }
    }

    /// True if the text has at least one alphanumeric char (same rule as the hash tokenizer).
    fn has_content(text: &str) -> bool {
        text.chars().any(|c| c.is_alphanumeric())
    }

    /// Check the width and L2-normalize (the contract promises a unit vector; the runtime
    /// already normalizes, this is a defensive, idempotent second pass).
    fn finish(mut v: Vec<f32>, dim: usize) -> Result<Embedding, BgeError> {
        if v.len() != dim {
            return Err(BgeError::Dim {
                got: v.len(),
                expected: dim,
            });
        }
        let sumsq: f64 = v.iter().map(|x| (*x as f64) * (*x as f64)).sum();
        if sumsq > 0.0 {
            let inv = (1.0 / sumsq.sqrt()) as f32;
            for x in v.iter_mut() {
                *x *= inv;
            }
        }
        Ok(Embedding { dim, v })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::OnceLock;

        /// One model for the whole test binary. `get_or_init` makes concurrent tests block on the
        /// first construction instead of racing each other into the same Hugging Face download
        /// (four threads calling `try_new` at once genuinely fails with "Failed to retrieve model
        /// file 'onnx/model.onnx'"). The `Result` is cached too, so a failed load is reported
        /// once rather than retried per test.
        static MODEL: OnceLock<Result<BgeEmbedder, String>> = OnceLock::new();

        fn load() -> &'static BgeEmbedder {
            match MODEL.get_or_init(|| BgeEmbedder::new().map_err(|e| e.to_string())) {
                Ok(m) => m,
                Err(e) => panic!(
                    "BGE-small could not be loaded: {e}\n\
                     This test needs the model; the first run downloads it into the cache dir\n\
                     (`models/fastembed`, or $SEARCH_MODEL_CACHE). Run:\n\
                     \x20 cargo test -p search-embed --features bge"
                ),
            }
        }

        /// The whole point of the feature: on a genuine paraphrase (no content-word overlap) the
        /// hashed bag-of-words has almost nothing to go on, while a real semantic model still
        /// finds the right passage.
        #[test]
        fn paraphrase_beats_lexical_bag_of_words() {
            let bge = load();
            let hash = super::super::HashEmbedder::new(BGE_SMALL_DIM);

            let query = "how do I stop my program freezing while it waits for the network";
            let relevant = "Asynchronous runtimes keep a thread free while I/O is pending, so the \
                            event loop is never blocked by a slow request.";
            let unrelated = "Sourdough needs a warm room and twelve hours of patience.";

            let q = bge.embed_query(query).expect("query embedding");
            let rel = bge.embed(relevant);
            let unrel = bge.embed(unrelated);

            let rel_score = super::super::cosine(&q, &rel);
            let unrel_score = super::super::cosine(&q, &unrel);
            assert!(
                rel_score > unrel_score,
                "BGE should prefer the on-topic passage: {rel_score} vs {unrel_score}"
            );
            assert!(
                rel_score > 0.5,
                "BGE should be clearly confident here, got {rel_score}"
            );

            // The lexical lane: the only tokens these two strings share are stopwords
            // ("while", "the"), so it has no signal — it sits in a low, noisy band and, on this
            // pair, does not separate on-topic from off-topic the way BGE does.
            let hq = hash.embed(query);
            let h_rel = super::super::cosine(&hq, &hash.embed(relevant));
            let h_unrel = super::super::cosine(&hq, &hash.embed(unrelated));
            eprintln!(
                "BGE: rel={rel_score:.4} unrel={unrel_score:.4} | hash: rel={h_rel:.4} unrel={h_unrel:.4}"
            );
            assert!(
                h_rel < 0.5,
                "stopword overlap only — the hash lane should stay low, got {h_rel}"
            );
            assert!(
                rel_score > h_rel + 0.2,
                "the semantic lane should be markedly more confident than the lexical one: \
                 {rel_score} vs {h_rel}"
            );
        }

        #[test]
        fn dim_is_384_and_vectors_are_unit_length() {
            let bge = load();
            assert_eq!(bge.dim(), BGE_SMALL_DIM);
            let e = bge.embed("a passage about indexing");
            assert_eq!(e.dim, BGE_SMALL_DIM);
            assert_eq!(e.v.len(), BGE_SMALL_DIM);
            let norm: f32 = e.v.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-4, "L2 norm was {norm}");
        }

        #[test]
        fn empty_and_punctuation_only_are_zero_vectors() {
            let bge = load();
            for text in ["", "   ", "\t\n", ",,, ... !!!"] {
                let e = bge.embed(text);
                assert_eq!(e.dim, BGE_SMALL_DIM);
                assert_eq!(e.v, vec![0.0f32; BGE_SMALL_DIM], "text {text:?}");
            }
        }

        #[test]
        fn batch_matches_single_and_preserves_order() {
            let bge = load();
            let texts = ["alpha passage about cars", "", "beta passage about cooking"];
            let batch = bge.embed_batch(&texts).expect("batch");
            assert_eq!(batch.len(), 3);
            assert_eq!(batch[1].v, vec![0.0f32; BGE_SMALL_DIM]);
            for (i, t) in texts.iter().enumerate() {
                if !t.is_empty() {
                    assert_eq!(batch[i], bge.embed(t), "row {i} must match the single call");
                }
            }
        }

        #[test]
        fn query_instruction_changes_the_vector_and_is_used_for_queries() {
            let bge = load();
            let text = "how do I make my crawler polite";
            let passage = bge.embed(text);
            let query = bge.embed_query(text).expect("query");
            // The instruction is real input to the model, not decoration.
            assert_ne!(passage.v, query.v, "the query instruction must change the vector");
            assert_eq!(query.dim, BGE_SMALL_DIM);
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
