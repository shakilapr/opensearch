//! 64-bit SimHash fingerprint.

/// Deterministic FNV-1a 64-bit hash over `bytes`.
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// A 64-bit SimHash fingerprint. Order-independent over tokens; similar token multisets yield
/// fingerprints that differ in few bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SimHash64(pub u64);

impl SimHash64 {
    /// Build a fingerprint from a token stream. Each token contributes `+1` to every bit its
    /// hash sets and `-1` to every bit it clears; a bit is set in the result iff its column sum
    /// is strictly positive (ties resolve to `0`). Empty input yields `SimHash64(0)`.
    pub fn of_tokens<'a>(tokens: impl IntoIterator<Item = &'a str>) -> Self {
        let mut columns = [0i64; 64];
        for token in tokens {
            let h = fnv1a64(token.as_bytes());
            for (bit, col) in columns.iter_mut().enumerate() {
                if (h >> bit) & 1 == 1 {
                    *col += 1;
                } else {
                    *col -= 1;
                }
            }
        }
        let mut out = 0u64;
        for (bit, col) in columns.iter().enumerate() {
            if *col > 0 {
                out |= 1u64 << bit;
            }
        }
        SimHash64(out)
    }

    /// Hamming distance between two fingerprints (`0..=64`).
    pub fn hamming(&self, other: &SimHash64) -> u32 {
        (self.0 ^ other.0).count_ones()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hamming_identity_and_symmetry() {
        let a = SimHash64::of_tokens(["alpha", "beta", "gamma"]);
        let b = SimHash64::of_tokens(["beta", "gamma", "delta"]);
        assert_eq!(a.hamming(&a), 0);
        assert_eq!(a.hamming(&b), b.hamming(&a));
        assert!(a.hamming(&b) > 0);
        assert!(a.hamming(&b) <= 64);
    }

    #[test]
    fn of_tokens_is_order_independent_and_deterministic() {
        let a = SimHash64::of_tokens(["one", "two", "three", "four"]);
        let b = SimHash64::of_tokens(["four", "three", "two", "one"]);
        let c = SimHash64::of_tokens(["one", "two", "three", "four"]);
        assert_eq!(a, b, "SimHash is a bag-of-tokens fingerprint");
        assert_eq!(a, c, "same input yields the same fingerprint");
    }

    #[test]
    fn similar_multisets_are_closer_than_disjoint_ones() {
        let base: Vec<&str> = "the quick brown fox jumps over the lazy dog today".split(' ').collect();
        let near: Vec<&str> = "the quick brown fox jumps over the lazy cat today".split(' ').collect();
        let far: Vec<&str> = "quantum entanglement lattice gauge theory renormalization".split(' ').collect();
        let f = SimHash64::of_tokens(base.iter().copied());
        let n = SimHash64::of_tokens(near.iter().copied());
        let x = SimHash64::of_tokens(far.iter().copied());
        assert!(
            f.hamming(&n) < f.hamming(&x),
            "a one-word edit must be closer than an unrelated vocabulary"
        );
    }

    #[test]
    fn empty_tokens_fingerprint_is_zero() {
        assert_eq!(SimHash64::of_tokens(std::iter::empty::<&str>()), SimHash64(0));
    }
}
