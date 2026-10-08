//! frontier: per-origin priority dispatch with leases and fencing (spec §5).
//!
//! [`Frontier`] is the dispatch core: a priority-ordered, per-origin-gated
//! queue over the metadata SQLite database. Every dispatch carries an `epoch`
//! fencing token, every channel is bounded, and expired leases are recovered.

pub mod backpressure;
pub mod lease;
pub mod queue;

pub use backpressure::{bounded, Stage};
pub use lease::{Lease, LeaseError};
pub use queue::{Frontier, FrontierConfig, FrontierEntry, Priority};
