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
        // Biochemistry vocabulary is included because a paper on neurochemistry rarely says
        // "chemistry": it says neuropeptide, receptor, neurotransmitter. Found by the oxytocin
        // test failing to tag a paper that is unambiguously about the chemistry of bonding.
        aliases: &["chemical", "chemicals", "chemically", "molecular", "molecule", "reaction",
                   "biochemistry", "biochemical", "neurochemistry", "neuropeptide",
                   "neurotransmitter", "enzyme", "receptor"],
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

/// Markers that show a concept is being used in its *other* sense.
///
/// "romance" is a concept about attachment, and also a fiction genre and a band name. Measured
/// 2026-10-29 while validating the concept lane: a light-novel category page
/// (`novelcool.com/category/Drama.html`) scored **0.60 on `romance`** and ranked in the top five for
/// "chemical reactions that affect romance", as did My Chemical Romance concert news. The alias
/// list cannot fix this, because the word is correct in both senses; only context distinguishes
/// them.
///
/// A non-fiction corpus should therefore discount a concept when the page also carries markers of
/// the other sense. The discount is applied to `direct`, since a genre listing is not claiming this
/// concept topically, and the association is dropped entirely if nothing else supports it.
const FICTION_MARKERS: &[&str] = &[
    "novel", "chapter", "fiction", "romance novel", "audiobook", "ebook", "kindle", "read online",
    "webnovel", "light novel", "manga", "anime", "episode", "season 2", "watch online", "subtitle",
    "lyrics", "album", "tracklist", "tour dates", "concert", "tickets", "band",
];

/// Markers of a music/entertainment page, the other sense of "chemical".
///
/// Measured 2026-10-29: "My Chemical Romance announce 2026 tour" scored **0.49 on `chemistry`**
/// because the band's name contains the alias. The page is not about chemistry at all, and it was
/// the top lexical result for "chemical reactions that affect romance".
const MUSIC_MARKERS: &[&str] = &[
    "band", "tour", "album", "lyrics", "concert", "tickets", "spotify", "itunes", "playlist",
    "singer", "song", "track", "gig", "festival", "merch", "vinyl", "setlist",
];

/// Concepts whose correct sense is ambiguous in a general web corpus, and the markers that indicate
/// the wrong sense. Keyed by concept id.
fn wrong_sense_markers(concept_id: &str) -> Option<&'static [&'static str]> {
    match concept_id {
        // "romance" the attachment concept vs the fiction genre.
        "c-romance" => Some(FICTION_MARKERS),
        // "chemical" the science vs the band.
        "c-chemistry" => Some(MUSIC_MARKERS),
        _ => None,
    }
}

/// How much of `direct` survives when the page carries the concept's other sense.
///
/// Not zero: a genuine article about romantic attachment can quote a novel, and a page about a band
/// can still discuss chemistry. A modest discount removes genre listings from the top ranks without
/// making the concept unreachable.
const AMBIGUOUS_SENSE_FACTOR: f64 = 0.15;

/// One document's association with one concept, as v2 §6 requires: a vector of dimensions, not a
/// single number.
///
/// `weight` is the scalar retrieval uses (the strongest dimension, see [`Association::weight`]);
/// the dimensions are what let the same article rank differently for different queries without
/// being reindexed.
#[derive(Debug, Clone, PartialEq)]
pub struct Association {
    pub concept_id: String,
    /// `direct`: the document states it. r1 in v2 §6 — direct topical relevance.
    pub direct: f64,
    /// `contextual`: reached by co-occurrence rather than statement. r2 in v2 §6.
    pub contextual: f64,
    /// `mechanism`: the document discusses how/why, not only that. r3 in v2 §6.
    pub mechanism: f64,
}

impl Association {
    /// The single number retrieval scores with.
    ///
    /// The strongest dimension rather than the sum: dimensions describe *aspects* of one
    /// association, so adding them would let a document that mentions a concept's mechanism and
    /// context and topic outrank one that states it plainly, which is the opposite of what v2 §6
    /// says the oxytocin example should do.
    pub fn weight(&self) -> f64 {
        self.direct.max(self.contextual).max(self.mechanism)
    }

    /// One dimension by name, or 0.0 when the model did not emit it.
    pub fn dimension(&self, name: &str) -> f64 {
        match name {
            "direct" => self.direct,
            "contextual" => self.contextual,
            "mechanism" => self.mechanism,
            _ => 0.0,
        }
    }

    /// The dimensions to store, in the order v2 §6 lists them.
    pub fn dims(&self) -> [(&'static str, f64); 3] {
        [
            ("direct", self.direct),
            ("contextual", self.contextual),
            ("mechanism", self.mechanism),
        ]
    }
}

/// One document's associations, strongest first.
pub fn tag(
    title: &str,
    headings: &str,
    body: &str,
    lexicon: &[ConceptSeed],
) -> Vec<Association> {
    let title_l = title.to_ascii_lowercase();
    let headings_l = headings.to_ascii_lowercase();
    let body_l = body.to_ascii_lowercase();

    let mut out: Vec<Association> = Vec::new();
    for seed in lexicon {
        let mut title_hits = 0usize;
        let mut heading_hits = 0usize;
        let mut body_hits = 0usize;
        let mut mechanism_hits = 0usize;

        for form in std::iter::once(&seed.name).chain(seed.aliases.iter()) {
            let f = form.to_ascii_lowercase();
            if f.len() < 3 {
                continue;
            }
            if title_l.contains(&f) {
                title_hits += 1;
            }
            heading_hits += headings_l.matches(&f).count();
            body_hits += body_l.matches(&f).count();
            // A concept is discussed as a *mechanism* when a causal or process word sits within
            // `PROXIMITY` characters of it. Measured on 20,000 real documents, the earlier test —
            // "does the document contain a marker anywhere" — produced a mechanism dimension that
            // never exceeded 0.2 and never once beat `direct`, because almost every explanatory
            // page contains "because" somewhere. Proximity is what makes the dimension mean "this
            // concept is explained" rather than "this page explains something".
            mechanism_hits += marker_near(&body_l, &f);
        }

        if title_hits == 0 && heading_hits == 0 && body_hits == 0 {
            continue;
        }

        // The three dimensions must measure *different things*, or they cannot disagree and v2 §6
        // cannot be satisfied. Measured on 20,000 real documents, an earlier model built all
        // three from the same body counts and the result was structural: `mechanism` was a subset
        // of `direct`'s evidence, so it never once exceeded it and could not change a ranking.
        //
        // So each dimension now has its own evidence:
        //
        //   r1 direct      — the page's own claim to be about the concept: title, headings, and a
        //                    *bounded* contribution from body volume. A page that merely uses a
        //                    word a lot does not thereby claim the topic.
        //   r2 contextual  — the concept appears in the body without being claimed or explained.
        //                    Co-occurrence, nothing more.
        //   r3 mechanism   — the concept is *explained*: causal/process language within 240 bytes.
        //                    Independent of title and headings, so an untitled explanatory page can
        //                    out-rank its own topical mention on this dimension.

        // r1: what the page says about itself.
        let claim = title_hits.min(2) as f64 * TITLE_HIT
            + heading_hits.min(MAX_COUNTED_HITS) as f64 * HEADING_HIT;
        // Body volume contributes, but cannot alone make a page "directly about" a concept: the
        // cap is what leaves room for mechanism to exceed direct on an explanatory page.
        let direct = (claim + body_weight(body_hits) * DIRECT_BODY_FACTOR).min(MAX_WEIGHT);

        // r2: present but unclaimed. Scaled down so that any claim outranks mere presence.
        let contextual = if body_hits > 0 {
            (body_weight(body_hits) * CONTEXTUAL_FACTOR).min(MAX_WEIGHT)
        } else {
            0.0
        };

        // r3: explained. Its own evidence, not a function of `direct`.
        let mechanism = if mechanism_hits > 0 {
            (body_weight(mechanism_hits) * MECHANISM_FACTOR).min(MAX_WEIGHT)
        } else {
            0.0
        };

        // Wrong-sense discount. Applied to `direct` only, and only when the page actually carries
        // the markers — a page with no genre vocabulary is untouched.
        let (direct, contextual, mechanism) = match wrong_sense_markers(seed.concept_id) {
            Some(markers) => {
                let hits = markers.iter().filter(|m| body_l.contains(**m)).count();
                if hits >= 2 {
                    (
                        direct * AMBIGUOUS_SENSE_FACTOR,
                        contextual * AMBIGUOUS_SENSE_FACTOR,
                        mechanism * AMBIGUOUS_SENSE_FACTOR,
                    )
                } else {
                    (direct, contextual, mechanism)
                }
            }
            None => (direct, contextual, mechanism),
        };

        let assoc = Association {
            concept_id: seed.concept_id.to_string(),
            direct,
            contextual,
            mechanism,
        };
        // Keep an association only when it clears the floor on at least one dimension. Storing a
        // row of near-zero weights costs space and dilutes every lane that reads it.
        if assoc.weight() >= MIN_WEIGHT {
            out.push(assoc);
        }
    }
    out.sort_by(|a, b| {
        b.weight()
            .partial_cmp(&a.weight())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// How close a marker must be to a concept mention to count as explaining it, in bytes.
const PROXIMITY: usize = 240;

/// Number of concept occurrences (up to the counting cap) that have a mechanism marker nearby.
///
/// Byte-indexed rather than character-indexed: the search is on an ASCII-lowercased string, and a
/// UTF-8 boundary can only be missed in non-ASCII text, where a slightly wrong window costs a
/// little accuracy and never correctness of the slice (`get` returns `None` rather than panicking).
fn marker_near(body: &str, form: &str) -> usize {
    let mut hits = 0usize;
    let mut from = 0usize;
    while hits < MAX_COUNTED_HITS {
        let Some(rel) = body[from..].find(form) else {
            break;
        };
        let at = from + rel;
        let start = at.saturating_sub(PROXIMITY);
        let end = (at + form.len() + PROXIMITY).min(body.len());
        if let Some(window) = body.get(start..end) {
            if contains_any(window, MECHANISM_MARKERS) {
                hits += 1;
            }
        }
        from = at + form.len();
        if from >= body.len() {
            break;
        }
    }
    hits
}

/// Words that mark a passage as describing *how* something works rather than merely naming it.
///
/// Chosen for causal/process language, not for topic: these words appear in any explanatory text,
/// so they only shift a dimension when the concept is already present.
const MECHANISM_MARKERS: &[&str] = &[
    "because", "causes", "caused by", "leads to", "results in", "mechanism", "pathway",
    "receptor", "signalling", "signaling", "regulates", "modulates", "mediates", "triggers",
    "in response to", "process by which", "how it works", "drives", "produces",
];

/// Scale a body-derived weight into the contextual dimension, so contextual evidence never
/// outweighs the same evidence stated directly.
const CONTEXTUAL_FACTOR: f64 = 0.50;

/// How much body volume may contribute to the *direct* dimension.
///
/// Below 1.0 deliberately: body mentions are the mechanism dimension's evidence, and if they could
/// saturate `direct` on their own then `direct` would always dominate `mechanism` and the two
/// dimensions could never disagree. 0.35 leaves the title/headings as the dominant signal for
/// "this page is about X" while letting an explanatory page lead on "this page explains X".
const DIRECT_BODY_FACTOR: f64 = 0.70;

/// Scale a mechanism weight. Can reach `MAX_WEIGHT` because it is independent evidence: a page that
/// explains a concept thoroughly is strongly associated along this dimension regardless of whether
/// its title names the concept.
const MECHANISM_FACTOR: f64 = 1.0;

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
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
        for a in tag(&title, &headings, &body, lexicon) {
            *counts.entry(a.concept_id).or_insert(0) += 1;
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

    fn weight_of(tags: &[Association], id: &str) -> Option<f64> {
        tags.iter().find(|a| a.concept_id == id).map(|a| a.weight())
    }

    /// v2 §6's own example: "A research paper about oxytocin and pair bonding could have a strong
    /// direct association with neurochemistry and a weaker contextual association with romantic
    /// attachment." This asserts exactly that shape.
    #[test]
    fn the_oxytocin_paper_has_a_strong_direct_and_a_weaker_contextual_association() {
        let title = "Oxytocin modulation of pair-bond formation in voles";
        let headings = "Introduction\nHormonal mechanisms\nAttachment behaviour";
        let body = "Oxytocin is a neuropeptide released in the hypothalamus. Central dopamine \
                    signalling mediates attachment formation because oxytocin receptors in the \
                    nucleus accumbens regulate social reward. Romantic attachment in humans is \
                    thought to share this pathway.";
        let tags = tag(title, headings, body, lex());

        let chem = tags
            .iter()
            .find(|a| a.concept_id == "c-chemistry")
            .expect("chemistry association");
        let romance = tags
            .iter()
            .find(|a| a.concept_id == "c-romance")
            .expect("romance association");

        // The title says "oxytocin", not "chemistry", so chemistry's evidence is the body's
        // biochemistry vocabulary — it is a real association and it carries the mechanism
        // dimension, but it is not the page's stated subject.
        assert!(chem.mechanism > 0.0, "mechanism dimension missing: {chem:?}");
        assert!(
            chem.direct > 0.0,
            "chemistry should still be a direct association: {chem:?}"
        );
        // v2 §6's exact claim: strong on neurochemistry, *weaker* contextual on romantic
        // attachment. The paper mentions romantic attachment once, in passing.
        assert!(
            romance.direct < chem.direct,
            "romance must be a weaker association than chemistry: {romance:?} vs {chem:?}"
        );
        // And the subject of the paper — oxytocin, tagged through the hormones concept — must
        // outrank both, because it is named in the title and the headings.
        let hormones = tags
            .iter()
            .find(|a| a.concept_id == "c-hormones")
            .expect("hormones association");
        assert!(
            hormones.direct > chem.direct,
            "the title's own subject must dominate: {hormones:?} vs {chem:?}"
        );
    }

    /// The same article must be able to rank differently for two queries without reindexing
    /// (v2 §6). Both the mechanism and the direct dimension are stored, and they disagree.
    #[test]
    fn one_article_carries_disagreeing_dimensions() {
        let tags = tag(
            "Hormonal regulation of social behaviour",
            "Mechanisms",
            "Oxytocin receptors regulate pair-bonding because the pathway mediates reward.",
            lex(),
        );
        let hormones = tags
            .iter()
            .find(|a| a.concept_id == "c-hormones")
            .expect("hormones");
        // The title names the concept (direct) and the body explains it (mechanism): both high.
        assert!(hormones.direct > 0.0);
        assert!(hormones.dimension("mechanism") > 0.0);
        // The dimensions are separate values, not one number copied into three slots.
        assert_ne!(hormones.direct, hormones.dimension("mechanism"));
    }

    /// The mechanism dimension must be able to *win*, not merely be non-zero. A page whose whole
    /// point is explaining how a concept works has to outrank its own topical mention on the
    /// mechanism dimension, or the dimension cannot change any ranking and is decoration.
    #[test]
    fn an_explanatory_page_scores_high_on_mechanism() {
        let tags = tag(
            "Pair bonding",
            "",
            "Pair bonding is driven by oxytocin because the receptor pathway regulates reward \
             signalling in the nucleus accumbens; this mechanism produces attachment.",
            lex(),
        );
        let a = tags
            .iter()
            .find(|a| a.concept_id == "c-attachment")
            .expect("attachment");
        assert!(
            a.mechanism > a.contextual,
            "an explained concept must out-score plain co-occurrence: {a:?}"
        );
        assert!(a.mechanism > 0.4, "mechanism too weak to matter: {a:?}");
    }

    /// The two dimensions must be able to *cross*: a document that explains a concept without
    /// being titled about it must score higher on mechanism than on direct. Without this, no query
    /// interpretation can change a ranking and v2 §6's "does not need to be reindexed for every
    /// query interpretation" is vacuous.
    #[test]
    fn mechanism_can_exceed_direct_on_an_explanatory_page() {
        let tags = tag(
            "Study 4",
            "",
            "Pair bonding is driven by oxytocin because the receptor pathway regulates reward \
             signalling; this mechanism produces attachment between adults.",
            lex(),
        );
        let a = tags
            .iter()
            .find(|a| a.concept_id == "c-attachment")
            .expect("attachment");
        assert!(
            a.mechanism > a.direct,
            "an explanatory page with an unhelpful title must lead on mechanism: {a:?}"
        );
    }

    /// Polysemy: a fiction-genre listing must not score high on `romance`, while a page about
    /// romantic attachment must. Measured 2026-10-29: a light-novel category page scored 0.60 on
    /// `romance` and ranked top-five for a chemistry query.
    #[test]
    fn a_genre_listing_is_not_a_romance_association() {
        let genre = tag(
            "Drama Novels - Read Online",
            "Romance",
            "Read romance novel online. Latest chapter updates, webnovel listing, ebook downloads.",
            lex(),
        );
        let scholarly = tag(
            "Romantic attachment and pair bonding",
            "Introduction",
            "Romantic love involves attachment processes; research on romantic relationships \
             and pair bonding in adults.",
            lex(),
        );

        let genre_w = weight_of(&genre, "c-romance").unwrap_or(0.0);
        let real_w = weight_of(&scholarly, "c-romance").unwrap_or(0.0);
        assert!(
            real_w > genre_w * 2.0,
            "a genre listing ({genre_w:.3}) must score far below a real article ({real_w:.3})"
        );
    }

    /// My Chemical Romance: a band page mentioning the words must not dominate a chemistry query.
    #[test]
    fn a_band_page_is_not_a_chemistry_association() {
        let band = tag(
            "My Chemical Romance announce 2026 tour",
            "",
            "The band My Chemical Romance add tour dates; tickets on sale, album news, lyrics.",
            lex(),
        );
        let chem = weight_of(&band, "c-chemistry").unwrap_or(0.0);
        assert!(
            chem < 0.2,
            "a band page must not be a strong chemistry association: {chem:.3}"
        );
    }

    /// A page that merely mentions concepts without discussing them must not acquire the
    /// mechanism dimension — otherwise every document with the word "because" scores as mechanism.
    #[test]
    fn a_passing_mention_gets_no_mechanism_dimension() {
        let tags = tag(
            "Weekly link roundup",
            "Links",
            "Some notes on chemistry and rust and databases.",
            lex(),
        );
        for a in &tags {
            assert_eq!(
                a.mechanism, 0.0,
                "{} acquired a mechanism dimension from a list of names: {a:?}",
                a.concept_id
            );
        }
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
        let w = weight_of(&tags, "c-chemistry").expect("chemistry tagged");
        assert!(w <= MAX_WEIGHT, "weight {w} exceeded the ceiling");
    }

    /// The scalar `weight()` is the strongest dimension, not the sum of three correlated ones.
    #[test]
    fn the_scalar_weight_is_the_max_not_the_sum() {
        let a = Association {
            concept_id: "c".into(),
            direct: 0.6,
            contextual: 0.3,
            mechanism: 0.2,
        };
        assert!((a.weight() - 0.6).abs() < 1e-9, "{}", a.weight());
        assert_eq!(a.dims().len(), 3);
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
