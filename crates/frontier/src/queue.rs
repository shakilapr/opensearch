//! Priority dispatch queue (spec §5). OWNER: frontier agent.

use serde::{Deserialize, Serialize};

/// Scheduler priority tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Priority {
    QueryCritical = 0,
    Hot = 1,
    Normal = 2,
    Discovery = 3,
    Backfill = 4,
}

/// The frontier. Dispatch goes through per-origin queues, never FIFO.
pub struct Frontier;

impl Frontier {
    pub fn new() -> Self {
        Self
    }
}

impl Default for Frontier {
    fn default() -> Self {
        Self::new()
    }
}
