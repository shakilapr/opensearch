//! §17 Budgets, deadlines, and stopping. OWNER: engine-plan agent.

use crate::plan::{Mode, StageDeadlines};

/// Why the coordinator stopped (§17).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Deadline,
    BudgetExhausted,
    SufficientCoverage,
    NoAdmissibleAction,
    Cancelled,
    Plateau,
}

/// The atomic budget ledger (§17). Charged before dispatch, never after.
#[derive(Debug, Clone, PartialEq)]
pub struct Budget {
    pub mode: Mode,
    /// Overall server deadline, relative to `started_ms`.
    pub deadline_ms: u64,
    /// Query start, set by the coordinator (`0` until then).
    pub started_ms: u64,
    pub rounds_remaining: u32,
    pub fetches_remaining: u32,
    pub selector_calls_remaining: u32,
    pub rerank_cap: usize,
    pub stage_deadlines: StageDeadlines,
}

impl Budget {
    /// Mode defaults from the §17 table. `deadline_ms` is the *effective* deadline (the client
    /// may impose a tighter one than the mode default; pass the smaller of the two).
    pub fn for_mode(mode: Mode, deadline_ms: u64) -> Self {
        // §17 caps: rounds 1/2/5, live fetches 0/8/30, selector calls 0/1/4, rerank 30/100/300.
        let (rounds, fetches, selectors, rerank) = match mode {
            Mode::Fast => (1, 0, 0, 30),
            Mode::Auto => (2, 8, 1, 100),
            Mode::Deep => (5, 30, 4, 300),
        };
        Self {
            mode,
            deadline_ms,
            started_ms: 0,
            rounds_remaining: rounds,
            fetches_remaining: fetches,
            selector_calls_remaining: selectors,
            rerank_cap: rerank,
            stage_deadlines: StageDeadlines::default(),
        }
    }

    /// Anchor the ledger at the query start time.
    pub fn start(&mut self, now_ms: u64) {
        self.started_ms = now_ms;
    }

    /// Milliseconds left before the deadline (saturating).
    pub fn remaining_ms(&self, now_ms: u64) -> u64 {
        self.deadline_ms.saturating_sub(now_ms.saturating_sub(self.started_ms))
    }

    /// `Some(reason)` when the loop must stop now.
    pub fn should_stop(&self, now_ms: u64) -> Option<StopReason> {
        if self.remaining_ms(now_ms) == 0 {
            return Some(StopReason::Deadline);
        }
        if self.rounds_remaining == 0 {
            return Some(StopReason::BudgetExhausted);
        }
        None
    }

    /// Charge one retrieval round. `false` when nothing is left.
    pub fn charge_round(&mut self) -> bool {
        if self.rounds_remaining == 0 {
            return false;
        }
        self.rounds_remaining -= 1;
        true
    }
}
