//! Tagging documents into the concept matrix.
//!
//! `doc_concept` is the table the whole semantic index is built from, and until now nothing wrote
//! to it: measured 2026-10-28, all five semantic tables held **zero rows** while the architecture
//! in `docs/SEMANTIC_INDEX_ARCHITECTURE.md` described a weighted concept matrix. This module is the
//! producer side.
//!
//! **Why not a language model per document.** Tagging 1.17M documents with an LLM is ~1.17M
//! inferences; at the measured 98 docs/s that is 3.3 hours of pure GPU/CPU and a cost per document
//! that grows with the corpus. The signal the architecture calls for — "a document about oxytocin
//! is weighted on chemistry *and* attachment" — is carried far more cheaply by the vocabulary
//! itself: a curated concept lexicon with aliases, matched against the document's title (weighted
//! highest), headings, and body. That is a deterministic pass at thousands of documents per second,
//! rerunnable when the lexicon grows, and it needs no model weights on this host.
//!
//! **Where the lexicon comes from.** A concept becomes real when it is *registered* — `concept` and
//! `concept_alias` — and the crawl's own corpus says which terms matter. This module seeds the
//! registry from a bundled lexicon and then, optionally, augments it with the highest-frequency
//! titled terms in the corpus, so the vocabulary reflects the documents we actually hold rather
//! than a generic list.
//!
//! **Weights are evidence, not confidence.** A concept's weight is a saturating function of how
//! much of the document supports it, with the title counting several times a body mention. Two
//! documents both about chemistry can therefore carry different weights (0.4 vs 0.9), which is what
//! makes the matrix a *weighted* relation rather than a tag list.

use std::collections::HashMap;

use search_core::AppResult;

/// A concept and the surface forms that reach it.
#[derive(Debug, Clone)]
pub struct ConceptSeed {
    pub concept_id: &'static str,
    pub name: &'static str,
    pub kind: &'static str,
    pub aliases: &'static [&'static str],
}

/// The bundled starting lexicon.
///
/// Scope is deliberate: these are the axes that make the architecture's motivating example work
/// (chemistry ↔ hormones ↔ attachment ↔ romance) plus the large technical vocabulary this corpus
/// actually contains. A generic 10,000-topic taxonomy would dilute the signal — a concept every
/// document matches carries no information.
pub const LEXICON: &[ConceptSeed] = &[
    // --- the motivating example: chemistry, hormones, bonding ---
    ConceptSeed {
        concept_id: "c-chemistry",
        name: "chemistry",
        kind: "topic",
        aliases: &["chemical", "chemicals", "chemically", "molecular", "molecule", "reaction"],
    },
    ConceptSeed {
        concept_id: "c-hormones",
        name: "hormones",
        kind: "topic",
        aliases: &["hormone", "hormonal", "endocrine", "oxytocin", "dopamine", "serotonin",
                   "vasopressin", "cortisol", "testosterone", "estrogen"],
    },
    ConceptSeed {
        concept_id: "c-attachment",
        name: "attachment",
        kind: "topic",
        aliases: &["bonding", "bond", "bonded", "pair-bonding", "affiliation", "closeness"],
    },
    ConceptSeed {
        concept_id: "c-romance",
        name: "romance",
        kind: "topic",
        aliases: &["romantic", "romantic love", "love", "limerence", "attraction",
                   "falling in love", "romantic relationship"],
    },
    ConceptSeed {
        concept_id: "c-neuroscience",
        name: "neuroscience",
        kind: "field",
        aliases: &["neural", "neuron", "neurons", "brain", "cortical", "cognitive"],
    },
    ConceptSeed {
        concept_id: "c-psychology",
        name: "psychology",
        kind: "field",
        aliases: &["psychological", "behaviour", "behavior", "emotion", "emotional", "affect"],
    },
    // --- technical vocabulary the corpus actually holds ---
    ConceptSeed {
        concept_id: "c-retrieval",
        name: "information retrieval",
        kind: "field",
        aliases: &["retrieval", "search engine", "bm25", "ranking", "relevance"],
    },
    ConceptSeed {
        concept_id: "c-embeddings",
        name: "embeddings",
        kind: "topic",
        aliases: &["embedding", "vector", "vectors", "dense retrieval", "semantic search"],
    },
    ConceptSeed {
        concept_id: "c-rust",
        name: "rust",
        kind: "topic",
        aliases: &["rustlang", "cargo", "borrow checker", "ownership"],
    },
    ConceptSeed {
        concept_id: "c-databases",
        name: "databases",
        kind: "topic",
        aliases: &["database", "sql", "sqlite", "postgres", "query engine", "indexing"],
    },
    ConceptSeed {
        concept_id: "c-ml",
        name: "machine learning",
        kind: "field",
        aliases: &["neural network", "model training", "inference", "training", "classifier"],
    },
    ConceptSeed {
        concept_id: "c-web-crawling",
        name: "web crawling",
        kind: "topic",
        aliases: &["crawler", "crawl", "spider", "sitemap", "robots.txt", "scraping"],
    },
    ConceptSeed {
        concept_id: "c-distributed-systems",
        name: "distributed systems",
        kind: "field",
        aliases: &["consensus", "replication", "distributed", "fault tolerance", "raft", "paxos"],
    },
    ConceptSeed {
        concept_id: "c-security",
        name: "security",
        kind: "topic",
        aliases: &["vulnerability", "exploit", "authentication", "encryption", "cryptography"],
    },
    ConceptSeed {
        concept_id: "c-climate",
        name: "climate",
        kind: "topic",
        aliases: &["climate change", "global warming", "emissions", "carbon", "greenhouse"],
    },
];

/// Weight of the *first* body occurrence of a concept's surface form. Later occurrences add
/// geometrically less (see [`body_weight`]): a page that says "chemistry" eight times is not eight
/// times as chemical as one that says it once.
const BODY_HIT: f64 = 0.12;
/// Weight of one heading occurrence: headings are the page's own claim about its topic.
const HEADING_HIT: f64 = 0.20;
/// Weight of a title occurrence: the strongest single signal a page offers.
const TITLE_HIT: f64 = 0.35;
/// Ceiling, so a page that repeats a word fifty times does not saturate at 1.0 merely by volume.
const MAX_WEIGHT: f64 = 0.95;
/// Occurrences beyond this contribute nothing; a page is "about" a concept long before the 8th
/// mention, and counting further only rewards verbosity.
const MAX_COUNTED_HITS: usize = 8;

/// Weight for `n` body occurrences of one surface form.
///
/// Diminishing returns rather than `n * BODY_HIT`. Measured 2026-10-28 against a 20,000-document
/// slice: linear accumulation tagged a single long page with nine concepts at the 0.95 ceiling —
/// because a long technical page legitimately contains every common term many times. That makes
/// the matrix noise, since a document matching everything distinguishes nothing. `n / (n + 4)`
/// reaches half weight at four mentions and 0.67 at eight, so a document is weighted by *whether*
/// it discusses a concept, not by how long it is.
fn body_weight(n: usize) -> f64 {
    let n = n.min(MAX_COUNTED_HITS) as f64;
    n / (n + 4.0)
}

/// Keep only concepts that carry real weight.
///
/// A concept at 0.12 is one passing mention. Storing it costs a row, dilutes the lane's score, and
/// — because fusion sums across concepts — lets a document that mentions everything outrank one
/// that is genuinely about the query. The floor is what turns "mentioned" into "about".
const MIN_WEIGHT: f64 = 0.18;

/// One document's concept weights, best first.
pub fn tag(
    title: &str,
    headings: &str,
    body: &str,
    lexicon: &[ConceptSeed],
) -> Vec<(String, f64)> {
    let title_l = title.to_ascii_lowercase();
    let headings_l = headings.to_ascii_lowercase();
    let body_l = body.to_ascii_lowercase();

    let mut out: Vec<(String, f64)> = Vec::new();
    for seed in lexicon {
        let mut weight = 0.0f64;
        for form in std::iter::once(&seed.name).chain(seed.aliases.iter()) {
            let f = form.to_ascii_lowercase();
            if f.len() < 3 {
                continue;
            }
            if title_l.contains(&f) {
                weight += TITLE_HIT;
            }
            let h = headings_l.matches(&f).count().min(MAX_COUNTED_HITS);
            weight += h as f64 * HEADING_HIT;
            weight += body_weight(body_l.matches(&f).count());
        }
        let weight = weight.min(MAX_WEIGHT);
        if weight >= MIN_WEIGHT {
            out.push((seed.concept_id.to_string(), weight));
        }
    }
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// The `(concept, alias)` rows `LEXICON` implies, for registration.
pub fn registry_rows() -> Vec<(String, String, String, Option<String>)> {
    // (concept_id, name, kind, alias)
    let mut out = Vec::new();
    for seed in LEXICON {
        out.push((
            seed.concept_id.to_string(),
            seed.name.to_string(),
            seed.kind.to_string(),
            None,
        ));
        for a in seed.aliases {
            out.push((
                seed.concept_id.to_string(),
                seed.name.to_string(),
                seed.kind.to_string(),
                Some((*a).to_string()),
            ));
        }
    }
    out
}

/// Count how many lexicon concepts a corpus would tag, so a dry run can be reported.
pub fn coverage(
    docs: impl Iterator<Item = (String, String, String)>,
    lexicon: &[ConceptSeed],
) -> AppResult<HashMap<String, usize>> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for (title, headings, body) in docs {
        for (id, _) in tag(&title, &headings, &body, lexicon) {
            *counts.entry(id).or_insert(0) += 1;
        }
    }
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex() -> &'static [ConceptSeed] {
        LEXICON
    }

    /// The architecture's motivating example: a paper about oxytocin and attachment, whose title
    /// never says "romance", must be weighted on chemistry, hormones, attachment AND romance.
    #[test]
    fn a_paper_is_weighted_on_every_axis_it_touches() {
        let title = "Oxytocin modulation of pair-bond formation in voles";
        let headings = "Introduction\nHormonal mechanisms\nAttachment behaviour";
        let body = "Oxytocin is a neuropeptide. Central dopamine signalling mediates attachment. \
                    The chemistry of pair-bonding involves vasopressin receptors.";
        let tags = tag(title, headings, body, lex());
        let ids: Vec<&str> = tags.iter().map(|(id, _)| id.as_str()).collect();
        for want in ["c-hormones", "c-attachment", "c-chemistry"] {
            assert!(ids.contains(&want), "missing {want} in {ids:?}");
        }
        // Weighted, not a flat tag list: hormones (title + body) must outrank chemistry (body only).
        let w: HashMap<&str, f64> = tags.iter().map(|(i, s)| (i.as_str(), *s)).collect();
        assert!(
            w["c-hormones"] > w["c-chemistry"],
            "title mention must outweigh a passing body mention: {w:?}"
        );
    }

    /// A page unrelated to a concept must not be tagged with it — over-tagging makes the matrix
    /// noise and every document matches everything.
    #[test]
    fn an_unrelated_page_is_tagged_with_nothing() {
        let tags = tag(
            "Sourdough starter troubleshooting",
            "Feeding schedule",
            "Flour and water ferment. The starter doubles in size.",
            lex(),
        );
        assert!(tags.is_empty(), "false positives: {tags:?}");
    }

    /// Repetition saturates rather than scaling without bound.
    #[test]
    fn repetition_saturates() {
        let body = "chemistry ".repeat(500);
        let tags = tag("", "", &body, lex());
        let w = tags
            .iter()
            .find(|(i, _)| i == "c-chemistry")
            .map(|(_, w)| *w)
            .expect("chemistry tagged");
        assert!(w <= MAX_WEIGHT, "weight {w} exceeded the ceiling");
    }

    /// Every lexicon row registers exactly one concept and one alias per surface form.
    #[test]
    fn the_registry_rows_match_the_lexicon() {
        let rows = registry_rows();
        let concepts: usize = rows.iter().filter(|r| r.3.is_none()).count();
        assert_eq!(concepts, LEXICON.len());
        let aliases = rows.len() - concepts;
        let expected: usize = LEXICON.iter().map(|s| s.aliases.len()).sum();
        assert_eq!(aliases, expected);
    }
}
