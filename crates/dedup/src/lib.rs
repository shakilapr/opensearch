//! Near-duplicate detection (plan Stage 5): SimHash64, MinHash + LSH banding, shingling.
//! Interfaces frozen in `docs/STAGE5_SPEC.md`.
//!
//! All hashing here is a fixed, dependency-free FNV-1a/64-bit mixer with constant seeds and
//! `BTreeMap`/`BTreeSet` containers, so every result is deterministic and stable across runs
//! (no `HashMap` iteration order, no randomness).

#![forbid(unsafe_code)]

pub mod minhash;
pub mod neardup;
pub mod shingles;
pub mod simhash;

pub use minhash::{lsh_bands, MinHash};
pub use neardup::NearDupIndex;
pub use shingles::shingles;
pub use simhash::SimHash64;
