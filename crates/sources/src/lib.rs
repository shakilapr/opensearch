//! Source registry (plan Stage 1, crawler §3).
//!
//! Captures what we know about a source: topics, entities, URL patterns, authority, observed
//! change rate, JS probability, and fetch statistics. Pure types + logic; persistence lives
//! in `search-metadata`. Interfaces frozen in `docs/STAGE1_SPEC.md`.

#![forbid(unsafe_code)]
