//! Rank fusion and reranking (plan Stage 2, engine §21, engine-arch §10/§16).
//!
//! Reciprocal Rank Fusion over lanes, a linear feature ranker, a cross-encoder interface (with
//! a deterministic lexical default), greedy MMR diversification, and entity disambiguation
//! that keeps competing hypotheses instead of collapsing them early. Interfaces are frozen in
//! `docs/STAGE2_SPEC.md`.
//!
//! Nothing here downloads a model or touches the network: every default is deterministic and
//! hermetic so tests are reproducible.

#![forbid(unsafe_code)]

mod cross_encoder;
mod disambiguate;
mod mmr;
mod ranker;
mod rrf;
mod tokenize;

pub use cross_encoder::{lexical_overlap_score, CrossEncoder, LexicalCrossEncoder};
pub use disambiguate::{Disambiguator, Hypothesis, DEFAULT_KEEP_RATIO};
pub use mmr::mmr;
pub use ranker::{rerank, FeatureRanker, Features, RerankInput, DEFAULT_LLM_WEIGHTS, FEATURE_ORDER};
pub use rrf::Rrf;
pub use tokenize::tokenize;
