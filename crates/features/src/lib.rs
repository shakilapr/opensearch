//! v2's sparse neural memory: the learned feature layer of `docs/legacy/architecture-update-v2.md` §5.2/§9.
//!
//! The engine has held a semantic *schema* since migration 0005 and semantic *vectors* since
//! Phase B, but neither is a retrieval index: `doc_concept` is a hand-tagged matrix and the vector
//! sidecar is a brute-force scan (measured 1.07 µs/document, i.e. 1.5 s over the 1.39M-document
//! fleet). This crate is the missing middle — the representation PLAN-v2 Phase 3 asks for and
//! EXECUTION-PLAN-v2 Phase D names as "the v2 core":
//!
//! * [`Encoder`] — Model B, `z = TopK(ReLU(Wx+b))`, 4,096 features, 16–32 active, tied decoder,
//!   trained on CPU in minutes from the teacher embeddings the fleet already has.
//! * [`SparseIndex`] — the inverted postings (feature → documents, quantised activation), with the
//!   forward index written to `doc_feature` by `agent0 features build`.
//! * [`FeatureLane`] — Model D, the query side: project the query's teacher embedding through the
//!   same `W` and search the postings, honouring a source scope.
//!
//! What it is *not*: a trained transformer, a learned router, or a replacement for BM25. Each of
//! those is gated in PLAN-v2 §5 on a measurement this crate exists to make possible.

#![forbid(unsafe_code)]

pub mod encoder;
pub mod lane;
pub mod postings;

pub use encoder::{train, Encoder, EncoderError, FeatureHit, TrainOptions};
pub use lane::{FeatureLane, LaneError};
pub use postings::{DocRef, PostingsError, SparseIndex};