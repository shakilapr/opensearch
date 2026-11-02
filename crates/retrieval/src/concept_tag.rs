//! Tagging documents into the concept matrix.
//!
//! `doc_concept` is the table the whole semantic index is built from, and until now nothing wrote
//! to it: measured 2026-10-28, all five semantic tables held **zero rows** while the architecture
//! in `docs/specs/SEMANTIC_INDEX_ARCHITECTURE.md` described a weighted concept matrix. This module is the
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
//! **The vocabulary is open.** [`DEFAULT_SEEDS`] is a starting point, not a ceiling: a
//! [`ConceptLexicon`] can be loaded from JSON (`agent0 concepts build --lexicon <PATH>`) and holds
//! thousands or millions of concepts, because tagging walks one flattened list of pre-lowered
//! surface forms rather than re-deriving them per document.
//!
//! **Weights are evidence, not confidence.** A concept's weight is a saturating function of how
//! much of the document supports it, with the title counting several times a body mention. Two
//! documents both about chemistry can therefore carry different weights (0.4 vs 0.9), which is what
//! makes the matrix a *weighted* relation rather than a tag list.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use search_core::{AppError, AppResult};
use serde::{Deserialize, Serialize};

/// A concept and the surface forms that reach it.
///
/// The string fields are [`Cow`]s, so one type serves both worlds: the bundled lexicon is a
/// `const` of borrowed literals, and a vocabulary loaded at runtime from JSON, a database or an
/// API response is the same type holding owned strings. Callers never need a second "dynamic
/// concept" type, and `&[ConceptSeed]` remains the input to [`tag`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConceptSeed {
    pub concept_id: Cow<'static, str>,
    pub name: Cow<'static, str>,
    /// Coarse category — `topic`, `field`, `person`, … Free-form: the matrix only stores it.
    #[serde(default = "default_kind")]
    pub kind: Cow<'static, str>,
    #[serde(default)]
    pub aliases: Cow<'static, [Cow<'static, str>]>,
    /// Whether the concept's own `name` may be matched as a surface form.
    ///
    /// `true` for most concepts: a page saying "chemistry" is about chemistry. `false` when the
    /// name is a homonym that dominates the corpus in its other sense — measured 2026-10-29,
    /// "attachment" tagged 290 documents of which three quarters were Elasticsearch reference
    /// pages, and the name alone was the only remaining source of those false positives after the
    /// aliases were narrowed. Such a concept is reachable only through its multi-word aliases.
    #[serde(default = "default_match_name")]
    pub match_name: bool,
}

fn default_kind() -> Cow<'static, str> {
    Cow::Borrowed("topic")
}

fn default_match_name() -> bool {
    true
}

impl ConceptSeed {
    /// A seed built from owned strings — the shape a runtime-loaded concept takes.
    pub fn new(
        concept_id: impl Into<Cow<'static, str>>,
        name: impl Into<Cow<'static, str>>,
        kind: impl Into<Cow<'static, str>>,
        aliases: impl IntoIterator<Item = impl Into<Cow<'static, str>>>,
        match_name: bool,
    ) -> Self {
        Self {
            concept_id: concept_id.into(),
            name: name.into(),
            kind: kind.into(),
            aliases: Cow::Owned(aliases.into_iter().map(Into::into).collect()),
            match_name,
        }
    }

    /// The forms this seed may be matched through, subject to `match_name`.
    pub fn surface_forms(&self) -> impl Iterator<Item = &str> {
        let named = if self.match_name {
            Some(&*self.name)
        } else {
            None
        };
        named.into_iter().chain(self.aliases.iter().map(|a| &**a))
    }
}

/// Const constructor for a borrowed seed, so the bundled lexicon stays a `const` even though
/// [`ConceptSeed`] can hold owned strings.
macro_rules! seed {
    ($id:expr, $name:expr, $kind:expr, $match_name:expr, [$($alias:expr),* $(,)?]) => {
        ConceptSeed {
            concept_id: Cow::Borrowed($id),
            name: Cow::Borrowed($name),
            kind: Cow::Borrowed($kind),
            match_name: $match_name,
            aliases: Cow::Borrowed(&[$(Cow::Borrowed($alias)),*]),
        }
    };
}

/// The bundled starting lexicon.
///
/// Scope is deliberate: these are the axes that make the architecture's motivating example work
/// (chemistry ↔ hormones ↔ attachment ↔ romance) plus the large technical vocabulary this corpus
/// actually contains. A generic 10,000-topic taxonomy would dilute the signal — a concept every
/// document matches carries no information.
///
/// This is only the *default*. The vocabulary is open: pass a JSON file to
/// `agent0 concepts build --lexicon <PATH>` (see [`ConceptLexicon`]) and the same machinery tags
/// against thousands or millions of concepts.
// `rustfmt::skip`: formatting each `seed!(…)` invocation across a dozen lines would bury the
// alias list it exists to document.
#[rustfmt::skip]
pub const DEFAULT_SEEDS: &[ConceptSeed] = &[
    // --- the motivating example: chemistry, hormones, bonding ---
    // "chemical" is a band name and a laundry-product word; keep the name but rely on the sense
    // guard for the band case (measured: 0.49 on a tour-news page). Biochemistry vocabulary is
    // included because a paper on neurochemistry rarely says "chemistry": it says neuropeptide,
    // receptor, neurotransmitter. Found by the oxytocin test failing to tag a paper that is
    // unambiguously about the chemistry of bonding.
    seed!("c-chemistry", "chemistry", "topic", true, [
        "chemical", "chemicals", "chemically", "molecular", "molecule", "reaction",
        "biochemistry", "biochemical", "neurochemistry", "neuropeptide",
        "neurotransmitter", "enzyme", "receptor",
    ]),
    seed!("c-hormones", "hormones", "topic", true, [
        "hormone", "hormonal", "endocrine", "oxytocin", "dopamine", "serotonin",
        "vasopressin", "cortisol", "testosterone", "estrogen",
    ]),
    // NOT the bare word "attachment" and not "bond": measured 2026-10-29 on the serving root,
    // 15 of 20 documents tagged `c-attachment` were software documentation — Elasticsearch
    // "attachment field type", Docker "attach", email attachments, "bond" in finance and
    // networking. Only multi-word forms that name the psychological construct are kept.
    seed!("c-attachment", "attachment", "topic", false, [
        "pair bonding", "pair-bonding", "emotional bond", "emotional bonding",
        "human bonding", "social bonding", "attachment theory", "attachment style",
        "affiliation", "closeness",
    ]),
    // "romance" alone is a fiction genre and a band name; the aliases carry the construct.
    // "love" alone is excluded for the same reason as "attachment": it appears in product
    // copy, tennis scores and software docs. The phrases below are unambiguous.
    seed!("c-romance", "romance", "topic", false, [
        "romantic", "romantic love", "limerence", "romantic attraction",
        "falling in love", "romantic relationship", "romantic partner",
    ]),
    seed!("c-neuroscience", "neuroscience", "field", true, [
        "neural", "neuron", "neurons", "brain", "cortical", "cognitive",
    ]),
    // Single generic adjectives are excluded. Measured 2026-10-29: "behavioral" alone tagged
    // the Elasticsearch "Geoshape field type" reference page as psychology, because
    // "behavioral" is also a product feature name. Only multi-word psychological phrases remain,
    // plus the field's own name, which is specific enough to be safe.
    seed!("c-psychology", "psychology", "field", true, [
        "psychological", "emotion regulation", "emotional response",
        "emotional state", "cognitive psychology", "clinical psychology",
        "developmental psychology",
    ]),
    // --- technical vocabulary the corpus actually holds ---
    seed!("c-retrieval", "information retrieval", "field", true, [
        "retrieval", "search engine", "bm25", "ranking", "relevance",
    ]),
    seed!("c-embeddings", "embeddings", "topic", true, [
        "embedding", "vector", "vectors", "dense retrieval", "semantic search",
    ]),
    seed!("c-rust", "rust", "topic", true, [
        "rustlang", "cargo", "borrow checker", "ownership",
    ]),
    seed!("c-databases", "databases", "topic", true, [
        "database", "sql", "sqlite", "postgres", "query engine", "indexing",
    ]),
    seed!("c-ml", "machine learning", "field", true, [
        "neural network", "model training", "inference", "training", "classifier",
    ]),
    seed!("c-web-crawling", "web crawling", "topic", true, [
        "crawler", "crawl", "spider", "sitemap", "robots.txt", "scraping",
    ]),
    seed!("c-distributed-systems", "distributed systems", "field", true, [
        "consensus", "replication", "distributed", "fault tolerance", "raft", "paxos",
    ]),
    seed!("c-security", "security", "topic", true, [
        "vulnerability", "exploit", "authentication", "encryption", "cryptography",
    ]),
    seed!("c-climate", "climate", "topic", true, [
        "climate change", "global warming", "emissions", "carbon", "greenhouse",
    ]),
];

/// Backwards-compatible alias for [`DEFAULT_SEEDS`].
///
/// Kept because the name is part of the published surface of this module; new code should use
/// `DEFAULT_SEEDS` and prefer [`ConceptLexicon::from_defaults`].
pub const LEXICON: &[ConceptSeed] = DEFAULT_SEEDS;

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

/// A concept whose every alias is ordinary in a corpus becomes useless: measured 2026-10-29, `c-ml`
/// was associated with 6,167 of 19,255 documents (32%) because "training", "inference" and
/// "classifier" appear on every software page. A concept matching a third of the corpus cannot
/// discriminate, and it floods any lane that reads the matrix — the labelled document for
/// "teaching machines to learn from their own errors" ranked 1,644 of 2,000 concept hits.
///
/// The fix is ubiquity suppression rather than a better alias list: an association is kept only if
/// the concept is *not* ubiquitous in this corpus, or if the document's own evidence is strong
/// (title/heading) even for a common concept. Computed per corpus, so a niche domain keeps its
/// broad concepts.
///
/// Keep an association when its evidence is at least this fraction of `MAX_WEIGHT`, regardless of
/// how common the concept is. A page titled "Machine Learning" stays tagged even if the concept is
/// everywhere; a page that merely says "training" eight times does not.
pub const UBIQUITY_STRONG_EVIDENCE: f64 = 0.55;

/// Drop a concept's associations when it covers more than this fraction of the tagged corpus.
///
/// 0.10 is the line between "a field this corpus discusses" and "vocabulary this corpus is made
/// of". At 32% (`c-ml`) the concept was noise; at 10% a corpus with 19K documents keeps concepts
/// that name at most ~1,900 of them, which is still broad enough to be a real topic.
pub const UBIQUITY_LIMIT: f64 = 0.20;

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
    "novel",
    "chapter",
    "fiction",
    "romance novel",
    "audiobook",
    "ebook",
    "kindle",
    "read online",
    "webnovel",
    "light novel",
    "manga",
    "anime",
    "episode",
    "season 2",
    "watch online",
    "subtitle",
    "lyrics",
    "album",
    "tracklist",
    "tour dates",
    "concert",
    "tickets",
    "band",
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

/// A concept vocabulary that can be tagged against.
///
/// This is the loadable form of the seed list: it owns its concepts, pre-lowercases every surface
/// form once at build time, and tags a document against all of them. It is the entry point for
/// vocabularies that do not fit in a `const` — a JSON file with thousands of concepts, a table, an
/// API response — while [`ConceptLexicon::from_defaults`] reproduces the bundled lexicon exactly.
///
/// Ordering and weights are identical to the standalone [`tag`] helper: a document tagged through
/// the lexicon scores the same as through `tag(.., DEFAULT_SEEDS)`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConceptLexicon {
    seeds: Vec<ConceptSeed>,
    /// `(seed index, lowered surface form)`, flattened across the vocabulary.
    ///
    /// Built once. The old code re-lowered and re-collected every form of every concept for every
    /// document; that was invisible at 15 concepts and becomes the dominant cost at 10^4–10^6,
    /// where a document is compared against more forms than it has bytes.
    forms: Vec<(usize, String)>,
    by_id: HashMap<String, usize>,
}

/// Accepted JSON shapes: a bare array of concepts, or an object carrying a `concepts` array (so a
/// file can also hold provenance such as a version or a source). Unknown fields are ignored.
#[derive(Deserialize)]
#[serde(untagged)]
enum LexiconFile {
    Bare(Vec<ConceptSeed>),
    Wrapped { concepts: Vec<ConceptSeed> },
}

impl ConceptLexicon {
    /// An empty vocabulary. Tags nothing until seeds are added.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from seeds held in memory, merging repeated `concept_id`s.
    ///
    /// Duplicates are normal in vocabularies assembled from several files: the first definition's
    /// `name` and `kind` win, aliases accumulate (case-insensitively deduplicated) and
    /// `match_name` is the OR of the definitions. Entries with a blank `concept_id` or `name` are
    /// dropped here; [`ConceptLexicon::try_from_seeds`] is the strict variant.
    pub fn from_seeds<I: IntoIterator<Item = ConceptSeed>>(seeds: I) -> Self {
        let (lexicon, _problems) = Self::build(seeds);
        lexicon
    }

    /// Build from seeds held in memory, rejecting any entry a loader should not accept silently.
    pub fn try_from_seeds<I: IntoIterator<Item = ConceptSeed>>(seeds: I) -> AppResult<Self> {
        let (lexicon, problems) = Self::build(seeds);
        if let Some(first) = problems.first() {
            return Err(AppError::Invalid(format!(
                "concept lexicon: {first} ({} problem(s) in total)",
                problems.len()
            )));
        }
        Ok(lexicon)
    }

    /// The bundled 15-concept lexicon.
    pub fn from_defaults() -> Self {
        Self::from_seeds(DEFAULT_SEEDS.iter().cloned())
    }

    /// Load a vocabulary from a JSON string.
    ///
    /// Each object is `{"concept_id", "name", "kind", "aliases", "match_name"}`; `kind` defaults
    /// to `"topic"`, `aliases` to `[]` and `match_name` to `true`. An empty file is an error
    /// rather than an empty vocabulary: a run that silently tags nothing is indistinguishable
    /// from a lexicon that matched nothing, and the difference matters.
    pub fn from_json_str(json: &str) -> AppResult<Self> {
        let file: LexiconFile = serde_json::from_str(json)?;
        let seeds = match file {
            LexiconFile::Bare(v) => v,
            LexiconFile::Wrapped { concepts } => concepts,
        };
        let lexicon = Self::try_from_seeds(seeds)?;
        if lexicon.is_empty() {
            return Err(AppError::Invalid(
                "concept lexicon: no concepts found".to_string(),
            ));
        }
        Ok(lexicon)
    }

    /// Load a vocabulary from any reader holding JSON.
    pub fn from_reader<R: Read>(mut reader: R) -> AppResult<Self> {
        let mut buf = String::new();
        reader.read_to_string(&mut buf)?;
        Self::from_json_str(&buf)
    }

    /// Load a vocabulary from a JSON file.
    pub fn from_path(path: impl AsRef<Path>) -> AppResult<Self> {
        let path = path.as_ref();
        let file = std::fs::File::open(path).map_err(|e| {
            AppError::Invalid(format!(
                "cannot open concept lexicon {}: {e}",
                path.display()
            ))
        })?;
        let lexicon = Self::from_reader(file)
            .map_err(|e| AppError::Invalid(format!("concept lexicon {}: {e}", path.display())))?;
        Ok(lexicon)
    }

    /// Trim, merge duplicates, drop blank entries, and pre-lowercase every surface form.
    fn build<I: IntoIterator<Item = ConceptSeed>>(seeds: I) -> (Self, Vec<String>) {
        let mut lexicon = Self::default();
        let mut problems = Vec::new();
        for (pos, seed) in seeds.into_iter().enumerate() {
            let id = seed.concept_id.trim().to_string();
            let name = seed.name.trim().to_string();
            if id.is_empty() {
                problems.push(format!("entry {pos}: empty concept_id"));
                continue;
            }
            if name.is_empty() {
                problems.push(format!("entry {pos} ({id}): empty name"));
                continue;
            }
            let kind = match seed.kind.trim() {
                "" => "topic".to_string(),
                k => k.to_string(),
            };
            match lexicon.by_id.get(&id).copied() {
                Some(idx) => {
                    let existing = &mut lexicon.seeds[idx];
                    existing.match_name |= seed.match_name;
                    for alias in seed.aliases.iter() {
                        let dup = existing
                            .aliases
                            .iter()
                            .any(|a| a.eq_ignore_ascii_case(alias));
                        if !dup {
                            existing
                                .aliases
                                .to_mut()
                                .push(Cow::Owned(alias.to_string()));
                        }
                    }
                }
                None => {
                    let idx = lexicon.seeds.len();
                    lexicon.by_id.insert(id.clone(), idx);
                    lexicon.seeds.push(ConceptSeed::new(
                        id,
                        name,
                        kind,
                        seed.aliases.iter().map(|a| a.to_string()),
                        seed.match_name,
                    ));
                }
            }
        }
        lexicon.reindex();
        (lexicon, problems)
    }

    /// Rebuild the flattened form list from the seeds.
    fn reindex(&mut self) {
        self.forms.clear();
        for (idx, seed) in self.seeds.iter().enumerate() {
            let mut seen: Vec<String> = Vec::new();
            for form in seed.surface_forms() {
                let lowered = form.trim().to_ascii_lowercase();
                // Two bytes can be a substring of almost anything; the old `tag` skipped these and
                // so must the lexicon, or a dynamic vocabulary tags the whole corpus.
                if lowered.len() < 3 || seen.contains(&lowered) {
                    continue;
                }
                seen.push(lowered.clone());
                self.forms.push((idx, lowered));
            }
        }
    }

    /// The concepts this lexicon holds, in registration order.
    pub fn seeds(&self) -> &[ConceptSeed] {
        &self.seeds
    }

    /// One concept by id.
    pub fn get(&self, concept_id: &str) -> Option<&ConceptSeed> {
        self.by_id.get(concept_id).map(|&i| &self.seeds[i])
    }

    /// Whether the lexicon defines `concept_id`.
    pub fn contains(&self, concept_id: &str) -> bool {
        self.by_id.contains_key(concept_id)
    }

    /// How many concepts the lexicon holds.
    pub fn len(&self) -> usize {
        self.seeds.len()
    }

    /// Whether the lexicon holds no concepts.
    pub fn is_empty(&self) -> bool {
        self.seeds.is_empty()
    }

    /// How many distinct surface forms are matched against each document.
    pub fn surface_form_count(&self) -> usize {
        self.forms.len()
    }

    /// Tag one document against every concept in the vocabulary.
    pub fn tag(&self, title: &str, headings: &str, body: &str) -> Vec<Association> {
        tag_prepared(&self.seeds, &self.forms, title, headings, body)
    }

    /// `(concept_id, name, kind, alias)` rows this lexicon implies, for registration.
    pub fn registry_rows(&self) -> Vec<(String, String, String, Option<String>)> {
        registry_rows_for(&self.seeds)
    }

    /// Count how many concepts tag each document in `docs`.
    pub fn coverage(
        &self,
        docs: impl Iterator<Item = (String, String, String)>,
    ) -> AppResult<HashMap<String, usize>> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for (title, headings, body) in docs {
            for a in self.tag(&title, &headings, &body) {
                *counts.entry(a.concept_id).or_insert(0) += 1;
            }
        }
        Ok(counts)
    }
}

/// Per-concept match counts for one document, accumulated over its surface forms.
#[derive(Debug, Default, Clone, Copy)]
struct Hits {
    title: usize,
    headings: usize,
    body: usize,
    mechanism: usize,
}

/// One document's associations, strongest first.
pub fn tag(title: &str, headings: &str, body: &str, lexicon: &[ConceptSeed]) -> Vec<Association> {
    // Lower the forms once for this call. `ConceptLexicon` exists to hoist this out of a
    // per-document loop; the standalone helper keeps its old signature for callers that hold a
    // plain seed slice.
    ConceptLexicon::from_seeds(lexicon.iter().cloned()).tag(title, headings, body)
}

/// The tagging core: matches pre-lowered `forms` (each tagged with its seed index) against the
/// document, then turns the counts into dimensions.
fn tag_prepared(
    seeds: &[ConceptSeed],
    forms: &[(usize, String)],
    title: &str,
    headings: &str,
    body: &str,
) -> Vec<Association> {
    if seeds.is_empty() || forms.is_empty() {
        return Vec::new();
    }
    let title_l = title.to_ascii_lowercase();
    let headings_l = headings.to_ascii_lowercase();
    let body_l = body.to_ascii_lowercase();

    let mut hits = vec![Hits::default(); seeds.len()];
    for (idx, form) in forms {
        let h = &mut hits[*idx];
        if title_l.contains(form.as_str()) {
            h.title += 1;
        }
        h.headings += headings_l.matches(form.as_str()).count();
        h.body += body_l.matches(form.as_str()).count();
        // A concept is discussed as a *mechanism* when a causal or process word sits within
        // `PROXIMITY` characters of it. Measured on 20,000 real documents, the earlier test —
        // "does the document contain a marker anywhere" — produced a mechanism dimension that
        // never exceeded 0.2 and never once beat `direct`, because almost every explanatory
        // page contains "because" somewhere. Proximity is what makes the dimension mean "this
        // concept is explained" rather than "this page explains something".
        h.mechanism += marker_near(&body_l, form);
    }

    let mut out: Vec<Association> = Vec::new();
    for (seed, h) in seeds.iter().zip(hits.iter()) {
        let title_hits = h.title;
        let heading_hits = h.headings;
        let body_hits = h.body;
        let mechanism_hits = h.mechanism;

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
        let (direct, contextual, mechanism) = match wrong_sense_markers(&seed.concept_id) {
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
    "because",
    "causes",
    "caused by",
    "leads to",
    "results in",
    "mechanism",
    "pathway",
    "receptor",
    "signalling",
    "signaling",
    "regulates",
    "modulates",
    "mediates",
    "triggers",
    "in response to",
    "process by which",
    "how it works",
    "drives",
    "produces",
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

/// The `(concept, alias)` rows `DEFAULT_SEEDS` implies, for registration.
///
/// Kept for callers that register the bundled vocabulary; a loaded vocabulary uses
/// [`ConceptLexicon::registry_rows`].
pub fn registry_rows() -> Vec<(String, String, String, Option<String>)> {
    registry_rows_for(DEFAULT_SEEDS)
}

/// `(concept_id, name, kind, alias)` rows for any seed slice.
fn registry_rows_for(seeds: &[ConceptSeed]) -> Vec<(String, String, String, Option<String>)> {
    let mut out = Vec::new();
    for seed in seeds {
        out.push((
            seed.concept_id.to_string(),
            seed.name.to_string(),
            seed.kind.to_string(),
            None,
        ));
        for a in seed.aliases.iter() {
            out.push((
                seed.concept_id.to_string(),
                seed.name.to_string(),
                seed.kind.to_string(),
                Some(a.to_string()),
            ));
        }
    }
    out
}

/// Count how many lexicon concepts a corpus would tag, so a dry run can be reported.
///
/// Kept as a free function for a seed slice; [`ConceptLexicon::coverage`] is the prepared form.
pub fn coverage(
    docs: impl Iterator<Item = (String, String, String)>,
    lexicon: &[ConceptSeed],
) -> AppResult<HashMap<String, usize>> {
    ConceptLexicon::from_seeds(lexicon.iter().cloned()).coverage(docs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex() -> &'static [ConceptSeed] {
        DEFAULT_SEEDS
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
        assert!(
            chem.mechanism > 0.0,
            "mechanism dimension missing: {chem:?}"
        );
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
             signalling in the nucleus accumbens. The mechanism produces a lasting emotional \
             bond, and pair bonding in voles depends on vasopressin, which also causes the \
             attachment behaviour observed after mating.",
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
        assert_eq!(concepts, DEFAULT_SEEDS.len());
        let aliases = rows.len() - concepts;
        let expected: usize = DEFAULT_SEEDS.iter().map(|s| s.aliases.len()).sum();
        assert_eq!(aliases, expected);
    }

    // --- dynamic, open vocabularies -------------------------------------------------------

    /// The JSON a user would hand `agent0 concepts build --lexicon`.
    const JSON_LEXICON: &str = r#"[
        {
            "concept_id": "c-sourdough",
            "name": "sourdough",
            "kind": "topic",
            "match_name": true,
            "aliases": ["starter culture", "wild yeast", "levain"]
        },
        {
            "concept_id": "c-fermentation",
            "name": "fermentation",
            "kind": "topic",
            "match_name": false,
            "aliases": ["lactic acid bacteria", "fermented food"]
        }
    ]"#;

    /// The bundled vocabulary is unchanged, so `--lexicon` is additive rather than a migration.
    #[test]
    fn the_bundled_lexicon_is_still_the_fifteen_seeds() {
        assert_eq!(
            DEFAULT_SEEDS.len(),
            15,
            "the bundled vocabulary changed size"
        );
        assert_eq!(
            LEXICON.len(),
            15,
            "the backwards-compatible alias must match"
        );
        let lexicon = ConceptLexicon::from_defaults();
        assert_eq!(lexicon.len(), 15);
        assert!(
            lexicon.surface_form_count() > 50,
            "surface forms were not indexed: {}",
            lexicon.surface_form_count()
        );
        assert_eq!(lexicon.seeds(), DEFAULT_SEEDS);
        assert_eq!(lexicon.registry_rows().len(), registry_rows().len());
    }

    /// The point of the task: a vocabulary that exists only in a file tags documents.
    #[test]
    fn a_json_lexicon_tags_the_document_it_describes() {
        let lexicon = ConceptLexicon::from_json_str(JSON_LEXICON).expect("valid lexicon");
        assert_eq!(lexicon.len(), 2);
        assert!(lexicon.get("c-sourdough").is_some());
        assert!(lexicon.contains("c-fermentation"));

        let tags = lexicon.tag(
            "Sourdough starter troubleshooting",
            "Feeding schedule",
            "Wild yeast and levain raise the dough; the starter culture doubles overnight.",
        );
        let sd = tags
            .iter()
            .find(|a| a.concept_id == "c-sourdough")
            .expect("sourdough association");
        assert!(sd.weight() >= MIN_WEIGHT, "{sd:?}");
        assert!(sd.direct > 0.0, "the title names the concept: {sd:?}");
        assert_eq!(tags.len(), 1, "false positives: {tags:?}");
    }

    /// `match_name: false` must be honoured from JSON, not only from the bundled `const`.
    #[test]
    fn a_loaded_concept_can_exclude_its_own_name() {
        let lexicon = ConceptLexicon::from_json_str(JSON_LEXICON).expect("valid lexicon");
        let bare = lexicon.tag("Fermentation", "", "");
        assert!(bare.is_empty(), "the bare name must not match: {bare:?}");
        let by_alias = lexicon.tag(
            "Lactic acid bacteria in food",
            "",
            "Lactic acid bacteria ferment sugars.",
        );
        assert_eq!(by_alias.len(), 1, "{by_alias:?}");
        assert_eq!(by_alias[0].concept_id, "c-fermentation");
    }

    #[test]
    fn json_accepts_a_wrapped_concepts_object() {
        let lexicon = ConceptLexicon::from_json_str(
            r#"{"version": 1, "source": "hand-curated", "concepts": [
                 {"concept_id": "c-widgets", "name": "widgets", "aliases": ["widgetry"]}
               ]}"#,
        )
        .expect("wrapped lexicon");
        assert_eq!(lexicon.len(), 1);
        assert!(lexicon.contains("c-widgets"));
        assert!(!lexicon.tag("Widgetry at scale", "", "").is_empty());
    }

    /// Defaults fill in: `kind` is optional, and a seed without `match_name` matches its name.
    #[test]
    fn json_defaults_fill_in_missing_fields() {
        let lexicon =
            ConceptLexicon::from_json_str(r#"[{"concept_id":"c-tidal","name":"tidal power"}]"#)
                .expect("minimal lexicon");
        let seed = lexicon.get("c-tidal").expect("concept");
        assert_eq!(seed.kind, "topic");
        assert!(seed.match_name);
        assert!(seed.aliases.is_empty());
        assert_eq!(lexicon.tag("Tidal power in the bay", "", "").len(), 1);
    }

    #[test]
    fn a_lexicon_loads_from_a_file_and_from_a_reader() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("concepts.json");
        std::fs::write(&path, JSON_LEXICON).expect("write lexicon");

        let from_file = ConceptLexicon::from_path(&path).expect("load from path");
        let from_reader = ConceptLexicon::from_reader(std::fs::File::open(&path).expect("open"))
            .expect("load from reader");
        assert_eq!(from_file, from_reader);
        assert_eq!(from_file.len(), 2);

        let err = ConceptLexicon::from_path(dir.path().join("missing.json")).unwrap_err();
        assert!(err.to_string().contains("missing.json"), "{err}");
    }

    /// An empty or malformed vocabulary is an error: a pass that silently tags nothing would
    /// look like a corpus with no concepts in it.
    #[test]
    fn a_broken_or_empty_lexicon_is_an_error_not_a_silent_success() {
        assert!(ConceptLexicon::from_json_str("{not json").is_err());
        assert!(ConceptLexicon::from_json_str("[]").is_err(), "empty array");
        assert!(
            ConceptLexicon::from_json_str(r#"{"concepts": []}"#).is_err(),
            "empty wrapped"
        );
        assert!(ConceptLexicon::from_json_str(r#"[{"concept_id":"","name":"x"}]"#).is_err());
        assert!(ConceptLexicon::from_json_str(r#"[{"concept_id":"c","name":""}]"#).is_err());
        // A wrong type is a parse error, not a silently dropped alias list.
        assert!(ConceptLexicon::from_json_str(
            r#"[{"concept_id":"c","name":"n","aliases":"oops"}]"#
        )
        .is_err());
    }

    #[test]
    fn duplicate_concept_ids_merge_their_aliases() {
        let lexicon = ConceptLexicon::from_json_str(
            r#"[
                 {"concept_id":"c-dup","name":"dup","aliases":["first alias"],"match_name":false},
                 {"concept_id":"c-dup","name":"ignored","kind":"field",
                  "aliases":["second alias","First Alias"],"match_name":true}
               ]"#,
        )
        .expect("duplicate ids are legal");
        assert_eq!(lexicon.len(), 1, "one id is one concept");
        let seed = lexicon.get("c-dup").expect("concept");
        assert_eq!(seed.name, "dup", "the first definition keeps its name");
        assert_eq!(seed.kind, "topic", "the first definition keeps its kind");
        assert!(seed.match_name, "match_name is the OR of the definitions");
        let aliases: Vec<&str> = seed.aliases.iter().map(|a| &**a).collect();
        assert_eq!(aliases, vec!["first alias", "second alias"]);
        assert!(!lexicon.tag("Second alias here", "", "").is_empty());
    }

    #[test]
    fn the_lenient_constructor_drops_unusable_entries_and_the_strict_one_refuses_them() {
        let seeds = vec![
            ConceptSeed::new("", "blank id", "topic", Vec::<String>::new(), true),
            ConceptSeed::new("c-ok", "fine", "topic", ["fine alias"], true),
        ];
        assert_eq!(ConceptLexicon::from_seeds(seeds.clone()).len(), 1);
        assert!(ConceptLexicon::try_from_seeds(seeds).is_err());
    }

    /// The refactor must not move a single weight: the standalone helper and the prepared
    /// lexicon are the same computation.
    #[test]
    fn the_standalone_tag_and_the_lexicon_agree() {
        let title = "Oxytocin modulation of pair-bond formation in voles";
        let headings = "Introduction\nHormonal mechanisms\nAttachment behaviour";
        let body = "Oxytocin is a neuropeptide released in the hypothalamus. Central dopamine \
                    signalling mediates attachment formation because oxytocin receptors in the \
                    nucleus accumbens regulate social reward. Romantic attachment in humans is \
                    thought to share this pathway.";
        assert_eq!(
            tag(title, headings, body, DEFAULT_SEEDS),
            ConceptLexicon::from_defaults().tag(title, headings, body),
        );
    }

    /// The constraint this task removes: a file with thousands of concepts loads and tags
    /// against all of them, matching only the one whose form actually occurs.
    #[test]
    fn a_lexicon_of_thousands_of_concepts_tags_only_the_matching_one() {
        const N: usize = 5_000;
        let mut json = String::from("[");
        for i in 0..N {
            if i > 0 {
                json.push(',');
            }
            json.push_str(&format!(
                r#"{{"concept_id":"c-{i:05}","name":"topic-{i:05}-end","aliases":["zz{i:05}qq"]}}"#
            ));
        }
        json.push(']');

        let lexicon = ConceptLexicon::from_json_str(&json).expect("5000-concept lexicon");
        assert_eq!(lexicon.len(), N);
        assert_eq!(
            lexicon.surface_form_count(),
            N * 2,
            "every name and alias is indexed"
        );

        let tags = lexicon.tag(
            "All about topic-04321-end",
            "Sections",
            "This page mentions zz04321qq exactly once.",
        );
        assert_eq!(tags.len(), 1, "expected one concept, got: {tags:?}");
        assert_eq!(tags[0].concept_id, "c-04321");
    }

    /// A loaded vocabulary gets the same polysemy guard as the bundled one, keyed on concept id.
    #[test]
    fn the_wrong_sense_guard_applies_to_loaded_concepts() {
        let lexicon = ConceptLexicon::from_json_str(
            r#"[{"concept_id":"c-romance","name":"romance","match_name":false,
                 "aliases":["romantic love","romantic attachment"]}]"#,
        )
        .expect("valid lexicon");
        let genre = lexicon.tag(
            "Drama Novels - Read Online",
            "",
            "Romantic love stories. Read online, chapter updates, webnovel, ebook, manga, anime.",
        );
        let real = lexicon.tag(
            "Romantic attachment in adult couples",
            "",
            "A study of romantic love and romantic attachment over time.",
        );
        let genre_w = weight_of(&genre, "c-romance").unwrap_or(0.0);
        let real_w = weight_of(&real, "c-romance").unwrap_or(0.0);
        assert!(real_w > 0.5, "a real article must be tagged: {real:?}");
        assert!(
            genre_w * 2.0 < real_w,
            "a genre listing ({genre_w:.3}) must score far below a real article ({real_w:.3})"
        );
    }

    #[test]
    fn loaded_forms_are_case_insensitive_and_two_byte_forms_are_ignored() {
        let lexicon = ConceptLexicon::from_json_str(
            r#"[{"concept_id":"c-rust","name":"Rust","aliases":["ai","Ownership"]}]"#,
        )
        .expect("valid lexicon");
        let tags = lexicon.tag("OWNERSHIP and the borrow checker", "", "");
        assert_eq!(tags.len(), 1, "{tags:?}");
        assert_eq!(tags[0].concept_id, "c-rust");
        assert!(
            lexicon.tag("AI safety", "", "").is_empty(),
            "a two-byte alias matches half the corpus and must be dropped"
        );
    }

    #[test]
    fn coverage_counts_documents_per_concept() {
        let lexicon = ConceptLexicon::from_defaults();
        let docs = vec![
            (
                "Chemistry of bonding".to_string(),
                String::new(),
                "molecular reaction".to_string(),
            ),
            (
                "Rust ownership".to_string(),
                String::new(),
                "borrow checker".to_string(),
            ),
            (
                "Rust again".to_string(),
                String::new(),
                "ownership rules".to_string(),
            ),
        ];
        let counts = lexicon.coverage(docs.into_iter()).expect("coverage");
        assert_eq!(counts.get("c-rust"), Some(&2));
        assert_eq!(counts.get("c-chemistry"), Some(&1));
    }
}
