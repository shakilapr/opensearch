//! §16 Greedy maximal-marginal-relevance diversification. OWNER: engine-rank agent.
//!
//! Frozen contract: `docs/ENGINE_SPEC.md` §16 and `engine/engine-architecture.md` §16.

use std::collections::BTreeSet;

/// A ranked candidate plus the features diversification needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Scored {
    pub doc_id: String,
    pub relevance: f32,
    /// Gain from covering query units no selected document covers yet.
    pub uncovered_gain: f32,
    /// Story/duplicate cluster (e.g. syndication group); `None` = unknown.
    pub cluster: Option<String>,
    pub domain: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiversityConfig {
    pub redundancy_weight: f32,
    pub uncovered_weight: f32,
    /// At most this many results from one domain (unless the query is site-scoped §16).
    pub max_per_domain: usize,
}

impl Default for DiversityConfig {
    fn default() -> Self {
        Self { redundancy_weight: 1.0, uncovered_weight: 0.5, max_per_domain: 3 }
    }
}

/// Greedy MMR (§16):
/// `utility(d | selected) = relevance(d) + uncovered_weight*uncovered_gain(d) - redundancy`.
///
/// Redundancy is charged for a shared story cluster (`cluster`, `Some`) and for a domain
/// already at `max_per_domain`. Domain diversity is a **soft** objective — it is a penalty,
/// never a hard filter (§16: "Domain diversity alone does not establish independent
/// reporting") — so the result set is never starved: if every remaining candidate is from one
/// domain that has reached the cap, the next-best is still selected, and the function returns
/// `min(k, available)`.
///
/// Determinism: ties in utility are broken by `doc_id` ascending. A `doc_id` already selected
/// is never selected again. The greedy loop runs at most `k` times.
pub fn diversify(candidates: Vec<Scored>, k: usize, cfg: &DiversityConfig) -> Vec<Scored> {
    let mut pool = candidates;
    let mut selected: Vec<Scored> = Vec::new();
    let mut selected_ids: BTreeSet<String> = BTreeSet::new();

    while selected.len() < k && !pool.is_empty() {
        let mut best: Option<(usize, f32, String)> = None;
        for (i, c) in pool.iter().enumerate() {
            // Never select the same document twice.
            if selected_ids.contains(&c.doc_id) {
                continue;
            }
            let mut redundancy = 0.0f32;
            if let Some(cl) = &c.cluster {
                if selected.iter().any(|s| s.cluster.as_deref() == Some(cl.as_str())) {
                    redundancy += cfg.redundancy_weight;
                }
            }
            let domain_count = selected.iter().filter(|s| s.domain == c.domain).count();
            if domain_count >= cfg.max_per_domain {
                redundancy += cfg.redundancy_weight;
            }
            let utility =
                c.relevance + cfg.uncovered_weight * c.uncovered_gain - redundancy;
            let better = match &best {
                None => true,
                Some((_, best_util, best_id)) => {
                    utility > *best_util || (utility == *best_util && c.doc_id < *best_id)
                }
            };
            if better {
                best = Some((i, utility, c.doc_id.clone()));
            }
        }
        match best {
            Some((i, _, _)) => {
                let picked = pool.remove(i);
                selected_ids.insert(picked.doc_id.clone());
                selected.push(picked);
            }
            // Every remaining candidate duplicates an already-selected doc_id.
            None => break,
        }
    }
    selected
}
