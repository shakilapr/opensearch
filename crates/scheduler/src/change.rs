//! Learned change model (crawler §39, engine-arch §14).
//!
//! `P(changed since last fetch) = 1 - exp(-rate · age)`, where `rate` is changes per second
//! and `age` is seconds since the last successful verification.

/// Probability that a source changed since it was last verified.
///
/// * `rate` — changes per second (non-negative; non-positive yields `0.0`).
/// * `age_secs` — seconds since the last verified fetch (non-negative; non-positive yields `0.0`).
///
/// The result is monotone non-decreasing in both arguments and constrained to `[0, 1)`; it is
/// exactly `0.0` at age `0` and is capped just below `1.0` so the strict upper bound always
/// holds even when `rate · age` overflows the exponential. Non-finite inputs yield `0.0`.
pub fn change_probability(rate: f64, age_secs: f64) -> f64 {
    if !rate.is_finite() || !age_secs.is_finite() || rate <= 0.0 || age_secs <= 0.0 {
        return 0.0;
    }
    let x = rate * age_secs;
    let p = 1.0 - (-x).exp();
    if p >= 1.0 {
        // exp(-x) underflowed to 0; stay strictly inside the open upper bound.
        1.0 - f64::EPSILON
    } else {
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn change_probability_zero_at_age_zero() {
        assert_eq!(change_probability(1.0, 0.0), 0.0);
        assert_eq!(change_probability(1.0, -5.0), 0.0);
        assert_eq!(change_probability(1.0, f64::NAN), 0.0);
        // Zero or negative rate never changes.
        assert_eq!(change_probability(0.0, 10_000.0), 0.0);
        assert_eq!(change_probability(-1.0, 10_000.0), 0.0);
    }

    #[test]
    fn change_probability_monotonic_in_age() {
        let rate = 1.0 / 3600.0;
        let mut prev = -1.0;
        for age in [0.0, 1.0, 60.0, 3600.0, 86_400.0, 1_000_000.0] {
            let p = change_probability(rate, age);
            assert!(p >= prev, "prob decreased with age: {p} < {prev}");
            prev = p;
        }
        // A full mean interval gives 1 - 1/e.
        let p = change_probability(1.0, 1.0);
        assert!((p - (1.0 - 1.0 / std::f64::consts::E)).abs() < 1e-12);
    }

    #[test]
    fn change_probability_monotonic_in_rate() {
        let age = 3600.0;
        let mut prev = -1.0;
        for rate in [0.0, 1e-6, 1e-4, 1e-2, 1e6] {
            let p = change_probability(rate, age);
            assert!(p >= prev, "prob decreased with rate: {p} < {prev}");
            prev = p;
        }
    }

    #[test]
    fn change_probability_bounded_unit_interval() {
        for rate in [1e-9, 1.0 / 3600.0, 1.0, 1e9] {
            for age in [0.0, 1.0, 86_400.0, 1e12] {
                let p = change_probability(rate, age);
                assert!((0.0..1.0).contains(&p), "out of [0,1): {p}");
            }
        }
    }
}
