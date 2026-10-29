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

/// The lane's read side: the metadata database, opened once.
pub struct ConceptLane {
    db_path: PathBuf,
    /// Concepts to return per query term before scoring, bounded to keep one broad concept from
    /// pulling the whole corpus.
    per_concept_limit: usize,
}

impl ConceptLane {
    /// A lane over `db_path`. The path is stored rather than a connection because the lane must be
    /// `Send + Sync` and is used across threads; SQLite connections are opened per call.
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
            per_concept_limit: 2_000,
        }
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
            const STEM_LEN: usize = 4;
            let (sa, sb) = (stem(a), stem(b));
            if sa.len() < STEM_LEN || sb.len() < STEM_LEN {
                return sa == sb;
            }
            sa[..STEM_LEN] == sb[..STEM_LEN] && (sa.starts_with(&sb[..STEM_LEN]) || sb.starts_with(&sa[..STEM_LEN]))
        }

        let term_stems: Vec<String> = terms.iter().map(|t| stem(t)).collect();
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
            let _ = &term_stems;
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
        if concepts.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.connect().map_err(|e| search_core::AppError::Db(e.to_string()))?;
        let mut best: HashMap<String, f32> = HashMap::new();
        let mut per_doc: HashMap<String, usize> = HashMap::new();

        let mut stmt = conn
            .prepare(
                "SELECT resource_id, weight, relation_kind
                   FROM doc_concept
                  WHERE concept_id = ?1
                  ORDER BY weight DESC
                  LIMIT ?2",
            )
            .map_err(search_metadata::db_err)?;

        for concept in concepts {
            let rows = stmt
                .query_map(
                    rusqlite::params![concept, self.per_concept_limit as i64],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, f64>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    },
                )
                .map_err(search_metadata::db_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(search_metadata::db_err)?;

            for (doc, weight, kind) in rows {
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
                let contribution = weight as f32 * factor;
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

impl LaneSearcher for ConceptLane {
    fn search(
        &self,
        query: &str,
        limit: usize,
        // The concept matrix holds `resource_id`, and a resource belongs to exactly one source;
        // filtering here would need a join this lane does not do. Fusion re-applies the facet.
        _source_id: Option<&str>,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        let concepts = self
            .concepts_for(query)
            .map_err(|e| RetrievalError::index(e.to_string()))?;
        let docs = self
            .documents_for(&concepts, limit)
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
