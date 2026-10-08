//! Rank fusion and reranking (plan Stage 2, engine §21, engine-arch §10/§16).
//!
//! Reciprocal Rank Fusion over lanes, a linear feature ranker, a cross-encoder interface (with
//! a deterministic lexical default), greedy MMR diversification, and entity disambiguation
//! that keeps competing hypotheses instead of collapsing them early. Interfaces are frozen in
//! `docs/STAGE2_SPEC.md`.

#![forbid(unsafe_code)]
