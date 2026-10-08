//! Atomic budget ledger: charged BEFORE dispatch, never after (engine-arch §13).
//!
//! The ledger holds a fixed `capacity` and a monotonically increasing `spent` counter. Charges
//! are attempted atomically with a compare-and-swap loop, so a charge that would push the total
//! past capacity is refused outright — the caller must not dispatch. This is the mechanism that
//! prevents a refresh storm: no activation is produced unless its cost was first accounted for.

use std::sync::atomic::{AtomicU64, Ordering};

/// A fixed-capacity atomic budget ledger (units are caller-defined, e.g. live fetches).
#[derive(Debug)]
pub struct BudgetLedger {
    capacity: u64,
    spent: AtomicU64,
}

impl BudgetLedger {
    /// Create a ledger with `capacity` units of budget.
    pub fn new(capacity: u64) -> Self {
        Self {
            capacity,
            spent: AtomicU64::new(0),
        }
    }

    /// Total capacity.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Attempt to charge `cost` units.
    ///
    /// Returns `true` and records the charge if `spent + cost <= capacity`; returns `false` and
    /// changes nothing when the charge would exceed capacity. A `cost` that would overflow `u64`
    /// is treated as exceeding capacity. Charging `0` always succeeds.
    pub fn try_charge(&self, cost: u64) -> bool {
        let mut cur = self.spent.load(Ordering::SeqCst);
        loop {
            let Some(next) = cur.checked_add(cost) else {
                return false;
            };
            if next > self.capacity {
                return false;
            }
            match self.spent.compare_exchange_weak(
                cur,
                next,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(observed) => cur = observed,
            }
        }
    }

    /// Return `cost` units to the ledger (e.g. on early abort). Never underflows: refunding more
    /// than was spent clamps `spent` to `0`.
    pub fn refund(&self, cost: u64) {
        let mut cur = self.spent.load(Ordering::SeqCst);
        loop {
            let next = cur.saturating_sub(cost);
            match self.spent.compare_exchange_weak(
                cur,
                next,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return,
                Err(observed) => cur = observed,
            }
        }
    }

    /// Units charged so far.
    pub fn spent(&self) -> u64 {
        self.spent.load(Ordering::SeqCst)
    }

    /// Units still available (`capacity - spent`, saturating).
    pub fn remaining(&self) -> u64 {
        self.capacity.saturating_sub(self.spent())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn ledger_charge_and_refund() {
        let l = BudgetLedger::new(100);
        assert_eq!(l.spent(), 0);
        assert_eq!(l.remaining(), 100);
        assert!(l.try_charge(60));
        assert_eq!(l.spent(), 60);
        assert_eq!(l.remaining(), 40);
        assert!(l.try_charge(40));
        assert_eq!(l.spent(), 100);
        assert_eq!(l.remaining(), 0);

        // Refund returns budget.
        l.refund(30);
        assert_eq!(l.spent(), 70);
        assert_eq!(l.remaining(), 30);
        assert!(l.try_charge(30));
        assert_eq!(l.remaining(), 0);

        // A zero-cost charge is always admissible.
        assert!(l.try_charge(0));
        assert_eq!(l.spent(), 100);
    }

    #[test]
    fn ledger_rejects_overflow() {
        let l = BudgetLedger::new(100);
        assert!(l.try_charge(99));
        // 99 + 2 would exceed capacity: refused, nothing recorded.
        assert!(!l.try_charge(2));
        assert_eq!(l.spent(), 99);
        assert_eq!(l.remaining(), 1);
        // Exactly filling to capacity is allowed.
        assert!(l.try_charge(1));
        assert!(!l.try_charge(1));

        // A `u64::MAX` cost can never fit and must not panic.
        let l2 = BudgetLedger::new(u64::MAX);
        assert!(l2.try_charge(u64::MAX - 1));
        assert!(!l2.try_charge(2));
        assert!(!l2.try_charge(u64::MAX));
        assert_eq!(l2.spent(), u64::MAX - 1);
    }

    #[test]
    fn ledger_refund_never_underflows() {
        let l = BudgetLedger::new(50);
        assert!(l.try_charge(20));
        // Refunding more than spent clamps to zero rather than wrapping.
        l.refund(u64::MAX);
        assert_eq!(l.spent(), 0);
        assert_eq!(l.remaining(), 50);
        // Refunding an empty ledger stays at zero.
        l.refund(10);
        assert_eq!(l.spent(), 0);
    }

    #[test]
    fn ledger_concurrent_charges_respect_capacity() {
        let l = Arc::new(BudgetLedger::new(1_000));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let l = Arc::clone(&l);
            handles.push(std::thread::spawn(move || {
                let mut ok = 0u64;
                for _ in 0..1_000 {
                    if l.try_charge(1) {
                        ok += 1;
                    }
                }
                ok
            }));
        }
        let total: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
        // Exactly capacity charges may succeed across all threads.
        assert_eq!(total, 1_000);
        assert_eq!(l.spent(), 1_000);
        assert!(l.spent() <= l.capacity());
    }
}
