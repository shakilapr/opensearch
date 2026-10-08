//! PDF pipeline (plan Stage 1, crawler §23).
//!
//! size check → hash dedup → text-layer test → classifier → GROBID/OCR routing. OCR is an
//! interface only in Stage 1. Interfaces frozen in `docs/STAGE1_SPEC.md`.

#![forbid(unsafe_code)]
