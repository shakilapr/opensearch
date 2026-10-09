//! Entity / concept / source graph + scholarly WorkId resolution (plan Stage 1, crawler §24,
//! engine §13).
//!
//! In-memory graph of entities, aliases, and typed edges, plus DOI / arXiv / PMID
//! normalization for WorkId resolution. Interfaces frozen in `docs/STAGE1_SPEC.md`.
//!
//! ## Traversal semantics
//!
//! `expand` walks edges **undirected**: an edge participates in expansion from either
//! endpoint. This suits the AliasOf / RelatedTo relationships that drive query expansion and
//! keeps one code path; directional edge kinds (Cites / AuthoredBy / PublishedBy) are still
//! distinguishable by [`Edge::kind`] for callers that care.
//!
//! Interfaces are frozen in `docs/STAGE1_SPEC.md`. Module layout is up to the owner.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

pub mod activate;
pub mod extract;
pub mod sidecar;
pub mod work;

pub use extract::{
    canonical_key, cooccurrence_pairs, entity_id, extract_mentions, kind_for, Mention,
    DEFAULT_COOC_WINDOW, MAX_PHRASE_WORDS,
};
pub use work::{detect_identifier, work_id_from_identifier, WorkIdKind};

/// Maximum number of entities `expand` will return regardless of hops (cycle/blow-up guard).
const MAX_EXPAND_RESULTS: usize = 10_000;

/// What kind of thing an entity is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntityKind {
    Person,
    Organization,
    Product,
    Concept,
    Place,
    Work,
    Other(String),
}

impl EntityKind {
    /// Stable lowercase tag matching the `entity.kind` column in `0003_stage1.sql`.
    pub fn as_tag(&self) -> &str {
        match self {
            EntityKind::Person => "person",
            EntityKind::Organization => "organization",
            EntityKind::Product => "product",
            EntityKind::Concept => "concept",
            EntityKind::Place => "place",
            EntityKind::Work => "work",
            EntityKind::Other(s) => s.as_str(),
        }
    }
}

/// A canonical node: one id, one canonical name, alternate surface forms in `aliases`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entity {
    pub entity_id: String,
    pub name: String,
    pub kind: EntityKind,
    pub aliases: Vec<String>,
}

impl Entity {
    /// Convenience constructor with no aliases.
    pub fn new(
        entity_id: impl Into<String>,
        name: impl Into<String>,
        kind: EntityKind,
    ) -> Self {
        Self {
            entity_id: entity_id.into(),
            name: name.into(),
            kind,
            aliases: Vec::new(),
        }
    }
}

/// A typed, weighted relationship between two entities (by id).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
    pub weight: f32,
}

/// Edge relationship type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeKind {
    AliasOf,
    RelatedTo,
    Cites,
    AuthoredBy,
    PublishedBy,
}

impl Edge {
    /// Convenience constructor.
    pub fn new(
        from: impl Into<String>,
        to: impl Into<String>,
        kind: EdgeKind,
        weight: f32,
    ) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
            kind,
            weight,
        }
    }
}

/// In-memory entity/alias/edge graph.
///
/// Entities are keyed by `entity_id` (upsert overwrites), names and aliases are indexed
/// case-insensitively for `resolve_name`, and every edge is indexed under both endpoints for
/// undirected traversal in `expand`. A separate `order` vector preserves insertion order so
/// results are deterministic.
#[derive(Debug, Clone, Default)]
pub struct Graph {
    order: Vec<String>,
    entities: HashMap<String, Entity>,
    /// lowercased name/alias -> entity ids (insertion-ordered, deduped per key).
    name_index: HashMap<String, Vec<String>>,
    edges: Vec<Edge>,
    /// entity id -> indices into `edges` (an edge is listed under both endpoints).
    adjacency: HashMap<String, Vec<usize>>,
}

impl Graph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace an entity by `entity_id`. Re-indexes the name/aliases accordingly.
    pub fn upsert_entity(&mut self, e: Entity) {
        // Drop any stale name/alias index entries for this id.
        if let Some(old) = self.entities.get(&e.entity_id) {
            let old_keys: Vec<String> = std::iter::once(&old.name)
                .chain(old.aliases.iter())
                .map(|s| normalize_key(s))
                .collect();
            for key in old_keys {
                if let Some(ids) = self.name_index.get_mut(&key) {
                    ids.retain(|id| id != &e.entity_id);
                    if ids.is_empty() {
                        self.name_index.remove(&key);
                    }
                }
            }
        } else {
            self.order.push(e.entity_id.clone());
        }

        let keys: Vec<String> = std::iter::once(&e.name)
            .chain(e.aliases.iter())
            .map(|s| normalize_key(s))
            .collect();
        for key in keys {
            if key.is_empty() {
                continue;
            }
            let ids = self.name_index.entry(key).or_default();
            if !ids.iter().any(|id| id == &e.entity_id) {
                ids.push(e.entity_id.clone());
            }
        }

        self.entities.insert(e.entity_id.clone(), e);
    }

    /// Add a typed edge, indexing it under both endpoints.
    pub fn add_edge(&mut self, e: Edge) {
        let idx = self.edges.len();
        self.adjacency.entry(e.from.clone()).or_default().push(idx);
        if e.to != e.from {
            self.adjacency.entry(e.to.clone()).or_default().push(idx);
        }
        self.edges.push(e);
    }

    /// Look up an entity by canonical name or any alias, case-insensitive, exact match
    /// (not substring). Results are deduped and returned in insertion order.
    pub fn resolve_name(&self, name: &str) -> Vec<&Entity> {
        let key = normalize_key(name);
        let Some(ids) = self.name_index.get(&key) else {
            return Vec::new();
        };
        let mut seen = HashSet::new();
        ids.iter()
            .filter(|id| seen.insert((*id).clone()))
            .filter_map(|id| self.entities.get(id))
            .collect()
    }

    /// Edges incident to `entity_id`, in insertion order.
    ///
    /// Every edge is indexed under both endpoints (`add_edge`), so this returns it whether the id
    /// is the edge's `from` or its `to` — the neighbourhood walk behind graph activation.
    pub fn edges_of(&self, entity_id: &str) -> impl Iterator<Item = &Edge> {
        self.adjacency.get(entity_id).into_iter().flatten().map(move |&idx| &self.edges[idx])
    }

    /// Breadth-first expansion from `entity_id` up to `max_hops` edges away.
    ///
    /// Excludes the start entity, dedupes, and preserves BFS discovery order (deterministic
    /// given insertion-ordered adjacency). Capped at [`MAX_EXPAND_RESULTS`] for cycle safety.
    /// `max_hops == 0` yields an empty result.
    pub fn expand(&self, entity_id: &str, max_hops: u8) -> Vec<&Entity> {
        let mut out: Vec<&Entity> = Vec::new();
        if max_hops == 0 || !self.entities.contains_key(entity_id) {
            return out;
        }

        let mut seen: HashSet<&str> = HashSet::new();
        seen.insert(self.entities.get(entity_id).map(|e| e.entity_id.as_str()).unwrap_or(""));
        // (id, depth)
        let mut queue: VecDeque<(String, u8)> = VecDeque::new();
        queue.push_back((entity_id.to_string(), 0u8));

        while let Some((id, depth)) = queue.pop_front() {
            if depth >= max_hops {
                continue;
            }
            if let Some(edge_idxs) = self.adjacency.get(&id) {
                for &ei in edge_idxs {
                    let edge = &self.edges[ei];
                    let other = if edge.from == id { &edge.to } else { &edge.from };
                    if !seen.insert(other.as_str()) {
                        continue;
                    }
                    if let Some(entity) = self.entities.get(other) {
                        out.push(entity);
                        if out.len() >= MAX_EXPAND_RESULTS {
                            return out;
                        }
                        queue.push_back((other.clone(), depth + 1));
                    }
                }
            }
        }
        out
    }

    /// Number of entities currently held.
    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    /// Number of edges currently held.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Borrow an entity by id.
    pub fn get(&self, entity_id: &str) -> Option<&Entity> {
        self.entities.get(entity_id)
    }
}

/// Case-insensitive, whitespace-trimmed lookup key.
fn normalize_key(s: &str) -> String {
    s.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(id: &str, name: &str, aliases: &[&str]) -> Entity {
        Entity {
            entity_id: id.into(),
            name: name.into(),
            kind: EntityKind::Concept,
            aliases: aliases.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn resolve_name_by_name_alias_and_case() {
        let mut g = Graph::new();
        g.upsert_entity(entity("e1", "Large Language Model", &["LLM", "large language models"]));
        g.upsert_entity(entity("e2", "Retrieval", &[]));

        assert_eq!(g.resolve_name("Large Language Model").len(), 1);
        assert_eq!(g.resolve_name("large language model").len(), 1); // case-insensitive
        assert_eq!(g.resolve_name("LLM").len(), 1);
        assert_eq!(g.resolve_name("llm  ").len(), 1); // trimmed
        assert_eq!(g.resolve_name("large language models").len(), 1);

        assert!(g.resolve_name("large").is_empty()); // exact, not substring
        assert!(g.resolve_name("missing").is_empty());

        // Deduped when name and alias both index the same entity.
        let mut g2 = Graph::new();
        g2.upsert_entity(entity("x", "Foo", &["Foo"]));
        let hits = g2.resolve_name("foo");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn upsert_replaces_and_reindexes() {
        let mut g = Graph::new();
        g.upsert_entity(entity("e1", "Old Name", &["old-alias"]));
        assert_eq!(g.resolve_name("Old Name").len(), 1);

        g.upsert_entity(entity("e1", "New Name", &["new-alias"]));
        assert_eq!(g.entity_count(), 1);
        assert!(g.resolve_name("Old Name").is_empty());
        assert!(g.resolve_name("old-alias").is_empty());
        assert_eq!(g.resolve_name("New Name").len(), 1);
        assert_eq!(g.resolve_name("new-alias").len(), 1);
    }

    #[test]
    fn expand_respects_hop_limit() {
        let mut g = Graph::new();
        for id in ["a", "b", "c", "d"] {
            g.upsert_entity(entity(id, id, &[]));
        }
        g.add_edge(Edge::new("a", "b", EdgeKind::RelatedTo, 1.0));
        g.add_edge(Edge::new("b", "c", EdgeKind::RelatedTo, 1.0));
        g.add_edge(Edge::new("c", "d", EdgeKind::RelatedTo, 1.0));

        let h0 = g.expand("a", 0);
        assert!(h0.is_empty());

        let h1: Vec<&str> = g.expand("a", 1).iter().map(|e| e.entity_id.as_str()).collect();
        assert_eq!(h1, vec!["b"]);

        let h2: Vec<&str> = g.expand("a", 2).iter().map(|e| e.entity_id.as_str()).collect();
        assert_eq!(h2, vec!["b", "c"]);

        let h3: Vec<&str> = g.expand("a", 3).iter().map(|e| e.entity_id.as_str()).collect();
        assert_eq!(h3, vec!["b", "c", "d"]);

        // Unknown start id → empty.
        assert!(g.expand("nope", 5).is_empty());
    }

    #[test]
    fn expand_cycle_safe_and_excludes_start() {
        let mut g = Graph::new();
        for id in ["a", "b", "c"] {
            g.upsert_entity(entity(id, id, &[]));
        }
        g.add_edge(Edge::new("a", "b", EdgeKind::RelatedTo, 1.0));
        g.add_edge(Edge::new("b", "c", EdgeKind::RelatedTo, 1.0));
        g.add_edge(Edge::new("c", "a", EdgeKind::RelatedTo, 1.0));

        let out: Vec<&str> = g.expand("a", 10).iter().map(|e| e.entity_id.as_str()).collect();
        assert_eq!(out, vec!["b", "c"]); // no start, no duplicates, terminates
    }

    #[test]
    fn expand_dedupes_diamond() {
        let mut g = Graph::new();
        for id in ["a", "b", "c", "d"] {
            g.upsert_entity(entity(id, id, &[]));
        }
        g.add_edge(Edge::new("a", "b", EdgeKind::RelatedTo, 1.0));
        g.add_edge(Edge::new("a", "c", EdgeKind::RelatedTo, 1.0));
        g.add_edge(Edge::new("b", "d", EdgeKind::RelatedTo, 1.0));
        g.add_edge(Edge::new("c", "d", EdgeKind::RelatedTo, 1.0));

        let out: Vec<&str> = g.expand("a", 3).iter().map(|e| e.entity_id.as_str()).collect();
        assert_eq!(out, vec!["b", "c", "d"]); // d appears once
    }
}
