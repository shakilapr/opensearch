//! Retrieval lanes (plan Stage 1, engine §17).
//!
//! Source-first retrieval: run BM25 scoped to each source whose profile matches the query,
//! then merge global BM25 and source-scoped candidates. RRF fusion and reranking arrive in
//! Stage 2. Interfaces frozen in `docs/STAGE1_SPEC.md`.

#![forbid(unsafe_code)]
