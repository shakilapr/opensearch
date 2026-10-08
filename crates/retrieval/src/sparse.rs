//! Sparse query expansion (Stage 9 addition to `search-retrieval`).
//!
//! [`SparseExpander`] is a small, deterministic co-occurrence expander built from a committed
//! term-pair fixture. It is **not** a learned SPLADE / BGE sparse encoder: no model weights are
//! loaded and no neural inference happens here. It generalises a query by appending terms that
//! co-occur with the query terms in the fixture, with weights derived from pair counts.
//!
//! Determinism: pair counts live in `BTreeMap`s, so iteration (and therefore the expansion
//! order and tie-breaks) is stable across runs. `expand` never iterates a `HashMap`.

use std::collections::BTreeMap;

/// A deterministic term-co-occurrence expander.
///
/// Construct from `(a, b)` pairs observed in the fixture: each pair increments the
/// `a -> b` and `b -> a` co-occurrence counts. [`SparseExpander::expand`] scores every
/// neighbour of the input terms by count, normalised per input term, and returns the top
/// `max_terms` neighbours (excluding the input terms themselves).
#[derive(Debug, Clone, Default)]
pub struct SparseExpander {
    /// term -> (neighbour term -> count). Sorted maps give stable iteration.
    adjacency: BTreeMap<String, BTreeMap<String, u32>>,
    max_terms: usize,
}

impl SparseExpander {
    /// Build from observed co-occurring pairs. `max_terms` caps how many expanded terms
    /// [`SparseExpander::expand`] returns.
    pub fn from_pairs(pairs: &[(String, String)], max_terms: usize) -> Self {
        let mut adjacency: BTreeMap<String, BTreeMap<String, u32>> = BTreeMap::new();
        for (a, b) in pairs {
            if a == b {
                continue;
            }
            *adjacency
                .entry(a.clone())
                .or_default()
                .entry(b.clone())
                .or_insert(0) += 1;
            *adjacency
                .entry(b.clone())
                .or_default()
                .entry(a.clone())
                .or_insert(0) += 1;
        }
        Self {
            adjacency,
            max_terms,
        }
    }

    /// The number of distinct terms known to the expander.
    pub fn vocabulary_size(&self) -> usize {
        self.adjacency.len()
    }

    /// Expand `terms` into weighted co-occurring terms.
    ///
    /// Each input term distributes a total weight of 1.0 across its neighbours, proportional to
    /// the co-occurrence count. Contributions from multiple input terms accumulate. Input terms
    /// themselves are never returned. Output is sorted by weight descending, then term ascending,
    /// and truncated to `max_terms`. Empty input (or unknown terms) yields an empty vector.
    pub fn expand(&self, terms: &[String]) -> Vec<(String, f32)> {
        if terms.is_empty() || self.max_terms == 0 {
            return Vec::new();
        }
        let inputs: std::collections::BTreeSet<&str> =
            terms.iter().map(|s| s.as_str()).collect();

        let mut acc: BTreeMap<String, f32> = BTreeMap::new();
        for term in terms {
            let neighbours = match self.adjacency.get(term) {
                Some(n) => n,
                None => continue,
            };
            let total: u32 = neighbours.values().sum();
            if total == 0 {
                continue;
            }
            for (other, count) in neighbours {
                if inputs.contains(other.as_str()) {
                    continue;
                }
                *acc.entry(other.clone()).or_insert(0.0) += *count as f32 / total as f32;
            }
        }

        let mut out: Vec<(String, f32)> = acc.into_iter().collect();
        // Weight desc; ties by term asc (BTreeMap already sorted, but be explicit).
        out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out.truncate(self.max_terms);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs() -> Vec<(String, String)> {
        [
            ("llm", "transformer"),
            ("llm", "inference"),
            ("transformer", "attention"),
            ("rust", "async"),
            ("rust", "cargo"),
            ("async", "tokio"),
            ("search", "retrieval"),
        ]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
    }

    #[test]
    fn expansion_is_deterministic_and_sorted() {
        let e = SparseExpander::from_pairs(&pairs(), 8);
        let q = vec!["llm".to_string()];
        let a = e.expand(&q);
        let b = e.expand(&q);
        assert_eq!(a, b, "same input must give same expansion");
        // llm co-occurs once with transformer and inference -> weight 0.5 each; "inference" < "transformer".
        assert_eq!(
            a,
            vec![
                ("inference".to_string(), 0.5),
                ("transformer".to_string(), 0.5),
            ]
        );
    }

    #[test]
    fn max_terms_is_respected_and_inputs_excluded() {
        let e = SparseExpander::from_pairs(&pairs(), 1);
        let q = vec!["llm".to_string(), "rust".to_string()];
        let out = e.expand(&q);
        assert_eq!(out.len(), 1);
        // No returned term is an input term.
        for (t, _) in &out {
            assert_ne!(t, "llm");
            assert_ne!(t, "rust");
        }
    }

    #[test]
    fn unknown_terms_expand_to_nothing() {
        let e = SparseExpander::from_pairs(&pairs(), 8);
        assert!(e.expand(&["aardvark".to_string()]).is_empty());
        assert!(e.expand(&[]).is_empty());
        assert_eq!(e.vocabulary_size(), 10);
    }
}
