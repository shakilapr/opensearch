//! Entity / concept / source graph + scholarly WorkId resolution (plan Stage 1, crawler §24,
//! engine §13).
//!
//! In-memory graph of entities, aliases, and typed edges, plus DOI / arXiv / PMID
//! normalization for WorkId resolution. Interfaces frozen in `docs/STAGE1_SPEC.md`.

#![forbid(unsafe_code)]
