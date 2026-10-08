//! Banded SimHash index for near-duplicate candidate lookup.

use std::collections::{BTreeMap, BTreeSet};

use crate::simhash::SimHash64;

/// A deterministic `bands x rows` banding index over 64-bit SimHash fingerprints.
///
/// Each fingerprint is sliced into `bands` contiguous fields of `rows` bits (starting at the low
/// bits). Two fingerprints that agree on an entire band land in the same bucket — the classic LSH
/// candidate move. Buckets and fingerprint storage are `BTreeMap`/`BTreeSet`, so
/// [`candidates`](Self::candidates) order is stable across runs and `insert`/query results never
/// depend on hash-map iteration order.
#[derive(Debug, Clone)]
pub struct NearDupIndex {
    bands: usize,
    rows: usize,
    max_hamming: u32,
    /// One bucket map per band: band value -> set of ids sharing it.
    buckets: Vec<BTreeMap<u64, BTreeSet<String>>>,
    /// id -> fingerprint (needed to verify candidate Hamming distance exactly).
    fingerprints: BTreeMap<String, SimHash64>,
}

impl NearDupIndex {
    /// Create an index with `bands` bands of `rows` bits each. `max_hamming` is the inclusive
    /// Hamming-distance threshold for [`is_near_dup`](Self::is_near_dup).
    ///
    /// `bands` and `rows` are clamped to usable ranges; a `bands` of `0` disables candidate
    /// generation (every query is then a non-duplicate).
    pub fn new(bands: usize, rows: usize, max_hamming: u32) -> Self {
        let bands = bands.min(64);
        let rows = rows.min(64);
        Self {
            bands,
            rows,
            max_hamming,
            buckets: vec![BTreeMap::new(); bands],
            fingerprints: BTreeMap::new(),
        }
    }

    /// Number of bands.
    pub fn bands(&self) -> usize {
        self.bands
    }

    /// Bits per band.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Inclusive Hamming-distance threshold.
    pub fn max_hamming(&self) -> u32 {
        self.max_hamming
    }

    /// Number of indexed fingerprints.
    pub fn len(&self) -> usize {
        self.fingerprints.len()
    }

    /// Whether the index contains no fingerprints.
    pub fn is_empty(&self) -> bool {
        self.fingerprints.is_empty()
    }

    /// Index `id` under the buckets of every band of `fp`. Re-inserting an id overwrites its
    /// stored fingerprint; its bucket membership is idempotent.
    pub fn insert(&mut self, id: impl Into<String>, fp: SimHash64) {
        let id = id.into();
        self.fingerprints.insert(id.clone(), fp);
        for band in 0..self.bands {
            let key = self.band_key(fp, band);
            self.buckets[band].entry(key).or_default().insert(id.clone());
        }
    }

    /// Ids sharing at least one band with `fp`, in stable (sorted, de-duplicated) order. The
    /// caller may further filter by exact Hamming distance.
    pub fn candidates(&self, fp: &SimHash64) -> Vec<String> {
        let mut found: BTreeSet<String> = BTreeSet::new();
        for band in 0..self.bands {
            let key = self.band_key(*fp, band);
            if let Some(set) = self.buckets[band].get(&key) {
                found.extend(set.iter().cloned());
            }
        }
        found.into_iter().collect()
    }

    /// Whether any indexed fingerprint is within `max_hamming` of `fp`.
    pub fn is_near_dup(&self, fp: &SimHash64) -> bool {
        for band in 0..self.bands {
            let key = self.band_key(*fp, band);
            if let Some(ids) = self.buckets[band].get(&key) {
                for id in ids {
                    if let Some(other) = self.fingerprints.get(id) {
                        if fp.hamming(other) <= self.max_hamming {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    /// Extract band `band`'s bit-field from a fingerprint.
    fn band_key(&self, fp: SimHash64, band: usize) -> u64 {
        let shift = band * self.rows;
        if self.rows == 0 || shift >= 64 {
            return 0;
        }
        let mask = if self.rows >= 64 {
            u64::MAX
        } else {
            (1u64 << self.rows) - 1
        };
        (fp.0 >> shift) & mask
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_fingerprint_is_a_candidate_and_near_dup() {
        let mut idx = NearDupIndex::new(4, 16, 8);
        let fp = SimHash64(0xdead_beef_1234_5678);
        idx.insert("a", fp);
        let candidates = idx.candidates(&fp);
        assert_eq!(candidates, vec!["a".to_string()]);
        assert!(idx.is_near_dup(&fp));
    }

    #[test]
    fn candidates_are_sorted_and_deduplicated() {
        let mut idx = NearDupIndex::new(4, 16, 8);
        let fp = SimHash64(0x1111_2222_3333_4444);
        // Insert out of order; result must be sorted and each id present once.
        idx.insert("z", fp);
        idx.insert("m", fp);
        idx.insert("a", fp);
        assert_eq!(
            idx.candidates(&fp),
            vec!["a".to_string(), "m".to_string(), "z".to_string()]
        );
    }

    #[test]
    fn is_near_dup_respects_hamming_threshold() {
        let mut idx = NearDupIndex::new(4, 16, 4);
        let base = SimHash64(0);
        idx.insert("base", base);
        // One bit apart: within threshold 4.
        assert!(idx.is_near_dup(&SimHash64(0b1)));
        // Five bits apart: outside threshold 4.
        assert!(!idx.is_near_dup(&SimHash64(0b1_1111)));
    }

    #[test]
    fn empty_index_has_no_candidates_or_dups() {
        let idx = NearDupIndex::new(8, 8, 3);
        assert!(idx.is_empty());
        assert!(idx.candidates(&SimHash64(42)).is_empty());
        assert!(!idx.is_near_dup(&SimHash64(42)));
    }

    #[test]
    fn zero_bands_disables_candidate_generation() {
        let mut idx = NearDupIndex::new(0, 16, 64);
        idx.insert("a", SimHash64(7));
        assert!(idx.candidates(&SimHash64(7)).is_empty());
        assert!(!idx.is_near_dup(&SimHash64(7)));
    }
}
