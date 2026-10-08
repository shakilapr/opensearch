//! MinHash signatures and LSH banding.

use std::collections::BTreeSet;

use crate::simhash::fnv1a64;

/// SplitMix64 finalizer — a fixed, deterministic bijection used to derive permutation seeds.
fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// MinHash: `k` fixed permutations over the 64-bit hash of each shingle. Two sets that agree on
/// most shingles agree on most signature components (the probability a component matches equals
/// the Jaccard similarity).
#[derive(Debug, Clone)]
pub struct MinHash {
    /// `(a, b)` affine permutations `x -> a*x + b` (wrapping). `a` is forced odd (invertible).
    perms: Vec<(u64, u64)>,
}

impl MinHash {
    /// Build a MinHash with `k` permutations. `k == 0` yields an empty signature.
    pub fn new(k: usize) -> Self {
        let mut perms = Vec::with_capacity(k);
        for i in 0..k {
            let a = splitmix64((i as u64).wrapping_mul(2).wrapping_add(1)) | 1;
            let b = splitmix64((i as u64).wrapping_mul(2).wrapping_add(2));
            perms.push((a, b));
        }
        Self { perms }
    }

    /// Number of permutations (signature length).
    pub fn len(&self) -> usize {
        self.perms.len()
    }

    /// Whether the signature is empty (`k == 0`).
    pub fn is_empty(&self) -> bool {
        self.perms.is_empty()
    }

    /// The `k`-component signature: for each permutation, the minimum permuted shingle hash.
    /// An empty shingle set yields all `u64::MAX` components (identity element of `min`).
    pub fn signature(&self, shingles: &BTreeSet<String>) -> Vec<u64> {
        // Hash each shingle once; apply every permutation to those component hashes.
        let hashes: Vec<u64> = shingles.iter().map(|s| fnv1a64(s.as_bytes())).collect();
        self.perms
            .iter()
            .map(|&(a, b)| {
                hashes
                    .iter()
                    .map(|h| h.wrapping_mul(a).wrapping_add(b))
                    .min()
                    .unwrap_or(u64::MAX)
            })
            .collect()
    }
}

impl Default for MinHash {
    fn default() -> Self {
        Self::new(64)
    }
}

/// LSH banding: split a signature into `bands` roughly equal contiguous chunks and hash each
/// chunk to a single `u64`. Two signatures sharing a band value collide in that band — the
/// basis for candidate generation. Returns exactly `bands` values (fewer only if `bands == 0`).
pub fn lsh_bands(sig: &[u64], bands: usize) -> Vec<u64> {
    if bands == 0 || sig.is_empty() {
        return Vec::new();
    }
    let band_size = sig.len().div_ceil(bands).max(1);
    sig.chunks(band_size)
        .map(|chunk| {
            let mut acc: u64 = 0xcbf2_9ce4_8422_2325;
            for v in chunk {
                acc ^= *v;
                acc = acc.wrapping_mul(0x0000_0100_0000_01b3);
            }
            acc
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shingles::shingles;

    fn sig_of(text: &str, k: usize) -> Vec<u64> {
        MinHash::new(k).signature(&shingles(text, 3))
    }

    #[test]
    fn signature_has_k_components_and_is_deterministic() {
        let mh = MinHash::new(16);
        let s = shingles("the quick brown fox jumps over the lazy dog", 2);
        let a = mh.signature(&s);
        let b = mh.signature(&s);
        assert_eq!(a.len(), 16);
        assert_eq!(a, b, "signature must be deterministic for the same input");
    }

    #[test]
    fn identical_sets_have_identical_signatures() {
        let a = sig_of("the quick brown fox jumps over the lazy dog", 24);
        let b = sig_of("the quick brown fox jumps over the lazy dog", 24);
        assert_eq!(a, b);
    }

    #[test]
    fn lsh_bands_splits_signature_into_band_hashes() {
        let sig: Vec<u64> = (0..16).collect();
        let bands = lsh_bands(&sig, 8);
        assert_eq!(bands.len(), 8, "one value per band");
        // Deterministic call.
        assert_eq!(bands, lsh_bands(&sig, 8));
        assert!(lsh_bands(&sig, 0).is_empty());
        assert!(lsh_bands(&[], 8).is_empty());
    }

    #[test]
    fn near_duplicate_signatures_share_bands() {
        let base = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu";
        let near = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda nu";
        let far = "one two three four five six seven eight nine ten eleven twelve";
        let sb = sig_of(base, 32);
        let sn = sig_of(near, 32);
        let sf = sig_of(far, 32);

        let bands_b: BTreeSet<u64> = lsh_bands(&sb, 16).into_iter().collect();
        let bands_n: BTreeSet<u64> = lsh_bands(&sn, 16).into_iter().collect();
        let bands_f: BTreeSet<u64> = lsh_bands(&sf, 16).into_iter().collect();

        assert!(
            !bands_b.is_disjoint(&bands_n),
            "near-duplicate signatures must collide in at least one band"
        );
        // Sanity: a wholly disjoint vocabulary is expected to share no band.
        assert!(bands_b.is_disjoint(&bands_f));
    }
}
