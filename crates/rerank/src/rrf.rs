//! Reciprocal Rank Fusion (engine-arch §10).
//!
//! `RRF(d) = sum over lanes l of 1 / (c + rank_l(d))` where `rank_l(d)` is the **0-based** rank
//! of `d` in lane `l`. The spec writes this as `1 / (c + r + 1)`, the same thing with `r`
//! 0-based. Missing documents contribute zero. `c` defaults to 60 (the classic Cormack et al.
//! constant) but is a tuning knob, not a calibrated confidence.

use std::cmp::Ordering;
use std::collections::HashMap;

/// Reciprocal Rank Fusion over lanes. `c` defaults to 60.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rrf {
    /// Smoothing constant; larger `c` flattens the influence of top ranks.
    pub c: f64,
}

impl Default for Rrf {
    fn default() -> Self {
        Self { c: 60.0 }
    }
}

impl Rrf {
    pub fn new(c: f64) -> Self {
        Self { c }
    }

    /// Fuse ranked lanes. For each lane, the document at 0-based rank `r` contributes
    /// `1.0 / (c + r + 1)`. Contributions are summed per document.
    ///
    /// **Tie rule:** equal scores are broken by *first-seen order* — the order in which the
    /// document first appears scanning lanes left-to-right and, within a lane, top-to-bottom.
    /// Within a single lane only the first occurrence of a document counts (duplicates are
    /// ignored, not double-counted).
    ///
    /// Returns `(doc_id, score)` sorted by score descending, then by first-seen order.
    pub fn fuse(&self, lanes: &[Vec<String>]) -> Vec<(String, f64)> {
        let mut scores: HashMap<&str, f64> = HashMap::new();
        let mut first_seen: Vec<String> = Vec::new();

        for lane in lanes {
            let mut counted: HashMap<&str, ()> = HashMap::new();
            for (r, doc) in lane.iter().enumerate() {
                if counted.insert(doc.as_str(), ()).is_some() {
                    continue; // duplicate within the same lane
                }
                let contrib = 1.0 / (self.c + r as f64 + 1.0);
                match scores.get_mut(doc.as_str()) {
                    Some(s) => *s += contrib,
                    None => {
                        scores.insert(doc.as_str(), contrib);
                        first_seen.push(doc.clone());
                    }
                }
            }
        }

        let order: HashMap<&str, usize> = first_seen
            .iter()
            .enumerate()
            .map(|(i, d)| (d.as_str(), i))
            .collect();

        let mut out: Vec<(String, f64)> = first_seen
            .iter()
            .map(|d| (d.clone(), scores[d.as_str()]))
            .collect();

        out.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(Ordering::Equal)
                .then_with(|| order[a.0.as_str()].cmp(&order[b.0.as_str()]))
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::Rrf;

    fn lanes(v: &[&[&str]]) -> Vec<Vec<String>> {
        v.iter()
            .map(|l| l.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    #[test]
    fn known_two_lane_example_hand_computed() {
        // c = 0 so contributions are 1/(r+1): clean hand arithmetic.
        let rrf = Rrf::new(0.0);
        let out = rrf.fuse(&lanes(&[
            &["a", "b", "c", "d"],
            &["d", "a", "e"],
        ]));
        // a: 1/1 + 1/2 = 1.5 ; d: 1/4 + 1/1 = 1.25 ; b: 1/2 = 0.5 ;
        // c: 1/3 ; e: 1/3  -> c/e tie, c seen first (lane 1 before lane 2).
        let ids: Vec<&str> = out.iter().map(|(d, _)| d.as_str()).collect();
        assert_eq!(ids, ["a", "d", "b", "c", "e"]);
        assert!((out[0].1 - 1.5).abs() < 1e-9);
        assert!((out[1].1 - 1.25).abs() < 1e-9);
        assert!((out[2].1 - 0.5).abs() < 1e-9);
        assert!((out[3].1 - 1.0 / 3.0).abs() < 1e-9);
        assert!((out[4].1 - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn default_c_is_60() {
        assert_eq!(Rrf::default().c, 60.0);
        // Single lane, first doc: 1/(60 + 0 + 1) = 1/61.
        let out = Rrf::default().fuse(&lanes(&[&["x", "y"]]));
        assert!((out[0].1 - 1.0 / 61.0).abs() < 1e-12);
        assert!((out[1].1 - 1.0 / 62.0).abs() < 1e-12);
    }

    #[test]
    fn tie_broken_by_first_seen_order() {
        // Symmetric lanes -> m and n tie; m is seen first (lane 1).
        let out = Rrf::default().fuse(&lanes(&[&["m", "n"], &["n", "m"]]));
        assert!((out[0].1 - out[1].1).abs() < 1e-12, "scores must tie");
        let ids: Vec<&str> = out.iter().map(|(d, _)| d.as_str()).collect();
        assert_eq!(ids, ["m", "n"]);
    }

    #[test]
    fn duplicate_within_lane_counted_once() {
        let out = Rrf::new(0.0).fuse(&lanes(&[&["a", "a", "b"]]));
        // a = 1/1 (rank 0 only), b = 1/3 (rank 2).
        assert!((out[0].1 - 1.0).abs() < 1e-12);
        assert!((out[1].1 - 1.0 / 3.0).abs() < 1e-12);
    }
}
