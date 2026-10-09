//! §16 Greedy maximal-marginal-relevance diversification. OWNER: engine-rank agent.

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

/// Greedy MMR (§16): `utility(d | selected) = relevance + uncovered_gain - redundancy`.
/// Redundancy is charged for a shared cluster or a domain already at its cap.
pub fn diversify(candidates: Vec<Scored>, k: usize, cfg: &DiversityConfig) -> Vec<Scored> {
    let mut pool = candidates;
    let mut selected: Vec<Scored> = Vec::new();

    while selected.len() < k && !pool.is_empty() {
        let mut best: Option<(usize, f32)> = None;
        for (i, c) in pool.iter().enumerate() {
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
            if best.map_or(true, |(_, b)| utility > b) {
                best = Some((i, utility));
            }
        }
        match best {
            Some((i, _)) => selected.push(pool.remove(i)),
            None => break,
        }
    }
    selected
}
