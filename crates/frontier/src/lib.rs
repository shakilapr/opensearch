//! frontier: per-origin priority dispatch with leases and fencing (spec §5).

pub mod lease;
pub mod queue;

pub use lease::{Lease, LeaseError};
pub use queue::{Frontier, Priority};
