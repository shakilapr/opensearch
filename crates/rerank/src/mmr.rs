//! Greedy Maximal Marginal Relevance (engine-arch §16 "diversity").
//!
//! Classic MMR: starting from an empty selection, repeatedly add the document maximizing
//!
//! ```text
//! mmr(d) = lambda * relevance(d) - (1 - lambda) * max_{s in selected} sim(d, s)
//! ```
//!
//! - `lambda = 1.0` -> pure relevance order (diversification disabled).
//! - `lambda = 0.0` -> pick the least-similar document each step (relevance only breaks the
//!   first tie).
//!
//! Fully deterministic: ties in the MMR score are broken by the document's position in the
//! input `ranked` slice (earlier wins). `sim` is expected to return a similarity in roughly
//! `[0, 1]` (e.g. cosine); it is treated as `0.0` against an empty selection.

/// Greedy MMR over a pre-scored candidate list.
///
/// Returns `(doc_id, relevance)` in selection order — the relevance score is the one passed in,
/// so callers keep the original signal rather than the internal MMR blend. `lambda` is clamped
/// to `[0, 1]`. Output length is `min(limit, ranked.len())`.
pub fn mmr(
    ranked: &[(String, f32)],
    sim: &dyn Fn(&str, &str) -> f32,
    lambda: f32,
    limit: usize,
) -> Vec<(String, f32)> {
    let lambda = lambda.clamp(0.0, 1.0);
    let n = ranked.len();
    let take = limit.min(n);

    let mut selected: Vec<usize> = Vec::with_capacity(take);
    let mut chosen = vec![false; n];

    while selected.len() < take {
        let mut best: Option<usize> = None;
        let mut best_score = f32::NEG_INFINITY;

        for i in 0..n {
            if chosen[i] {
                continue;
            }
            let mut max_sim = 0.0f32;
            for &j in &selected {
                let s = sim(&ranked[i].0, &ranked[j].0);
                if s > max_sim {
                    max_sim = s;
                }
            }
            let score = lambda * ranked[i].1 - (1.0 - lambda) * max_sim;
            if score > best_score {
                best_score = score;
                best = Some(i);
            }
        }

        match best {
            Some(i) => {
                chosen[i] = true;
                selected.push(i);
            }
            None => break,
        }
    }

    selected
        .into_iter()
        .map(|i| (ranked[i].0.clone(), ranked[i].1))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::mmr;

    fn ranked() -> Vec<(String, f32)> {
        vec![
            ("a".to_string(), 0.9),
            ("b".to_string(), 0.85),
            ("c".to_string(), 0.10),
        ]
    }

    #[test]
    fn lambda_one_is_pure_relevance_order() {
        // sim would normally pull b down after a, but lambda=1 ignores it entirely.
        let sim = |x: &str, y: &str| if (x, y) == ("a", "b") || (x, y) == ("b", "a") { 0.9 } else { 0.0 };
        let out = mmr(&ranked(), &sim, 1.0, 3);
        let ids: Vec<&str> = out.iter().map(|(d, _)| d.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        // relevance is passed through unchanged
        assert_eq!(out[0].1, 0.9);
    }

    #[test]
    fn lambda_zero_diversifies_and_reorders() {
        // a and b are near-duplicates; c is dissimilar but low relevance.
        let sim = |x: &str, y: &str| {
            if (x, y) == ("a", "b") || (x, y) == ("b", "a") {
                0.9
            } else {
                0.0
            }
        };
        let out = mmr(&ranked(), &sim, 0.0, 2);
        let ids: Vec<&str> = out.iter().map(|(d, _)| d.as_str()).collect();
        // first pick: all max_sim=0 -> a (highest relevance, earliest index).
        // second pick: b scores -0.9, c scores 0.0 -> c wins, reordering vs relevance.
        assert_eq!(ids, ["a", "c"]);
    }

    #[test]
    fn deterministic_tie_break_and_limit() {
        let items = vec![
            ("x".to_string(), 0.5),
            ("y".to_string(), 0.5),
            ("z".to_string(), 0.5),
        ];
        let sim = |_: &str, _: &str| 0.0;
        let out = mmr(&items, &sim, 1.0, 2);
        let ids: Vec<&str> = out.iter().map(|(d, _)| d.as_str()).collect();
        assert_eq!(ids, ["x", "y"]); // equal scores -> input order, limited to 2
    }
}
