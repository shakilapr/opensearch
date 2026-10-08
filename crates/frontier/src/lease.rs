//! Frontier leases with fencing tokens (spec §5). OWNER: frontier agent.

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error("lease expired")]
    Expired,
    #[error("fencing epoch mismatch: expected {expected}, got {got}")]
    Fenced { expected: i64, got: i64 },
    #[error("not leased by this owner")]
    NotOwner,
}

/// A granted lease. The `epoch` is the fencing token: a worker must present it
/// when committing results, and the writer rejects mismatches.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lease {
    pub resource_id: String,
    pub owner: String,
    pub epoch: i64,
    pub lease_until: i64,
    pub attempt: i64,
}

impl Lease {
    pub fn is_expired(&self, now: i64) -> bool {
        self.lease_until < now
    }
}
