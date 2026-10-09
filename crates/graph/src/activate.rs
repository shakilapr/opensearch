//! Query-time activation over the entity graph (plan §9.4).
//!
//! The graph built by `agent0 graph build` is a *structure*; nothing turns a query into a set of
//! activated entities until this module runs. It implements **bounded personalised PageRank**:
//! mass starts on the entities the query names, spreads across co-occurrence edges, and decays
//! with distance — so an entity that keeps company with the query's entities is reachable even
//! when the query never names it.
//!
//! Deliberate properties, each of which is a test:
//!
//! * **Bounded** — the walk is capped by `hops` and `iterations`, so query cost cannot grow with
//!   corpus size; only the visited frontier is ever touched, never the whole graph.
//! * **Deterministic** — scores are sorted with an id tie-break, so the same graph and query give
//!   the same order every time. Nothing here reads a clock.
//! * **Alias-transparent** — `AliasOf` edges are identity, not relatedness: mass must not travel
//!   along them, or a name with many aliases would manufacture authority from its own spellings.
//! * **Non-negative and normalised** — an empty or all-zero result is reported as `0.0`, never
//!   `-0.0` (a float sum over an empty iterator is `-0.0` in Rust, and it survives `>= 0.0`
//!   checks before reaching a client as `"-0.0"`).

use std::collections::{HashMap, HashSet, VecDeque};

use crate::{EdgeKind, Graph};

/// Shape of one activation run. Defaults are the values the graph build and the API use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActivationOptions {
    /// Maximum number of co-occurrence hops from a seed entity.
    pub hops: u8,
    /// PageRank damping: fraction of mass that follows edges rather than restarting at a seed.
    pub damping: f32,
    /// Power-iteration cap; the walk always stops here so cost is bounded.
    pub iterations: usize,
    /// Maximum number of activated entities returned.
    pub top_k: usize,
}

impl Default for ActivationOptions {
    fn default() -> Self {
        Self { hops: 2, damping: 0.85, iterations: 12, top_k: 25 }
    }
}

impl ActivationOptions {
    /// A run whose work is bounded by construction: at least one iteration and one hop, damping
    /// in `(0, 1)`, and a non-zero `top_k`.
    pub fn sanitised(self) -> Self {
        Self {
            hops: self.hops.clamp(1, 4),
            damping: if self.damping.is_finite() { self.damping.clamp(0.05, 0.95) } else { 0.85 },
            iterations: self.iterations.clamp(1, 64),
            top_k: self.top_k.clamp(1, 500),
        }
    }
}

/// Activate entities around `seeds` (entity ids) and return `(entity_id, score)`, best first.
///
/// Scores are non-negative and sum to about `1.0` (bounded by the damping schedule; finite
/// iterations leave a small residue). Seeds are always part of the result — a query that names an
/// entity should reach that entity even when the graph around it is thin — but no ordering
/// between a seed and its neighbours is promised: PageRank describes flow, not precedence.
pub fn activate(graph: &Graph, seeds: &[String], opts: ActivationOptions) -> Vec<(String, f32)> {
    let opts = opts.sanitised();
    if seeds.is_empty() {
        return Vec::new();
    }

    // Visit only the frontier reachable within `hops`, keeping the strongest edges.
    let (frontier, weights) = collect_frontier(graph, seeds, &opts);
    if frontier.is_empty() {
        return Vec::new();
    }

    let seed_mass = 1.0 / seeds.len() as f32;
    let mut rank: HashMap<&str, f32> = HashMap::new();
    for id in &frontier {
        rank.insert(id.as_str(), 0.0);
    }

    for _ in 0..opts.iterations {
        let mut next: HashMap<&str, f32> = HashMap::new();
        for id in &frontier {
            // Restart mass: only the seeds receive it.
            let restart = if seeds.iter().any(|s| s == id) { (1.0 - opts.damping) * seed_mass } else { 0.0 };
            next.insert(id.as_str(), restart);
        }
        for id in &frontier {
            let current = rank.get(id.as_str()).copied().unwrap_or(0.0);
            if current == 0.0 {
                continue;
            }
            let neighbours = &weights[id.as_str()];
            let total: f32 = neighbours.values().sum();
            if total <= 0.0 {
                continue;
            }
            for (other, w) in neighbours {
                let share = opts.damping * current * (w / total);
                *next.get_mut(other.as_str()).unwrap_or(&mut 0.0) += share;
            }
        }
        rank = next;
    }

    let mut out: Vec<(String, f32)> =
        frontier.iter().map(|id| (id.clone(), normalise(rank[id.as_str()]))).collect();
    // Deterministic: score descending, then id ascending.
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.0.cmp(&b.0))
    });
    out.truncate(opts.top_k);
    out
}

/// Walk the co-occurrence graph from `seeds` up to `hops`, returning the visited ids (insertion
/// order preserved for determinism) and, per visited id, its neighbours and edge weights.
fn collect_frontier(
    graph: &Graph,
    seeds: &[String],
    opts: &ActivationOptions,
) -> (Vec<String>, HashMap<String, HashMap<String, f32>>) {
    let mut visited: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut weights: HashMap<String, HashMap<String, f32>> = HashMap::new();
    let mut queue: VecDeque<(String, u8)> = VecDeque::new();

    for seed in seeds {
        if let Some(entity) = graph.get(seed) {
            let id = entity.entity_id.clone();
            if seen.insert(id.clone()) {
                visited.push(id.clone());
                queue.push_back((id, 0));
            }
        }
    }
    while let Some((id, depth)) = queue.pop_front() {
        if depth >= opts.hops {
            continue;
        }
        for (other, weight) in related_neighbours(graph, &id) {
            weights.entry(id.clone()).or_default().insert(other.clone(), weight);
            weights.entry(other.clone()).or_default().insert(id.clone(), weight);
            if seen.insert(other.clone()) {
                visited.push(other.clone());
                queue.push_back((other, depth + 1));
            }
        }
    }
    // A seed with no edges still carries its own restart mass.
    for seed in seeds {
        if seen.contains(seed) {
            weights.entry(seed.clone()).or_default();
        }
    }
    (visited, weights)
}

/// Undirected co-occurrence neighbours of `entity_id`, excluding alias links.
fn related_neighbours(graph: &Graph, entity_id: &str) -> Vec<(String, f32)> {
    let mut out = Vec::new();
    for edge in graph.edges_of(entity_id) {
        if edge.kind == EdgeKind::AliasOf {
            continue;
        }
        let other = if edge.from == entity_id {
            edge.to.clone()
        } else if edge.to == entity_id {
            edge.from.clone()
        } else {
            continue;
        };
        if !edge.weight.is_finite() || edge.weight <= 0.0 {
            continue;
        }
        out.push((other, edge.weight));
    }
    out
}

/// Collapse `-0.0` (and any non-finite value) to `0.0`.
fn normalise(value: f32) -> f32 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Edge, EdgeKind, Entity, EntityKind, Graph};

    fn node(g: &mut Graph, id: &str) {
        g.upsert_entity(Entity::new(id, id, EntityKind::Concept));
    }

    fn relate(g: &mut Graph, a: &str, b: &str, weight: f32) {
        g.add_edge(Edge::new(a, b, EdgeKind::RelatedTo, weight));
    }

    /// a — b — c, all weight 1.
    fn chain() -> Graph {
        let mut g = Graph::new();
        for id in ["a", "b", "c"] {
            node(&mut g, id);
        }
        relate(&mut g, "a", "b", 1.0);
        relate(&mut g, "b", "c", 1.0);
        g
    }

    fn ids(out: &[(String, f32)]) -> Vec<&str> {
        out.iter().map(|(id, _)| id.as_str()).collect()
    }

    #[test]
    fn mass_decays_with_distance_from_the_seed() {
        // Every reachable node is activated, and the far one carries less than the near one.
        // Note what is deliberately NOT asserted: that the seed ranks first. It need not — in
        // `a—b—c` the middle node has two neighbours and legitimately collects more mass than the
        // seed that only has one. PageRank guarantees flow, not that the seed wins.
        let g = chain();
        let out = activate(&g, &["a".to_string()], ActivationOptions::default());
        assert_eq!(ids(&out).len(), 3, "all three nodes are reachable: {out:?}");
        let score = |id: &str| out.iter().find(|(k, _)| k == id).map(|(_, v)| *v).unwrap_or(-1.0);
        assert!(score("b") > score("c"), "one hop must beat two: {out:?}");
        assert!(out.iter().all(|(_, v)| v.is_finite() && *v >= 0.0), "{out:?}");
    }

    #[test]
    fn a_seed_that_is_a_bridge_between_two_seeds_accumulates_their_mass() {
        // a and c are seeds, b sits between them: b must end up activated by both.
        let g = chain();
        let out = activate(&g, &["a".to_string(), "c".to_string()], ActivationOptions::default());
        let score = |id: &str| out.iter().find(|(k, _)| k == id).map(|(_, v)| *v).unwrap_or(-1.0);
        assert!(score("b") > 0.0, "the bridge must be activated: {out:?}");
        assert!(score("b") > score("a") || score("b") > score("c"), "{out:?}");
    }

    #[test]
    fn alias_edges_are_identity_and_carry_no_mass() {
        // A name with many aliases must not manufacture authority from its own spellings.
        let mut g = Graph::new();
        node(&mut g, "a");
        node(&mut g, "b");
        g.add_edge(Edge::new("a", "b", EdgeKind::AliasOf, 1.0));
        let out = activate(&g, &["a".to_string()], ActivationOptions::default());
        assert_eq!(ids(&out), vec!["a"], "an alias link must not activate: {out:?}");
    }

    #[test]
    fn hops_bound_the_walk() {
        let g = chain();
        let opts = ActivationOptions { hops: 1, ..ActivationOptions::default() };
        let out = activate(&g, &["a".to_string()], opts);
        assert_eq!(ids(&out), vec!["a", "b"], "hops=1 must not reach c: {out:?}");
    }

    #[test]
    fn the_walk_is_deterministic() {
        let g = chain();
        let first = activate(&g, &["a".to_string()], ActivationOptions::default());
        let second = activate(&g, &["a".to_string()], ActivationOptions::default());
        assert_eq!(first, second);
    }

    #[test]
    fn top_k_bounds_the_output() {
        let mut g = Graph::new();
        node(&mut g, "a");
        for i in 0..20 {
            let other = format!("n{i}");
            node(&mut g, &other);
            relate(&mut g, "a", &other, 1.0);
        }
        let opts = ActivationOptions { top_k: 5, ..ActivationOptions::default() };
        assert_eq!(activate(&g, &["a".to_string()], opts).len(), 5);
    }

    #[test]
    fn an_isolated_seed_still_activates_itself_with_a_positive_score() {
        let mut g = Graph::new();
        node(&mut g, "lonely");
        let out = activate(&g, &["lonely".to_string()], ActivationOptions::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "lonely");
        // A positive score, and specifically not `-0.0`: an empty float sum in Rust is `-0.0`,
        // it survives `>= 0.0`, and it reaches clients as the string "-0.0".
        assert!(out[0].1 > 0.0, "{out:?}");
        assert_ne!(out[0].1.to_bits(), (-0.0f32).to_bits());
    }

    #[test]
    fn unknown_seeds_and_no_seeds_activate_nothing() {
        let g = chain();
        assert!(activate(&g, &[], ActivationOptions::default()).is_empty());
        // A name the graph does not know cannot seed a walk.
        assert!(activate(&g, &["absent".to_string()], ActivationOptions::default()).is_empty());
    }

    #[test]
    fn at_least_the_asking_entity_is_returned_even_when_its_neighbourhood_is_empty() {
        let mut g = Graph::new();
        node(&mut g, "a");
        // An edge whose weight is not usable must not enter the walk, and must not break it.
        relate(&mut g, "a", "b", 1.0);
        g.add_edge(Edge::new("a", "b", EdgeKind::Cites, 0.0));
        let out = activate(&g, &["a".to_string()], ActivationOptions::default());
        assert!(ids(&out).contains(&"a"), "{out:?}");
        assert!(out.iter().all(|(_, score)| *score >= 0.0 && score.is_finite()), "{out:?}");
    }
}
