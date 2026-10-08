//! HOT/WARM/COLD source classes (crawler §39).
//!
//! A source's class is a function of its *recent change rate* (changes per second, the same
//! unit the change model uses) and its *authority* (a `[0,1]` trust/quality prior). Authority
//! amplifies the effective rate: an authoritative source that demonstrably changes is treated
//! as hotter than an untrusted one changing at the same nominal rate.
//!
//! Thresholds are chosen so the class horizon lines up with the schedule horizons:
//! a source that changes at least **once per hour** is HOT, at least **once per day** is
//! WARM, otherwise COLD. This matches §39's examples (breaking news minutes => HOT;
//! documentation days => WARM; old research PDF months => COLD).

use serde::{Deserialize, Serialize};

/// Nominal rate (changes/sec) at or above which a source, with authority 0, is HOT.
pub const HOT_MIN_RATE_PER_SEC: f32 = 1.0 / 3600.0;
/// Nominal rate (changes/sec) at or above which a source, with authority 0, is WARM.
pub const WARM_MIN_RATE_PER_SEC: f32 = 1.0 / 86400.0;
/// Upper clamp applied to the `authority` argument (it is a prior in `[0,1]`).
pub const MAX_AUTHORITY: f32 = 1.0;

/// HOT/WARM/COLD source classes (crawler §39). Ordered hottest-first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SourceClass {
    Hot,
    Warm,
    Cold,
}

/// Classify a source from its recent change rate (changes per second) and authority prior.
///
/// The effective rate is `change_rate · (1 + clamp(authority, 0, 1))`, so authority can at most
/// double the heat. Boundaries are inclusive: a rate exactly at [`HOT_MIN_RATE_PER_SEC`]
/// (authority 0) is [`SourceClass::Hot`].
///
/// Non-finite or negative inputs are floored to `0.0`, which yields [`SourceClass::Cold`].
pub fn classify_source(change_rate: f32, authority: f32) -> SourceClass {
    let rate = if change_rate.is_finite() {
        change_rate.max(0.0)
    } else {
        0.0
    };
    let auth = if authority.is_finite() {
        authority.clamp(0.0, MAX_AUTHORITY)
    } else {
        0.0
    };
    let effective = rate * (1.0 + auth);

    if effective >= HOT_MIN_RATE_PER_SEC {
        SourceClass::Hot
    } else if effective >= WARM_MIN_RATE_PER_SEC {
        SourceClass::Warm
    } else {
        SourceClass::Cold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_source_boundaries_at_thresholds() {
        // Exactly at the HOT boundary (authority 0) is Hot.
        assert_eq!(classify_source(HOT_MIN_RATE_PER_SEC, 0.0), SourceClass::Hot);
        // Just below the HOT boundary drops to Warm.
        assert_eq!(
            classify_source(HOT_MIN_RATE_PER_SEC * 0.999, 0.0),
            SourceClass::Warm
        );
        // Exactly at the WARM boundary is Warm.
        assert_eq!(
            classify_source(WARM_MIN_RATE_PER_SEC, 0.0),
            SourceClass::Warm
        );
        // Just below the WARM boundary is Cold.
        assert_eq!(
            classify_source(WARM_MIN_RATE_PER_SEC * 0.5, 0.0),
            SourceClass::Cold
        );
        // Monotone non-decreasing in rate at fixed authority (note: `SourceClass` derives Ord as
        // hottest-first, so compare an explicit heat rank).
        fn heat(c: SourceClass) -> u8 {
            match c {
                SourceClass::Hot => 2,
                SourceClass::Warm => 1,
                SourceClass::Cold => 0,
            }
        }
        let r = [0.0, WARM_MIN_RATE_PER_SEC, HOT_MIN_RATE_PER_SEC, 1.0];
        for w in r.windows(2) {
            assert!(heat(classify_source(w[0], 0.5)) <= heat(classify_source(w[1], 0.5)));
        }
    }

    #[test]
    fn classify_source_authority_boosts_rate() {
        // A rate half of HOT with full authority doubles to the HOT boundary.
        assert_eq!(
            classify_source(HOT_MIN_RATE_PER_SEC * 0.5, 1.0),
            SourceClass::Hot
        );
        // The same rate with zero authority is only Warm.
        assert_eq!(
            classify_source(HOT_MIN_RATE_PER_SEC * 0.5, 0.0),
            SourceClass::Warm
        );
    }

    #[test]
    fn classify_source_zero_and_hostile_inputs_are_cold() {
        assert_eq!(classify_source(0.0, 1.0), SourceClass::Cold);
        assert_eq!(classify_source(-1.0, 1.0), SourceClass::Cold);
        assert_eq!(classify_source(f32::NAN, 1.0), SourceClass::Cold);
        // Authority is clamped, so an absurd authority cannot lift a zero rate.
        assert_eq!(classify_source(0.0, 1e9), SourceClass::Cold);
    }
}
