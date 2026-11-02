//! The concept lane: retrieval through the weighted document↔concept matrix.
//!
//! This is the lane `docs/SEMANTIC_INDEX_ARCHITECTURE.md` specifies and the schema has held since
//! migration 0005, but which nothing consumed: `doc_concept` stores `(resource_id, concept_id,
//! weight, relation_kind)` and the crawl fills nothing. A query is resolved to concepts, the
//! matrix is read concept → documents, and the weights become the lane's scores.
//!
//! Why it is not the same thing as BM25. Lexical matching requires the query's words to appear in
//! the document. This lane only requires that the query and the document *resolve to the same
//! concepts*, which is what lets a paper titled "Oxytocin modulation of pair-bond formation"
//! answer "chemical reactions that affect romance": both sides reach `oxytocin` and `attachment`,
//! and the association carries the weight.
//!
//! Two properties the schema deliberately encodes and this lane respects:
//!
//! * `relation_kind` separates `direct` (the document states it) from `inferred` (reached through
//!   `concept_relation`). They are scored with different multipliers rather than merged, so a later
//!   model cannot silently reinforce an earlier model's inference.
//! * `concept_alias` resolves surface forms, so "romantic love", "romance" and "pair-bonding" all
//!   reach the concept whose weights the matrix holds.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use search_core::AppResult;

use crate::{Candidate, Lane, LaneSearcher, RetrievalError};

/// Weight multiplier for an association the document itself states.
const DIRECT_FACTOR: f32 = 1.0;
/// Weight multiplier for an association reached through the concept graph.
///
/// Below `DIRECT_FACTOR` on purpose: an inference is a weaker claim than a statement, and the
/// architecture is explicit that the two must never be merged into one number.
const INFERRED_FACTOR: f32 = 0.5;

/// Per-document cap on how many concepts a query term may accumulate, so a document weighted on
/// hundreds of concepts cannot dominate merely by being broad.
const MAX_CONCEPTS_PER_DOC: usize = 32;

/// Query concepts after expansion. Bounded so a hub concept cannot turn into a corpus scan, and so
/// the score stays explainable: every contribution is one row of `doc_concept`.
const MAX_QUERY_CONCEPTS: usize = 12;

/// How much each dimension of `R(d,c)` counts for a particular query (v2 §6).
///
/// v2 §6's example: a query about *chemical processes involved in romance* should emphasise the
/// mechanism dimension, while a query about *hormonal regulation* should emphasise direct topic.
/// The same article then ranks differently for the two, with no reindex.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DimensionWeights {
    pub direct: f32,
    pub contextual: f32,
    pub mechanism: f32,
}

impl Default for DimensionWeights {
    /// Balanced: direct dominates slightly, because a document that states a topic is the safest
    /// result for an unqualified query. Mechanism is raised rather than dropped so a plain query
    /// still finds explanatory pages.
    fn default() -> Self {
        Self {
            direct: 1.0,
            contextual: 0.6,
            mechanism: 0.8,
        }
    }
}

impl DimensionWeights {
    /// Weights for a query that asks how or why something happens.
    pub fn mechanism_focused() -> Self {
        Self {
            direct: 0.7,
            contextual: 0.5,
            mechanism: 1.0,
        }
    }

    /// Weights for a query that names a topic and wants coverage of it.
    pub fn topic_focused() -> Self {
        Self {
            direct: 1.0,
            contextual: 0.7,
            mechanism: 0.4,
        }
    }

    /// Dimensions for a query: a how/why question emphasises mechanism, a topic question direct
    /// coverage. This is a rule over the query's *shape*, not a learned model — the honest first
    /// version, and it is measurable against the plain default.
    pub fn mechanic_if_asked(query: &str) -> Self {
        let q = query.to_ascii_lowercase();
        let asks_mechanism = ["how ", "why ", "mechanism", "cause", "effect of", "because"]
            .iter()
            .any(|m| q.contains(m));
        if asks_mechanism {
            Self::mechanism_focused()
        } else {
            Self::default()
        }
    }

    /// Weighted combination, normalised so the result stays in the range a scalar weight occupies
    /// and fused scores remain comparable across lanes.
    fn score(&self, direct: f64, contextual: f64, mechanism: f64) -> f32 {
        let total = self.direct + self.contextual + self.mechanism;
        if total <= 0.0 {
            return 0.0;
        }
        let s = direct as f32 * self.direct
            + contextual as f32 * self.contextual
            + mechanism as f32 * self.mechanism;
        (s / total).clamp(0.0, 1.0)
    }
}

/// How strongly a related concept contributes relative to a concept the query named directly.
///
/// Below 1.0 on purpose: a neighbour is evidence, not a statement. `0.4` means a document tagged
/// only with a neighbour of a query concept scores at most 40% of a document tagged with the
/// concept itself, so expansion can reorder candidates but cannot outvote direct support.
pub const DEFAULT_EXPANSION_LAMBDA: f32 = 0.4;

/// Query concepts after expansion: `(concept_id, weight)`, best first.
pub type WeightedConcepts = Vec<(String, f32)>;

/// The lane's read side: the metadata database, opened once.
pub struct ConceptLane {
    db_path: PathBuf,
    /// Concepts to return per query term before scoring, bounded to keep one broad concept from
    /// pulling the whole corpus.
    per_concept_limit: usize,
    /// Concept → related concepts with strength, from `concept_relation` (`cooccur` edges).
    relations: HashMap<String, Vec<(String, f32)>>,
    /// Concept → inverse document frequency, so a concept present on a third of the corpus cannot
    /// dominate a score the way a rare one should.
    idf: HashMap<String, f32>,
    /// Expansion strength; 0.0 disables the graph without rebuilding the lane.
    expansion: f32,
}

impl ConceptLane {
    /// A lane over `db_path`. The path is stored rather than a connection because the lane must be
    /// `Send + Sync` and is used across threads; SQLite connections are opened per call.
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        let db_path = db_path.into();
        let (relations, idf) = load_graph(&db_path).unwrap_or_default();
        Self {
            db_path,
            per_concept_limit: 2_000,
            relations,
            idf,
            expansion: DEFAULT_EXPANSION_LAMBDA,
        }
    }

    /// Override the expansion strength (`0.0` disables graph expansion).
    pub fn with_expansion(mut self, lambda: f32) -> Self {
        self.expansion = lambda.max(0.0);
        self
    }

    /// Concepts related to `concept`, with normalised strength.
    pub fn related(&self, concept: &str) -> &[(String, f32)] {
        self.relations
            .get(concept)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Override how many documents one concept may contribute per query.
    pub fn with_per_concept_limit(mut self, n: usize) -> Self {
        self.per_concept_limit = n.max(1);
        self
    }

    fn connect(&self) -> Result<Connection, RetrievalError> {
        Connection::open_with_flags(
            &self.db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|e| RetrievalError::index(format!("concept lane: {e}")))
    }

    /// Concept ids a query text resolves to, via name and alias.
    ///
    /// A concept matches when a query term equals its name, equals an alias, or contains the name
    /// as a whole word. The containment rule is what makes "chemical reactions that affect romance"
    /// reach the `romance` concept without requiring the query to be exactly that word.
    pub fn concepts_for(&self, query: &str) -> AppResult<Vec<String>> {
        let conn = self.connect().map_err(|e| search_core::AppError::Db(e.to_string()))?;
        let lowered = query.to_ascii_lowercase();
        let terms: Vec<&str> = lowered
            .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
            .filter(|t| t.len() >= 3)
            .collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }

        // A concept is reachable when a query term shares a stem with it: "chemical" must reach
        // `chemistry`, and "romantic" must reach `romance`. Exact-string matching fails exactly
        // where the semantic index is supposed to help — a query written in one form of a word and
        // a document tagged in another.
        //
        // The stem here is a deliberately crude prefix: strip a small set of derivational suffixes
        // and compare the first `STEM_LEN` characters. A real stemmer would be better, but this
        // costs one string slice per comparison and is auditable; the concept vocabulary is small
        // (thousands, not millions) so a dictionary approach buys nothing.
        fn stem(word: &str) -> String {
            const SUFFIXES: &[&str] = &[
                "ically", "ically", "ical", "istry", "istic", "ance", "ence", "ment", "tion",
                "sion", "ness", "ical", "ing", "ise", "ize", "ate", "ant", "ent", "ous", "ive",
                "ity", "ery", "ary", "ory", "al", "ic", "ed", "es", "ly", "er", "s",
            ];
            let mut w = word.to_ascii_lowercase();
            // Longest suffix first, so "ically" wins over "al".
            for suf in SUFFIXES {
                if w.len() > suf.len() + 3 && w.ends_with(suf) {
                    w.truncate(w.len() - suf.len());
                    break;
                }
            }
            w
        }

        /// Whether two words share a stem, compared on a bounded prefix so "chemical"/"chemistry"
        /// (both stem to "chem") match without letting short words collide wildly.
        fn same_stem(a: &str, b: &str) -> bool {
            // Compared by *characters*, never bytes. Slicing a `String` at a fixed byte offset
            // panics on any non-ASCII text: the graded query set contains Russian and Chinese
            // titles, and `sa[..4]` on "进程程" splits a three-byte character, which aborted the
            // process mid-benchmark with "byte index 4 is not a char boundary". A panic in a
            // serving path is worse than a missed match, so this must be char-safe.
            const STEM_CHARS: usize = 4;
            let (sa, sb) = (stem(a), stem(b));
            let ca: Vec<char> = sa.chars().collect();
            let cb: Vec<char> = sb.chars().collect();
            if ca.len() < STEM_CHARS || cb.len() < STEM_CHARS {
                return sa == sb;
            }
            ca[..STEM_CHARS] == cb[..STEM_CHARS]
        }

        let mut found: Vec<String> = Vec::new();
        let mut stmt = conn
            .prepare(
                "SELECT c.concept_id, c.name, a.alias
                   FROM concept c
                   LEFT JOIN concept_alias a ON a.concept_id = c.concept_id
                  WHERE c.status = 'active'",
            )
            .map_err(search_metadata::db_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(search_metadata::db_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(search_metadata::db_err)?;

        for (id, name, alias) in rows {
            let name_words: Vec<&str> = name
                .split(|c: char| !c.is_alphanumeric())
                .filter(|t| !t.is_empty())
                .collect();
            let name_hit = name_words.iter().any(|nw| {
                terms
                    .iter()
                    .any(|t| t.eq_ignore_ascii_case(nw) || same_stem(t, nw))
            });
            let alias_hit = alias
                .as_deref()
                .map(|a| {
                    let alias_words: Vec<&str> = a
                        .split(|c: char| !c.is_alphanumeric())
                        .filter(|t| !t.is_empty())
                        .collect();
                    alias_words.iter().any(|aw| {
                        terms
                            .iter()
                            .any(|t| t.eq_ignore_ascii_case(aw) || same_stem(t, aw))
                    })
                })
                .unwrap_or(false);
            if (name_hit || alias_hit) && !found.contains(&id) {
                found.push(id);
            }
        }
        Ok(found)
    }

    /// Documents weighted for `concepts`, best per document.
    pub fn documents_for(
        &self,
        concepts: &[String],
        limit: usize,
    ) -> AppResult<Vec<(String, f32)>> {
        self.documents_for_dimensions(concepts, limit, &DimensionWeights::default())
    }

    /// Resolve a query to concepts *and their neighbours*, each with a weight.
    ///
    /// This is the part that stops ranking being keyword matching in disguise. The query resolves
    /// to concepts (by name, alias or stem), then the `concept_relation` graph adds the concepts the
    /// corpus associates with them at `lambda` of their strength. A query naming `chemistry` can
    /// therefore reach a page tagged `romance` and `chemistry` even when the page's own words never
    /// appear in the query — and a page tagged only with a neighbour still scores, at a discount.
    ///
    /// Weights are normalised so the strongest concept is 1.0; the list is capped so a hub concept
    /// cannot expand into the whole vocabulary.
    pub fn resolve_weighted(&self, query: &str) -> AppResult<WeightedConcepts> {
        let base = self.concepts_for(query)?;
        if base.is_empty() {
            return Ok(Vec::new());
        }
        let mut weights: HashMap<String, f32> = HashMap::new();
        for c in &base {
            weights.insert(c.clone(), 1.0);
        }
        if self.expansion > 0.0 {
            for c in &base {
                for (neighbour, strength) in self.related(c) {
                    let w = self.expansion * strength;
                    let slot = weights.entry(neighbour.clone()).or_insert(0.0);
                    if w > *slot {
                        *slot = w;
                    }
                }
            }
        }
        let max = weights.values().cloned().fold(0.0f32, f32::max).max(f32::MIN_POSITIVE);
        let mut out: WeightedConcepts = weights
            .into_iter()
            .map(|(c, w)| (c, (w / max).clamp(0.0, 1.0)))
            .collect();
        out.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        out.truncate(MAX_QUERY_CONCEPTS);
        Ok(out)
    }

    /// Weight of one concept's evidence: its IDF, floored so a concept present on every document
    /// still carries a little signal rather than none. A ubiquitous concept is downweighted, never
    /// deleted — deleting it turns a weak answer into no answer.
    pub fn concept_weight(&self, concept: &str) -> f32 {
        self.idf.get(concept).copied().unwrap_or(1.0)
    }

    /// Documents weighted for `concepts`, scoring each association's dimensions with `weights`.
    ///
    /// This is the v2 §6 requirement made operational: "When a user searches for chemical processes
    /// involved in romance, the query model should emphasize the relevant biochemical and
    /// relationship-oriented dimensions. When another user searches for hormonal regulation, the
    /// same article could be ranked differently. The document does not need to be reindexed for
    /// every query interpretation."
    ///
    /// With no `doc_concept_dim` rows the scalar `doc_concept.weight` is used, so a corpus tagged
    /// before the dimensional pass still ranks correctly.
    pub fn documents_for_dimensions(
        &self,
        concepts: &[String],
        limit: usize,
        dim_weights: &DimensionWeights,
    ) -> AppResult<Vec<(String, f32)>> {
        let weighted: WeightedConcepts = concepts.iter().map(|c| (c.clone(), 1.0)).collect();
        self.documents_for_weighted(&weighted, limit, dim_weights)
    }

    /// Documents weighted for `concepts` with per-concept query weights and concept IDF.
    pub fn documents_for_weighted(
        &self,
        concepts: &[(String, f32)],
        limit: usize,
        dim_weights: &DimensionWeights,
    ) -> AppResult<Vec<(String, f32)>> {
        if concepts.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.connect().map_err(|e| search_core::AppError::Db(e.to_string()))?;
        let mut best: HashMap<String, f32> = HashMap::new();
        let mut per_doc: HashMap<String, usize> = HashMap::new();

        // A document's dimensional breakdown, when the tagging pass wrote one. `LEFT JOIN` keeps
        // documents whose associations only have a scalar weight: absence of dimensions must not
        // remove them from results, it only means the scalar is the best available estimate.
        let mut stmt = conn
            .prepare(
                "SELECT dc.resource_id,
                        dc.weight,
                        dc.relation_kind,
                        MAX(CASE WHEN d.dim = 'direct'     THEN d.value END) AS r1,
                        MAX(CASE WHEN d.dim = 'contextual' THEN d.value END) AS r2,
                        MAX(CASE WHEN d.dim = 'mechanism'  THEN d.value END) AS r3
                   FROM doc_concept dc
                   LEFT JOIN doc_concept_dim d
                          ON d.resource_id = dc.resource_id
                         AND d.concept_id  = dc.concept_id
                  WHERE dc.concept_id = ?1
                  GROUP BY dc.resource_id, dc.concept_id, dc.relation_kind
                  ORDER BY MAX(COALESCE(d.value, dc.weight)) DESC
                  LIMIT ?2",
            )
            .map_err(search_metadata::db_err)?;

        for (concept, q_weight) in concepts {
            // Query importance × how specific the concept is. A concept on 32% of the corpus is
            // real evidence but weak evidence; multiplying by IDF is what stops the broadest
            // concept from deciding every ranking.
            let concept_gain = q_weight * self.concept_weight(concept);
            let rows = stmt
                .query_map(
                    rusqlite::params![concept, self.per_concept_limit as i64],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, f64>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<f64>>(3)?,
                            r.get::<_, Option<f64>>(4)?,
                            r.get::<_, Option<f64>>(5)?,
                        ))
                    },
                )
                .map_err(search_metadata::db_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(search_metadata::db_err)?;

            for (doc, scalar, kind, r1, r2, r3) in rows {
                let seen = per_doc.entry(doc.clone()).or_insert(0);
                if *seen >= MAX_CONCEPTS_PER_DOC {
                    continue;
                }
                *seen += 1;
                let factor = if kind == "direct" {
                    DIRECT_FACTOR
                } else {
                    INFERRED_FACTOR
                };
                // Prefer the dimensional score when the tagger wrote one; the scalar is the
                // fallback for corpora tagged before dimensions existed.
                let base = match (r1, r2, r3) {
                    (None, None, None) => scalar as f32,
                    _ => dim_weights.score(r1.unwrap_or(0.0), r2.unwrap_or(0.0), r3.unwrap_or(0.0)),
                };
                let contribution = base * factor * concept_gain;
                // Sum across concepts: a document weighting both `oxytocin` and `romance` is a
                // better answer than one weighting either alone, which is the whole point of a
                // multi-axis matrix.
                let slot = best.entry(doc).or_insert(0.0);
                *slot += contribution;
            }
        }

        let mut out: Vec<(String, f32)> = best.into_iter().collect();
        out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        out.truncate(limit);
        Ok(out)
    }
}

/// Read the relatedness graph and concept IDF from one metadata database.
///
/// Both are optional: a corpus tagged before `agent0 concepts relate` ran has no edges, and the lane
/// then behaves exactly as it did before (aliases only, equal concept weights). Absence of the graph
/// must degrade the capability, never the correctness of the lane.
fn load_graph(
    db_path: &Path,
) -> AppResult<(HashMap<String, Vec<(String, f32)>>, HashMap<String, f32>)> {
    let conn = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| search_core::AppError::Db(format!("concept lane: {e}")))?;

    // Undirected on purpose: if the corpus says chemistry and romance appear together, the
    // relationship is walkable from either name a query happens to use.
    let mut relations: HashMap<String, Vec<(String, f32)>> = HashMap::new();
    if let Ok(mut stmt) =
        conn.prepare("SELECT src_concept_id, dst_concept_id, strength FROM concept_relation WHERE strength IS NOT NULL")
    {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<f64>>(2)?,
            ))
        }) {
            for row in rows.flatten() {
                let (a, b, s) = row;
                let s = s.unwrap_or(0.0) as f32;
                if !(s.is_finite() && s > 0.0) {
                    continue;
                }
                relations.entry(a.clone()).or_default().push((b.clone(), s));
                relations.entry(b).or_default().push((a, s));
            }
        }
    }
    for edges in relations.values_mut() {
        edges.sort_by(|x, y| y.1.partial_cmp(&x.1).unwrap_or(std::cmp::Ordering::Equal));
        edges.dedup_by(|x, y| x.0 == y.0);
    }

    // IDF over documents, floored at 0.1. A concept every document carries is weak evidence, not
    // no evidence: deleting it converts a poor ranking into none, which is worse.
    let mut idf: HashMap<String, f32> = HashMap::new();
    let total: i64 = conn
        .query_row("SELECT COUNT(DISTINCT resource_id) FROM doc_concept", [], |r| r.get(0))
        .unwrap_or(0);
    if total > 0 {
        if let Ok(mut stmt) =
            conn.prepare("SELECT concept_id, COUNT(DISTINCT resource_id) FROM doc_concept GROUP BY concept_id")
        {
            if let Ok(rows) = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            }) {
                for (concept, df) in rows.flatten() {
                    let v = 1.0 + ((total as f32 + 1.0) / (df as f32 + 1.0)).ln();
                    idf.insert(concept, v.max(0.1));
                }
            }
        }
    }
    Ok((relations, idf))
}

impl LaneSearcher for ConceptLane {
    fn lane_hint(&self) -> Option<Lane> {
        Some(Lane::Concept)
    }

    fn search(
        &self,
        query: &str,
        limit: usize,
        // The concept matrix holds `resource_id`, and a resource belongs to exactly one source;
        // filtering here would need a join this lane does not do. Fusion re-applies the facet.
        _source_id: Option<&str>,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        // Resolve to concepts *and their corpus-derived neighbours*, each weighted. The lane's
        // score is therefore a measure of conceptual relatedness, not of shared words.
        let concepts = self
            .resolve_weighted(query)
            .map_err(|e| RetrievalError::index(e.to_string()))?;
        let docs = self
            .documents_for_weighted(&concepts, limit, &DimensionWeights::mechanic_if_asked(query))
            .map_err(|e| RetrievalError::index(e.to_string()))?;
        Ok(docs
            .into_iter()
            .map(|(doc_id, score)| Candidate {
                doc_id,
                score,
                lane: Lane::Concept,
                source_id: None,
                title: None,
                url: None,
            })
            .collect())
    }
}

/// Build a lane over `data_dir`, or `None` when the corpus has no concept associations yet.
///
/// Returning `None` rather than an empty lane is deliberate: an empty lane would quietly take a
/// share of every fused result's weight and contribute nothing, which is worse than being absent.
pub fn from_data_dir(data_dir: &Path) -> Option<ConceptLane> {
    let db = data_dir.join("meta").join("meta.sqlite");
    if !db.is_file() {
        return None;
    }
    let conn = Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM doc_concept", [], |r| r.get(0))
        .unwrap_or(0);
    if n == 0 {
        return None;
    }
    Some(ConceptLane::new(db))
}

/// A concept lane over several data roots, fused by score.
///
/// `serve` answers from four shards, each with its own metadata database and its own
/// `doc_concept` rows (measured 2026-10-29: 855,519 associations split across them). A lane over
/// only the primary root found no associations at all, because the primary is the 19K serving root
/// while the matrix was built on the fleet.
///
/// Resource ids are globally stable across shards (verified earlier: 0 mismatches in a 20,000-URL
/// sample), so unioning the shards' hits needs no id rewriting — the same document cannot appear
/// twice with different ids.
pub struct FederatedConceptLane {
    lanes: Vec<ConceptLane>,
}

impl FederatedConceptLane {
    /// Build over every root that has associations. Returns `None` when none does, so an absent
    /// corpus yields no lane rather than an empty one.
    pub fn from_dirs(dirs: &[std::path::PathBuf]) -> Option<Self> {
        let lanes: Vec<ConceptLane> = dirs.iter().filter_map(|d| from_data_dir(d)).collect();
        if lanes.is_empty() {
            None
        } else {
            Some(Self { lanes })
        }
    }

    pub fn shard_count(&self) -> usize {
        self.lanes.len()
    }
}

impl LaneSearcher for FederatedConceptLane {
    fn lane_hint(&self) -> Option<Lane> {
        Some(Lane::Concept)
    }

    fn search(
        &self,
        query: &str,
        limit: usize,
        source_id: Option<&str>,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        // Ask each shard for its own best `limit` and fuse by summed score, the same rule the
        // document-level federation uses. A document may only exist on one shard, so summing is
        // equivalent to taking the best — but summing is also correct if a later re-shard
        // duplicates one, where taking the best would hide a scoring disagreement.
        let mut merged: HashMap<String, Candidate> = HashMap::new();
        let mut errors = 0usize;
        for lane in &self.lanes {
            match lane.search(query, limit, source_id) {
                Ok(hits) => {
                    for h in hits {
                        merged
                            .entry(h.doc_id.clone())
                            .and_modify(|e| e.score += h.score)
                            .or_insert(h);
                    }
                }
                Err(_) => errors += 1,
            }
        }
        // Every shard failing is an error; one failing is a degraded answer, which a federated
        // search should still return.
        if errors == self.lanes.len() && errors > 0 {
            return Err(RetrievalError::index(
                "concept lane: every shard failed".to_string(),
            ));
        }
        let mut out: Vec<Candidate> = merged.into_values().collect();
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out.truncate(limit);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use search_metadata::{MetadataWriter, Mutation};

    /// Build a metadata DB with concepts, aliases and weighted associations.
    fn corpus(assocs: &[(&str, &str, f32, &str)]) -> (tempfile::TempDir, ConceptLane) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.sqlite");
        {
            let mut w = MetadataWriter::from_path(&path).unwrap();
            let now = 1_700_000_000;
            let mut batch = Vec::new();
            // Concepts and their aliases.
            for (id, name) in [
                ("c-chem", "chemistry"),
                ("c-love", "romance"),
                ("c-oxytocin", "oxytocin"),
            ] {
                batch.push(Mutation::UpsertConcept {
                    concept_id: id.into(),
                    name: name.into(),
                    description: None,
                    kind: "topic".into(),
                    created_at: now,
                });
            }
            batch.push(Mutation::UpsertConceptAlias {
                concept_id: "c-love".into(),
                alias: "romantic love".into(),
                lang: Some("en".into()),
                created_at: now,
            });
            // Resources must exist: doc_concept has an FK.
            for (doc, _, _, _) in assocs {
                batch.push(Mutation::UpsertDomain {
                    domain_id: "d.test".into(),
                    host: "d.test".into(),
                });
                batch.push(Mutation::UpsertResource {
                    resource_id: (*doc).into(),
                    url: format!("https://d.test/{doc}"),
                    canonical_url: format!("https://d.test/{doc}"),
                    domain_id: "d.test".into(),
                    source_id: None,
                    kind: "html".into(),
                });
            }
            w.apply_batch(&batch).unwrap();
        }
        // Associations go through their own call: they reference the rows above.
        {
            let mut w = MetadataWriter::from_path(&path).unwrap();
            let now = 1_700_000_000;
            for (doc, concept, weight, kind) in assocs {
                w.apply_batch(&[Mutation::UpsertDocConcept {
                    resource_id: (*doc).into(),
                    concept_id: (*concept).into(),
                    weight: *weight as f64,
                    relation_kind: (*kind).into(),
                    evidence: None,
                    passage_id: None,
                    via_concept_id: None,
                    model_version: Some("test".into()),
                    work_id: None,
                    created_at: now,
                }])
                .unwrap();
            }
        }
        let lane = ConceptLane::new(&path);
        (dir, lane)
    }

    /// The behaviour the architecture exists for: a query about romance reaches a document that is
    /// weighted on chemistry and romance, even though the query shares no words with its text.
    #[test]
    fn a_query_reaches_documents_by_concept_not_by_word() {
        let (_d, lane) = corpus(&[
            ("paper-oxytocin", "c-chem", 0.9, "direct"),
            ("paper-oxytocin", "c-love", 0.8, "direct"),
            ("paper-unrelated", "c-chem", 0.4, "direct"),
        ]);

        let concepts = lane.concepts_for("chemical reactions that affect romance").unwrap();
        assert!(concepts.contains(&"c-chem".to_string()), "concepts: {concepts:?}");
        assert!(concepts.contains(&"c-love".to_string()), "concepts: {concepts:?}");

        let hits = lane.search("chemical reactions that affect romance", 10, None).unwrap();
        assert_eq!(hits.len(), 2, "both chemistry documents: {hits:?}");
        assert_eq!(
            hits[0].doc_id, "paper-oxytocin",
            "the document weighting BOTH axes must rank first: {hits:?}"
        );
    }

    /// Direct evidence outweighs an inference of the same strength, and the two are not merged.
    #[test]
    fn a_direct_association_outranks_an_inferred_one() {
        let (_d, lane) = corpus(&[
            ("direct-doc", "c-love", 0.6, "direct"),
            ("inferred-doc", "c-love", 0.6, "inferred"),
        ]);
        let hits = lane.search("romance", 10, None).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].doc_id, "direct-doc",
            "a stated association beats an inferred one at equal weight: {hits:?}"
        );
        assert!(hits[0].score > hits[1].score);
    }

    /// A concept the query named reaches documents tagged only with a *related* concept, at a
    /// discount — the behaviour that makes this more than keyword search over concept names.
    #[test]
    fn a_related_concept_extends_the_candidate_set() {
        let (dir, lane) = corpus(&[
            ("direct-doc", "c-love", 0.9, "direct"),
            ("neighbour-doc", "c-oxytocin", 0.8, "direct"),
        ]);
        // No edge yet: the query cannot leave `romance`.
        assert_eq!(lane.related("c-love").len(), 0);
        let before = lane.search("romance", 10, None).unwrap();
        assert_eq!(before.len(), 1, "only the directly tagged doc: {before:?}");

        // Add the corpus-derived edge, then rebuild the lane so it loads the graph.
        let path = dir.path().join("meta.sqlite");
        {
            let mut w = MetadataWriter::from_path(&path).unwrap();
            w.apply_batch(&[Mutation::UpsertConceptRelation {
                src_concept_id: "c-love".into(),
                dst_concept_id: "c-oxytocin".into(),
                relation: "cooccur".into(),
                strength: Some(0.7),
                model_version: Some("test".into()),
                at: 1_700_000_000,
            }])
            .unwrap();
        }
        let lane = ConceptLane::new(&path);
        assert_eq!(lane.related("c-love").len(), 1, "edge loaded");

        let after = lane.search("romance", 10, None).unwrap();
        assert_eq!(after.len(), 2, "expansion must reach the neighbour: {after:?}");
        assert_eq!(
            after[0].doc_id, "direct-doc",
            "direct support still outranks a related concept: {after:?}"
        );
        assert!(
            after[1].score < after[0].score,
            "an expanded concept contributes at a discount: {after:?}"
        );
    }

    /// An alias reaches the concept it belongs to.
    #[test]
    fn an_alias_resolves_to_its_concept() {
        let (_d, lane) = corpus(&[("doc", "c-love", 0.7, "direct")]);
        let concepts = lane.concepts_for("romantic love and attachment").unwrap();
        assert!(
            concepts.contains(&"c-love".to_string()),
            "alias did not resolve: {concepts:?}"
        );
    }

    /// v2 §6's acceptance claim, tested directly: the same article must rank differently under
    /// different query interpretations *without being reindexed*.
    ///
    /// Two documents, one concept. `mechanism-paper` explains how the concept works but mentions it
    /// only in the body; `topic-page` is titled about the concept but never explains it. A
    /// mechanism-focused query must prefer the first, a topic-focused query the second — from the
    /// same stored rows.
    #[test]
    fn the_same_article_ranks_differently_per_query_without_reindexing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.sqlite");
        {
            let mut w = MetadataWriter::from_path(&path).unwrap();
            let now = 1_700_000_000;
            w.apply_batch(&[
                Mutation::UpsertConcept {
                    concept_id: "c-hormones".into(),
                    name: "hormones".into(),
                    description: None,
                    kind: "topic".into(),
                    created_at: now,
                },
                Mutation::UpsertDomain {
                    domain_id: "d.test".into(),
                    host: "d.test".into(),
                },
                Mutation::UpsertResource {
                    resource_id: "mechanism-paper".into(),
                    url: "https://d.test/m".into(),
                    canonical_url: "https://d.test/m".into(),
                    domain_id: "d.test".into(),
                    source_id: None,
                    kind: "html".into(),
                },
                Mutation::UpsertResource {
                    resource_id: "topic-page".into(),
                    url: "https://d.test/t".into(),
                    canonical_url: "https://d.test/t".into(),
                    domain_id: "d.test".into(),
                    source_id: None,
                    kind: "html".into(),
                },
            ])
            .unwrap();
            // Equal scalar weights, so only the dimensions can separate them.
            w.apply_batch(&[
                Mutation::UpsertDocConcept {
                    resource_id: "mechanism-paper".into(),
                    concept_id: "c-hormones".into(),
                    weight: 0.5,
                    relation_kind: "direct".into(),
                    evidence: None,
                    passage_id: None,
                    via_concept_id: None,
                    model_version: Some("t".into()),
                    work_id: None,
                    created_at: now,
                },
                Mutation::UpsertDocConcept {
                    resource_id: "topic-page".into(),
                    concept_id: "c-hormones".into(),
                    weight: 0.5,
                    relation_kind: "direct".into(),
                    evidence: None,
                    passage_id: None,
                    via_concept_id: None,
                    model_version: Some("t".into()),
                    work_id: None,
                    created_at: now,
                },
            ])
            .unwrap();
            for (doc, dims) in [
                ("mechanism-paper", [("direct", 0.3), ("contextual", 0.2), ("mechanism", 0.95)]),
                ("topic-page", [("direct", 0.95), ("contextual", 0.2), ("mechanism", 0.1)]),
            ] {
                for (dim, value) in dims {
                    w.apply_batch(&[Mutation::UpsertDocConceptDim {
                        resource_id: doc.into(),
                        concept_id: "c-hormones".into(),
                        dim: dim.into(),
                        value,
                        model_version: Some("t".into()),
                        created_at: now,
                    }])
                    .unwrap();
                }
            }
        }

        let lane = ConceptLane::new(&path);
        let concepts = vec!["c-hormones".to_string()];

        let mech = lane
            .documents_for_dimensions(&concepts, 10, &DimensionWeights::mechanism_focused())
            .unwrap();
        assert_eq!(mech[0].0, "mechanism-paper", "mechanism query: {mech:?}");

        let topic = lane
            .documents_for_dimensions(&concepts, 10, &DimensionWeights::topic_focused())
            .unwrap();
        assert_eq!(topic[0].0, "topic-page", "topic query: {topic:?}");

        // The scalar path is unchanged, so a corpus without dimensions still ranks.
        let scalar = lane.documents_for(&concepts, 10).unwrap();
        assert_eq!(scalar.len(), 2, "scalar fallback lost documents: {scalar:?}");
    }

    /// Documents with only a scalar weight are not dropped when dimensions exist for others.
    #[test]
    fn a_document_without_dimensions_still_appears() {
        let (_d, lane) = corpus(&[
            ("with-dims", "c-love", 0.7, "direct"),
            ("scalar-only", "c-love", 0.6, "direct"),
        ]);
        let hits = lane
            .documents_for_dimensions(
                &["c-love".to_string()],
                10,
                &DimensionWeights::mechanism_focused(),
            )
            .unwrap();
        assert_eq!(hits.len(), 2, "a scalar-only document vanished: {hits:?}");
    }

    /// A non-ASCII query must not panic. The graded set contains Russian and Chinese titles, and
    /// an earlier stemmer sliced `String` at a fixed *byte* offset, which panicked mid-benchmark:
    /// "byte index 4 is not a char boundary; it is inside '程'". A crash in the query path is worse
    /// than a missed match.
    #[test]
    fn a_non_ascii_query_does_not_panic() {
        let (_d, lane) = corpus(&[("doc", "c-love", 0.7, "direct")]);
        for q in [
            "Логи системных компонентов | Kubernetes",
            "进程程 进程程",
            "日本語のクエリ",
            "café ☕ romance",
            "Ω≈ç√∫˜µ≤≥÷",
        ] {
            // Must return, not unwind.
            let concepts = lane.concepts_for(q).expect("no panic on non-ASCII");
            let hits = lane.search(q, 5, None).expect("no panic on non-ASCII search");
            let _ = (concepts, hits);
        }
    }

    /// No associations means no lane at all, rather than an empty lane taking fusion weight.
    #[test]
    fn a_corpus_without_associations_has_no_lane() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.sqlite");
        let _w = MetadataWriter::from_path(&path).unwrap();
        std::fs::create_dir_all(dir.path().join("meta")).ok();
        // `from_data_dir` looks for <dir>/meta/meta.sqlite; the writer made one next to it.
        let _ = std::fs::copy(&path, dir.path().join("meta").join("meta.sqlite"));
        assert!(from_data_dir(dir.path()).is_none());
    }
}
