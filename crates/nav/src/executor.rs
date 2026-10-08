//! Execution boundary + the per-run context handed to executors.

use crate::budget::Budget;
use crate::error::NavError;
use crate::state::{Action, SearchAction, SearchState};

/// Everything an executor may read or update while running one request.
///
/// `budget` is the live ledger; the navigator charges it before dispatch and mirrors the
/// virtual clock (`elapsed_ms`) into `budget.used_ms` after each step. `solved` lets the
/// executor signal that the query is answered. `uncovered` lists important constraints the
/// executor knows are still unmet — escalation to a model tier requires a non-empty set (or
/// a clearly gainful admissible action).
#[derive(Debug, Clone)]
pub struct NavContext {
    pub budget: Budget,
    /// Virtual wall-clock advanced by the executor (kept hermetic; no real timers).
    pub elapsed_ms: u64,
    /// Optional client deadline. `None` disables the deadline stop condition.
    pub deadline_ms: Option<u64>,
    /// Set by the executor when the goal is reached -> `StopReason::Solved`.
    pub solved: bool,
    /// Important uncovered constraints / unresolved interpretations.
    pub uncovered: Vec<String>,
    /// Canonical keys actually dispatched, in order (observability / tests).
    pub executed: Vec<String>,
    /// Free-form notes (executor failures, plateau notices).
    pub notes: Vec<String>,
}

impl NavContext {
    pub fn new(budget: Budget) -> Self {
        Self {
            budget,
            elapsed_ms: 0,
            deadline_ms: None,
            solved: false,
            uncovered: Vec::new(),
            executed: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// Set a client deadline (virtual ms).
    pub fn with_deadline(mut self, deadline_ms: u64) -> Self {
        self.deadline_ms = Some(deadline_ms);
        self
    }

    /// Advance the virtual clock (hermetic time accounting).
    pub fn charge_ms(&mut self, ms: u64) {
        self.elapsed_ms = self.elapsed_ms.saturating_add(ms);
    }

    /// True when the deadline has been reached.
    pub fn past_deadline(&self) -> bool {
        matches!(self.deadline_ms, Some(d) if self.elapsed_ms >= d)
    }

    /// Signal the query is answered.
    pub fn mark_solved(&mut self) {
        self.solved = true;
    }
}

/// Pluggable action execution so navigation is testable without a live index.
///
/// `execute` mutates the state and context (add results, advance time, mark solved) and
/// returns an error only for genuine failures — the navigator records the failure and
/// continues instead of looping on the same action.
pub trait SearchExecutor: Send + Sync {
    fn execute(
        &mut self,
        action: &Action,
        state: &mut SearchState,
        ctx: &mut NavContext,
    ) -> Result<(), NavError>;

    /// Candidate actions for `state`, optionally biased by caller `hints`.
    fn candidates(&self, state: &SearchState, hints: &[String]) -> Vec<SearchAction>;
}
