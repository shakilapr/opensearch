//! Scheduler priority tiers (crawler §40, engine §12).

use serde::{Deserialize, Serialize};

/// Scheduler priority tiers, highest first. `rank()` returns `0` for the highest tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Priority {
    /// P0 — query-critical refresh; outranks all background crawling.
    P0QueryCritical,
    /// P1 — hot source updates.
    P1Hot,
    /// P2 — normal background refresh.
    P2Normal,
    /// P3 — discovery (new sources / links).
    P3Discovery,
    /// P4 — archival / backfill.
    P4Backfill,
}

impl Priority {
    /// Numeric rank, `0` = highest priority (P0). Monotone with the enum's `Ord`.
    pub fn rank(&self) -> u8 {
        match self {
            Priority::P0QueryCritical => 0,
            Priority::P1Hot => 1,
            Priority::P2Normal => 2,
            Priority::P3Discovery => 3,
            Priority::P4Backfill => 4,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_rank_order() {
        assert_eq!(Priority::P0QueryCritical.rank(), 0);
        assert_eq!(Priority::P1Hot.rank(), 1);
        assert_eq!(Priority::P2Normal.rank(), 2);
        assert_eq!(Priority::P3Discovery.rank(), 3);
        assert_eq!(Priority::P4Backfill.rank(), 4);

        // rank() is strictly increasing and consistent with the derived ordering.
        let all = [
            Priority::P0QueryCritical,
            Priority::P1Hot,
            Priority::P2Normal,
            Priority::P3Discovery,
            Priority::P4Backfill,
        ];
        for w in all.windows(2) {
            assert!(w[0].rank() < w[1].rank());
            assert!(w[0] < w[1]);
        }
    }
}
