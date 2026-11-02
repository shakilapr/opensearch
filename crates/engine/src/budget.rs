//! §17 Budgets, deadlines, and stopping. OWNER: engine-plan agent.
//!
//! Implements the per-mode budget ledger, the deadline accounting, and the first-applicable
//! stopping rule from `docs/legacy/engine-architecture.md` §17. Frozen contract:
//! `docs/specs/ENGINE_SPEC.md` §17.
//!
//! §17: "Stop on deadline or budget exhaustion, sufficient supported coverage, no admissible
//! positive-value action, or explicit cancellation." The ledger is charged before dispatch:
//! §17 "Charge the ledger atomically before dispatch".

use crate::plan::{split_stage_deadlines, Mode, StageDeadlines};

/// Why the coordinator stopped (§17).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Deadline,
    BudgetExhausted,
    SufficientCoverage,
    /// The result set reached the requested `limit` (§17): the request was satisfied, so the
    /// loop stopped even though more candidates or rounds may have remained. Reported ahead of
    /// [`StopReason::SufficientCoverage`] and [`StopReason::BudgetExhausted`], behind
    /// [`StopReason::Deadline`].
    LimitReached,
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
    ///
    /// §17 caps: rounds 1/2/5, live fetches 0/8/30, selector calls 0/1/4, rerank 30/100/300.
    pub fn for_mode(mode: Mode, deadline_ms: u64) -> Self {
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
            // §17 "Reserve finalization time before scheduling work": stage deadlines are
            // derived from the *effective* deadline and sum to no more than it.
            stage_deadlines: split_stage_deadlines(deadline_ms),
        }
    }

    /// Anchor the ledger at the query start time.
    pub fn start(&mut self, now_ms: u64) {
        self.started_ms = now_ms;
    }

    /// Milliseconds left before the deadline (saturating). A `now_ms` before `started_ms`
    /// yields the full deadline rather than underflowing.
    pub fn remaining_ms(&self, now_ms: u64) -> u64 {
        self.deadline_ms
            .saturating_sub(now_ms.saturating_sub(self.started_ms))
    }

    /// `Some(reason)` when the loop must stop now. Returns the FIRST applicable reason:
    /// [`StopReason::Deadline`] before [`StopReason::BudgetExhausted`] (§17 lists "deadline or
    /// budget exhaustion" in that order).
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

    /// §17 stopping rule "no admissible positive-value action": the coordinator calls this when
    /// its action frontier holds no admissible action that could resolve an open gap. The
    /// coordinator is responsible for establishing that the frontier is empty (bounded
    /// stopping is a caller decision); this names the reason so the loop can set
    /// `state.stop_reason` without inventing a string. It never masks [`Self::should_stop`],
    /// which the coordinator checks first.
    pub fn stop_no_action(&self) -> Option<StopReason> {
        Some(StopReason::NoAdmissibleAction)
    }
}
