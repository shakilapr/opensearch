//! Agent navigation (plan Stage 4, engine §7/§18/§19, engine-arch §12/§15/§17).
//!
//! `SearchState` + action frontier with exact cycle detection, best-first traversal, beam
//! search for ambiguity, evidence joins for multi-hop, and a policy hierarchy (rules →
//! classifier → local model → bounded LLM) that escalates only when a cheaper tier is not
//! sufficient. Interfaces are frozen in `docs/STAGE4_SPEC.md`.

#![forbid(unsafe_code)]
