//! MIME-based response router (plan Stage 1, crawler §19).
//!
//! Classifies a fetched response (Content-Type → magic-byte sniff → URL extension) into a
//! route so the pipeline can pick a parser. Interfaces frozen in `docs/STAGE1_SPEC.md`.

#![forbid(unsafe_code)]
