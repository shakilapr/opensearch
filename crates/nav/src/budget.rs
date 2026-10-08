//! Resource ledger for a single navigation request (engine-arch §17).
//!
//! The ledger is charged **atomically before dispatch** by the [`crate::Navigator`]: an
//! action's cost is reserved, then the executor runs. Time (`used_ms`) is whatever the
//! executor reports into the [`crate::NavContext`] virtual clock, mirrored back here.

/// A budget with hard caps and running counters.
#[derive(Debug, Clone, PartialEq)]
pub struct Budget {
    pub max_actions: u32,
    pub max_ms: u64,
    pub max_cost: f32,
    pub used_actions: u32,
    pub used_ms: u64,
    pub used_cost: f32,
}

impl Budget {
    /// A zeroed budget with the given caps.
    pub fn new(max_actions: u32, max_ms: u64, max_cost: f32) -> Self {
        Self {
            max_actions,
            max_ms,
            max_cost,
            used_actions: 0,
            used_ms: 0,
            used_cost: 0.0,
        }
    }

    /// A practically unbounded budget (tests / callers that enforce limits elsewhere).
    pub fn unbounded() -> Self {
        Self::new(u32::MAX, u64::MAX, f32::MAX)
    }

    /// True when any cap has been reached or exceeded.
    pub fn exhausted(&self) -> bool {
        self.used_actions >= self.max_actions
            || self.used_ms >= self.max_ms
            || self.used_cost >= self.max_cost
    }

    /// True when the budget is not exhausted *and* `cost` still fits under `max_cost`.
    pub fn can_afford(&self, cost: f32) -> bool {
        !self.exhausted() && self.used_cost + cost <= self.max_cost + f32::EPSILON
    }

    /// Charge one action of the given cost (does not touch `used_ms`).
    pub fn charge(&mut self, cost: f32) {
        self.used_actions = self.used_actions.saturating_add(1);
        self.used_cost += cost.max(0.0);
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self::unbounded()
    }
}
