//! Embedding lane (plan Stage 2, engine §13).
//!
//! The default [`HashEmbedder`] is deterministic and dependency-free so the dense path can
//! be exercised without a model download; the real BGE-small model sits behind
//! `feature = "bge"`. Interfaces are frozen in `docs/STAGE2_SPEC.md`.

#![forbid(unsafe_code)]
