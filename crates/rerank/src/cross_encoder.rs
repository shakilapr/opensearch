//! Cross-encoder interface + a deterministic lexical default (engine §21).
//!
//! A real CPU cross-encoder (BGE-style, ~67 MB) would live behind a cargo feature; the default
//! here is a pure lexical-overlap scorer that downloads nothing and is byte-for-byte
//! reproducible. It is honest about being a lexical proxy, not a learned model.

use std::collections::HashSet;

use async_trait::async_trait;

use crate::tokenize::tokenize;

/// Score a (query, doc) pair. Implementations must be `Send + Sync` and cheap to share.
#[async_trait]
pub trait CrossEncoder: Send + Sync {
    async fn score(&self, query: &str, doc: &str) -> f32;
}

/// Deterministic lexical-overlap cross-encoder: no model download, same input -> same score.
pub struct LexicalCrossEncoder;

/// The core (sync, pure) serving function behind [`LexicalCrossEncoder`], exposed for testing
/// and reuse.
///
/// Score = normalized query-token coverage + a small adjacency bonus, clamped to `[0, 1]`:
/// - coverage = |unique query tokens present in doc| / |unique query tokens|  (0..1)
/// - bonus    = 0.15 * (fraction of query bigrams that appear as adjacent doc bigrams)
///
/// A non-empty query with no matching tokens scores `0.0`; an exact token match scores `1.0`
/// (the bonus is capped away). An empty query scores `0.0`.
pub fn lexical_overlap_score(query: &str, doc: &str) -> f32 {
    let q = tokenize(query);
    if q.is_empty() {
        return 0.0;
    }
    let d = tokenize(doc);

    let q_set: HashSet<&String> = q.iter().collect();
    let d_set: HashSet<&String> = d.iter().collect();
    let matched = q_set.iter().filter(|t| d_set.contains(*t)).count();
    let coverage = matched as f32 / q_set.len() as f32;

    // Adjacency (proximity) bonus: how many query bigrams survive as doc bigrams.
    let mut pair_hits = 0usize;
    let mut pairs = 0usize;
    if q.len() >= 2 {
        let d_pairs: HashSet<(&str, &str)> = d
            .windows(2)
            .map(|w| (w[0].as_str(), w[1].as_str()))
            .collect();
        for w in q.windows(2) {
            pairs += 1;
            if d_pairs.contains(&(w[0].as_str(), w[1].as_str())) {
                pair_hits += 1;
            }
        }
    }
    let proximity = if pairs > 0 {
        pair_hits as f32 / pairs as f32
    } else {
        0.0
    };

    let score = (coverage + 0.15 * proximity).clamp(0.0, 1.0);
    // Defensive: `clamp` returns a negative zero unchanged (IEEE treats `-0.0 == 0.0`, so it is
    // already in range). No input reaches this today — coverage and proximity are both
    // non-negative here — but a score that serialises as `-0.0` must not be possible.
    if score > 0.0 {
        score
    } else {
        0.0
    }
}

#[async_trait]
impl CrossEncoder for LexicalCrossEncoder {
    async fn score(&self, query: &str, doc: &str) -> f32 {
        lexical_overlap_score(query, doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal single-poll executor: every future here is immediately ready, so one poll needs
    /// no real runtime dependency. Uses the std no-op waker (no `unsafe`).
    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        use std::pin::pin;
        use std::task::{Context, Poll};
        let mut cx = Context::from_waker(std::task::Waker::noop());
        let mut fut = pin!(fut);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("future unexpectedly pending"),
        }
    }

    #[test]
    fn identical_scores_higher_than_unrelated() {
        let ce = LexicalCrossEncoder;
        let identical = block_on(ce.score("rust async runtime", "rust async runtime"));
        let unrelated = block_on(ce.score("rust async runtime", "banana bread recipe"));
        assert!(identical > unrelated, "{identical} !> {unrelated}");
        assert!((identical - 1.0).abs() < 1e-6);
        assert!(unrelated.abs() < 1e-6);
        assert!((0.0..=1.0).contains(&identical) && (0.0..=1.0).contains(&unrelated));
    }

    #[test]
    fn partial_overlap_is_partial_and_reproducible() {
        let ce = LexicalCrossEncoder;
        let a = block_on(ce.score("rust async runtime", "rust async internals"));
        let b = block_on(ce.score("rust async runtime", "rust async internals"));
        assert_eq!(a, b, "must be deterministic");
        assert!(a > 0.0 && a < 1.0);
        // coverage 2/3 (rust, async present; runtime absent) + 1 of 2 query bigrams survives.
        assert!((a - (2.0 / 3.0 + 0.15 * 0.5)).abs() < 1e-6);
    }

    #[test]
    fn empty_query_scores_zero() {
        let ce = LexicalCrossEncoder;
        assert_eq!(block_on(ce.score("   ", "anything at all")), 0.0);
    }
}
