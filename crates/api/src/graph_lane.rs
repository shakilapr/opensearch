//! The §9.4 graph lane: query → activated entities → documents.
//!
//! This is the consumer of two things that exist separately: `agent0 graph build` now writes the
//! entity→document join as a sidecar ([`search_graph::sidecar`]), and `search_graph::activate`
//! turns the entities a query names into a ranked set of related entities. The lane joins them and
//! emits ordinary [`Candidate`]s stamped [`Lane::Graph`], which is what makes the API's
//! `graph_expansion` signal a measured fact rather than a placeholder.
//!
//! Honest limits, each deliberate:
//!
//! * **No source scope.** The lane knows entities and documents, not which source owns a document,
//!   so a `source_id`-scoped search returns nothing rather than guessing. The engine's source-first
//!   lane is the one that serves scoped queries.
//! * **No query text matching.** A candidate enters only through an activated entity, so a query
//!   naming no known entity yields an empty lane. That is the honest answer, and it is why the lane
//!   is fused with lexical retrieval rather than replacing it.
//! * **Scores are relative within the lane.** They are the activation mass summed per document and
//!   normalised to `(0, 1]`, so the strongest candidate is `1.0`. They are not comparable to a
//!   BM25 score; the engine fuses by rank, not by raw value.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use search_graph::activate::{activate, ActivationOptions};
use search_graph::extract::extract_mentions;
use search_graph::sidecar::GraphSidecar;
use search_graph::Graph;
use search_retrieval::{Candidate, Lane, LaneSearcher, RetrievalError};

/// Cap on the mention candidates taken from one query. A query is short; this only bounds the work
/// when someone searches with a paragraph.
const QUERY_MENTIONS: usize = 16;

pub struct GraphLaneSearcher {
    graph: Graph,
    /// entity id → documents mentioning it.
    docs: HashMap<String, Vec<String>>,
    opts: ActivationOptions,
    entity_count: usize,
}

impl GraphLaneSearcher {
    /// Build a lane over an already-parsed sidecar.
    pub fn new(sidecar: GraphSidecar) -> Self {
        let entity_count = sidecar.entity_count();
        let docs = sidecar
            .entities
            .iter()
            .map(|e| (e.entity_id.clone(), e.docs.clone()))
            .collect();
        Self { graph: sidecar.to_graph(), docs, opts: ActivationOptions::default(), entity_count }
    }

    /// Load the sidecar at `path`. An absent file is a normal condition — a corpus that has not had
    /// `agent0 graph build` run over it has no graph lane — but a present-yet-unreadable file is an
    /// error, because silently serving without the lane would be indistinguishable from a graph
    /// that matched nothing.
    pub fn from_path(path: &Path) -> io::Result<Self> {
        Ok(Self::new(search_graph::sidecar::load(path)?))
    }

    /// The number of entities the lane can reach.
    pub fn entity_count(&self) -> usize {
        self.entity_count
    }

    /// Entity ids the query names (by canonical name or alias), deduplicated and order-stable.
    fn seeds(&self, query: &str) -> Vec<String> {
        let mut seeds: Vec<String> = Vec::new();
        // `extract_mentions` is tuned for documents (recurrence, casing runs); a query is a few
        // words, so the whole trimmed query is tried as a phrase first, then each extracted key.
        let mut candidates: Vec<String> = Vec::new();
        let trimmed = query.trim();
        if !trimmed.is_empty() {
            candidates.push(trimmed.to_string());
        }
        for mention in extract_mentions(query, 1, QUERY_MENTIONS) {
            candidates.push(mention.name);
            candidates.push(mention.key);
        }
        for candidate in candidates {
            for entity in self.graph.resolve_name(&candidate) {
                if !seeds.iter().any(|s| s == &entity.entity_id) {
                    seeds.push(entity.entity_id.clone());
                }
            }
        }
        seeds
    }
}

impl LaneSearcher for GraphLaneSearcher {
    fn search(
        &self,
        query: &str,
        limit: usize,
        source_id: Option<&str>,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        // Documented limitation, enforced rather than ignored: the lane cannot honour a source
        // filter, and answering a scoped query with unscoped hits would leak across sources.
        if source_id.is_some() {
            return Ok(Vec::new());
        }
        let seeds = self.seeds(query);
        if seeds.is_empty() {
            return Ok(Vec::new());
        }

        let activated = activate(&self.graph, &seeds, self.opts);
        if activated.is_empty() {
            return Ok(Vec::new());
        }

        // A document reached by several activated entities collects all of their mass.
        let mut by_doc: HashMap<&str, f32> = HashMap::new();
        for (entity_id, score) in &activated {
            let Some(docs) = self.docs.get(entity_id) else { continue };
            for doc in docs {
                *by_doc.entry(doc.as_str()).or_insert(0.0) += *score;
            }
        }
        if by_doc.is_empty() {
            return Ok(Vec::new());
        }

        let best = by_doc.values().copied().fold(0.0f32, f32::max);
        if !(best > 0.0) {
            return Ok(Vec::new());
        }

        let mut hits: Vec<(&str, f32)> = by_doc
            .into_iter()
            .map(|(doc, score)| {
                let normalised = score / best;
                // Never `-0.0`: the project has been bitten by it reaching clients as "-0.0".
                (doc, if normalised > 0.0 { normalised } else { 0.0 })
            })
            .collect();
        // Deterministic: score descending, then document id ascending.
        hits.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(b.0))
        });
        hits.truncate(limit.max(1));

        Ok(hits
            .into_iter()
            .map(|(doc, score)| Candidate::new(doc, score).with_lane(Lane::Graph))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use search_graph::sidecar::{SidecarEdge, SidecarEntity};

    fn entity(id: &str, name: &str, aliases: &[&str], docs: &[&str]) -> SidecarEntity {
        SidecarEntity {
            entity_id: id.to_string(),
            name: name.to_string(),
            aliases: aliases.iter().map(|a| a.to_string()).collect(),
            docs: docs.iter().map(|d| d.to_string()).collect(),
        }
    }

    /// vLLM and Paged Attention co-occur in `doc-2`; only vLLM is in `doc-1`.
    fn lane() -> GraphLaneSearcher {
        GraphLaneSearcher::new(GraphSidecar {
            entities: vec![
                entity("vllm", "vLLM", &["VLLM"], &["doc-1", "doc-2"]),
                entity("paged attention", "Paged Attention", &[], &["doc-2"]),
            ],
            edges: vec![SidecarEdge {
                from: "vllm".to_string(),
                to: "paged attention".to_string(),
                weight: 3.0,
            }],
        })
    }

    #[test]
    fn a_query_naming_a_known_entity_reaches_its_documents_and_its_neighbours() {
        let lane = lane();
        let hits = lane.search("vLLM", 10, None).expect("search");
        let docs: Vec<&str> = hits.iter().map(|h| h.doc_id.as_str()).collect();
        assert!(docs.contains(&"doc-1"), "the named entity's own documents: {docs:?}");
        assert!(
            docs.contains(&"doc-2"),
            "a document of the co-occurring entity, reached by activation: {docs:?}"
        );
        assert!(hits.iter().all(|h| h.lane == Lane::Graph), "every hit is a graph hit");
        assert!(
            hits.iter().all(|h| h.score > 0.0 && h.score <= 1.0),
            "scores are relative and bounded: {:?}",
            hits.iter().map(|h| h.score).collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_alias_resolves_the_same_way_as_the_canonical_name() {
        let lane = lane();
        let canonical = lane.search("vLLM", 10, None).expect("search");
        let alias = lane.search("VLLM", 10, None).expect("search");
        assert_eq!(canonical, alias, "an alias must seed the same walk");
    }

    #[test]
    fn a_query_naming_nothing_known_yields_no_hits_rather_than_a_guess() {
        let lane = lane();
        assert!(lane.search("an entirely unrelated phrase", 10, None).expect("search").is_empty());
    }

    #[test]
    fn a_source_scoped_search_returns_nothing_because_the_lane_cannot_scope() {
        // Answering with unscoped hits would leak documents across sources; an empty lane is the
        // honest answer, and the engine's source-first lane serves scoped queries.
        let lane = lane();
        assert!(lane.search("vLLM", 10, Some("src-1")).expect("search").is_empty());
    }

    #[test]
    fn the_output_order_is_deterministic_and_bounded_by_the_limit() {
        let lane = lane();
        let first = lane.search("vLLM", 10, None).expect("search");
        let second = lane.search("vLLM", 10, None).expect("search");
        assert_eq!(first, second);
        assert_eq!(lane.search("vLLM", 1, None).expect("search").len(), 1);
    }

    #[test]
    fn the_lane_name_is_what_the_api_reads_as_graph_expansion() {
        // `build_signals` sets `graph_expansion` when the hit's lane contains "graph"; this is the
        // contract that makes the lane visible to the "Why this result?" panel.
        assert_eq!(crate::engine::lane_name(Lane::Graph), "graph");
        assert!(crate::engine::lane_name(Lane::Graph).contains("graph"));
    }

    #[test]
    fn an_empty_graph_produces_no_hits_instead_of_panicking() {
        let lane = GraphLaneSearcher::new(GraphSidecar::default());
        assert_eq!(lane.entity_count(), 0);
        assert!(lane.search("vLLM", 10, None).expect("search").is_empty());
    }
}
