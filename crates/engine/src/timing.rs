//! Diagnostic per-stage timing for the §6–§18 pipeline. OWNER: engine-coord agent.
//!
//! The engine's determinism rule is that the *request path* never reads a clock. This module
//! preserves that: timing is off unless the caller explicitly calls [`enable`], and every entry
//! point checks [`enabled`] first. When disabled, [`start`] returns `None`, [`record`] is a
//! no-op, and `Instant::now` is never called, so a normal request is byte-identical to before.
//!
//! The eval runner enables timing around each query and drains the accumulated stages with
//! [`take`] to report per-stage percentiles. Records are per-query and in first-seen order; a
//! stage that runs several times in one query (e.g. `retrieval` once per round, `evidence`
//! once per round) appears several times and the caller sums them per query.

use std::cell::RefCell;
use std::time::Instant;

thread_local! {
    static ENABLED: RefCell<bool> = const { RefCell::new(false) };
    static STAGES: RefCell<Vec<(&'static str, u64)>> = const { RefCell::new(Vec::new()) };
    static COUNTS: RefCell<Vec<(&'static str, u64)>> = const { RefCell::new(Vec::new()) };
}

/// Turn on per-stage timing for the current thread. Idempotent.
pub fn enable() {
    ENABLED.with(|e| *e.borrow_mut() = true);
}

/// Turn timing off and drop any accumulated records.
pub fn disable() {
    ENABLED.with(|e| *e.borrow_mut() = false);
    STAGES.with(|s| s.borrow_mut().clear());
    COUNTS.with(|s| s.borrow_mut().clear());
}

/// Whether timing is enabled on the current thread.
pub fn enabled() -> bool {
    ENABLED.with(|e| *e.borrow())
}

/// Start a stage timer, or `None` when timing is disabled (so no clock is read).
pub fn start() -> Option<Instant> {
    if enabled() {
        Some(Instant::now())
    } else {
        None
    }
}

/// Record one completed stage, in microseconds. A no-op when `start` returned `None`.
pub fn record(stage: &'static str, start: Option<Instant>) {
    if let Some(t) = start {
        let us = t.elapsed().as_micros().min(u64::MAX as u128) as u64;
        STAGES.with(|s| s.borrow_mut().push((stage, us)));
    }
}

/// Drain the accumulated `(stage, microseconds)` records for the current thread.
pub fn take() -> Vec<(&'static str, u64)> {
    STAGES.with(|s| std::mem::take(&mut *s.borrow_mut()))
}

/// Increment a named counter (e.g. how many searcher calls a query issued). No-op when
/// timing is disabled.
pub fn count(stage: &'static str) {
    if enabled() {
        COUNTS.with(|c| c.borrow_mut().push((stage, 1)));
    }
}

/// Drain the accumulated `(counter, count)` records for the current thread.
pub fn take_counts() -> Vec<(&'static str, u64)> {
    COUNTS.with(|s| std::mem::take(&mut *s.borrow_mut()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_timing_reads_no_clock_and_records_nothing() {
        disable();
        let t = start();
        assert!(t.is_none());
        record("noop", t);
        assert!(take().is_empty());
    }

    #[test]
    fn enabled_timing_records_in_order() {
        enable();
        let a = start();
        record("first", a);
        let b = start();
        record("second", b);
        let drained = take();
        assert_eq!(
            drained.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        // `take` drains.
        assert!(take().is_empty());
        disable();
    }
}
