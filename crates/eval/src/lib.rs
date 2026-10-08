//! Benchmarking: metrics, dataset loading, and the ablation runner (plan Stage 2).
//!
//! Gives hybrid retrieval a measuring stick — nDCG / MRR / recall over a committed query
//! set, with the resource budget recorded next to every number. Interfaces are frozen in
//! `docs/STAGE2_SPEC.md`.

#![forbid(unsafe_code)]
