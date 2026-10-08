//! Adaptive refresh-delay estimation (plan Stage 5, engine-arch §14).
//!
//! Where [`crate::change::change_probability`] turns a rate into `P(changed)`, this module closes
//! the loop the other way: given how often a source *actually* changed across recent checks and a
//! prior, it estimates how long to wait before checking again. The estimate is smoothed toward the
//! prior with a pseudo-count so a source with few observations is not over-trusted, and is always
//! clamped into `[min_delay, max_delay]`.

use crate::class::SourceClass;

/// Pseudo-count weight (in "checks") given to the prior when blending. One pseudo-observation:
/// with zero real checks the estimate is exactly the prior; each real check pulls it toward the
/// observed rate.
pub const PRIOR_PSEUDO_COUNT: f64 = 1.0;

/// Running tally of change observations for one source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChangeHistory {
    /// Number of times the source was checked.
    pub checks: u32,
    /// Number of those checks that observed a change.
    pub changes: u32,
}

impl ChangeHistory {
    /// Observed change rate (changes per check) in `[0, 1]`; exactly `0.0` when no checks.
    pub fn observed_rate(&self) -> f64 {
        if self.checks == 0 {
            0.0
        } else {
            f64::from(self.changes) / f64::from(self.checks)
        }
    }
}

/// Seconds to wait before the next check, blending the observed rate with `prior_rate`.
///
/// * The blended rate is `(changes + prior·W) / (checks + W)` with `W = PRIOR_PSEUDO_COUNT`, so
///   zero checks reduce to the prior and more checks shift trust toward the observation.
/// * A higher blended rate means more frequent change, hence a shorter delay: the base delay is
///   the mean interval `1 / rate`, scaled by the class (HOT checks more often than COLD).
/// * A non-positive blended rate (no evidence of change) waits the maximum.
/// * The result is always within `[min_delay, max_delay]` (the two are ordered defensively).
pub fn next_delay_secs(
    history: &ChangeHistory,
    prior_rate: f64,
    class: SourceClass,
    min_delay: i64,
    max_delay: i64,
) -> i64 {
    let lo = min_delay.min(max_delay);
    let hi = min_delay.max(max_delay);

    let prior = if prior_rate.is_finite() && prior_rate > 0.0 {
        prior_rate
    } else {
        0.0
    };
    let checks = f64::from(history.checks);
    let changes = f64::from(history.changes);
    let blended = (changes + prior * PRIOR_PSEUDO_COUNT) / (checks + PRIOR_PSEUDO_COUNT);

    if !blended.is_finite() || blended <= 0.0 {
        // No evidence the source ever changes: wait as long as allowed.
        return hi;
    }

    let class_multiplier = match class {
        SourceClass::Hot => 1.0,
        SourceClass::Warm => 3.0,
        SourceClass::Cold => 12.0,
    };
    let delay = (1.0 / blended) * class_multiplier;

    clamp_round(delay, lo, hi)
}

/// Clamp an `f64` delay into `[lo, hi]` and round to the nearest second.
fn clamp_round(delay: f64, lo: i64, hi: i64) -> i64 {
    if !delay.is_finite() || delay >= hi as f64 {
        return hi;
    }
    if delay <= lo as f64 {
        return lo;
    }
    delay.round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observed_rate_zero_checks_is_zero_and_ratio_otherwise() {
        assert_eq!(ChangeHistory { checks: 0, changes: 0 }.observed_rate(), 0.0);
        // changes with no checks is still 0.0 (guard against divide-by-zero).
        assert_eq!(ChangeHistory { checks: 0, changes: 5 }.observed_rate(), 0.0);
        assert_eq!(ChangeHistory { checks: 10, changes: 4 }.observed_rate(), 0.4);
        assert_eq!(ChangeHistory { checks: 3, changes: 3 }.observed_rate(), 1.0);
    }

    #[test]
    fn next_delay_always_within_bounds() {
        let (min, max) = (60, 86_400);
        let cases = [
            (ChangeHistory { checks: 0, changes: 0 }, 0.0),
            (ChangeHistory { checks: 1000, changes: 1000 }, 1.0),
            (ChangeHistory { checks: 1, changes: 0 }, 0.5),
            (ChangeHistory { checks: 50, changes: 1 }, 0.01),
        ];
        for class in [SourceClass::Hot, SourceClass::Warm, SourceClass::Cold] {
            for (h, prior) in cases {
                let d = next_delay_secs(&h, prior, class, min, max);
                assert!((min..=max).contains(&d), "delay {d} out of [{min},{max}]");
            }
        }
    }

    #[test]
    fn next_delay_clamps_extremes() {
        // Very high observed rate -> base interval ~0 -> clamped to the minimum.
        let hot = ChangeHistory { checks: 1_000_000, changes: 1_000_000 };
        assert_eq!(next_delay_secs(&hot, 1.0, SourceClass::Hot, 30, 3_600), 30);
        // No evidence of change -> the maximum.
        let none = ChangeHistory { checks: 100, changes: 0 };
        assert_eq!(next_delay_secs(&none, 0.0, SourceClass::Warm, 30, 3_600), 3_600);
    }

    #[test]
    fn higher_observed_rate_shortens_delay() {
        let min = 1;
        let max = 10_000_000;
        let low = ChangeHistory { checks: 100, changes: 1 };
        let high = ChangeHistory { checks: 100, changes: 50 };
        let d_low = next_delay_secs(&low, 0.1, SourceClass::Warm, min, max);
        let d_high = next_delay_secs(&high, 0.1, SourceClass::Warm, min, max);
        assert!(
            d_high < d_low,
            "a higher change rate must shorten the delay ({d_high} !< {d_low})"
        );
    }

    #[test]
    fn hot_class_waits_no_longer_than_cold() {
        let h = ChangeHistory { checks: 10, changes: 3 };
        let min = 1;
        let max = 10_000_000;
        let hot = next_delay_secs(&h, 0.0, SourceClass::Hot, min, max);
        let warm = next_delay_secs(&h, 0.0, SourceClass::Warm, min, max);
        let cold = next_delay_secs(&h, 0.0, SourceClass::Cold, min, max);
        assert!(hot <= warm && warm <= cold, "hot={hot} warm={warm} cold={cold}");
    }

    #[test]
    fn prior_blends_in_when_no_observations() {
        // Zero checks: the estimate reduces to the prior. A non-zero prior yields a
        // finite delay strictly inside the bounds.
        let empty = ChangeHistory { checks: 0, changes: 0 };
        let with_prior = next_delay_secs(&empty, 1.0 / 3600.0, SourceClass::Hot, 1, 10_000_000);
        assert_eq!(with_prior, 3600, "prior period 1h => one hour for HOT");
        // Zero prior and zero checks -> maximum.
        assert_eq!(
            next_delay_secs(&empty, 0.0, SourceClass::Hot, 1, 10_000_000),
            10_000_000
        );
    }
}
