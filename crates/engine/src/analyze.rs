//! §6 Query analysis and belief state. OWNER: engine-analyze agent.
//!
//! Keeps the original query immutable and produces normalized text, language, exact phrases,
//! identifiers, entities, concepts, hard predicates, soft preferences, temporal intervals,
//! intent features, and a bounded query-unit dependency graph.
//!
//! Determinism: no network, no clock reads (`now: i64` is the query start time), no
//! `unwrap`/`expect`/`panic!`/`todo!` on request paths, and every loop is statically bounded.

use crate::units::QueryUnit;

/// A typed analysis of one query. `original` is verbatim and is never rewritten.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QueryAnalysis {
    pub original: String,
    pub normalized: String,
    pub language: Option<String>,
    pub phrases: Vec<String>,
    pub identifiers: Vec<String>,
    pub terms: Vec<String>,
    pub entities: Vec<EntityMention>,
    pub concepts: Vec<String>,
    pub hard_predicates: Vec<Predicate>,
    pub soft_preferences: Vec<Preference>,
    pub temporal: Option<TimeInterval>,
    pub intent: Intent,
    pub units: Vec<QueryUnit>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EntityMention {
    pub text: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Predicate {
    pub kind: PredicateKind,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredicateKind {
    ExactPhrase,
    Identifier,
    Domain,
    Language,
    DateAfter,
    DateBefore,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Preference {
    pub text: String,
    pub weight: f32,
}

/// UTC seconds, half-open `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeInterval {
    pub start: i64,
    pub end: i64,
}

/// Routing-classifier features (§6). These are features, not rigid rules.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Intent {
    pub direct: f32,
    pub semantic: f32,
    pub ambiguous: f32,
    pub multihop: f32,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct EntityHypothesis {
    pub mention: String,
    pub candidates: Vec<(String, f32)>,
    pub chosen: Option<String>,
}

/// Alias priors for entity linking. Deterministic; built by the caller.
#[derive(Debug, Clone, Default)]
pub struct AliasPriors {
    pub aliases: std::collections::BTreeMap<String, Vec<(String, f32)>>,
}

impl AliasPriors {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Add one prior. Priors for the same mention accumulate, highest first.
    pub fn insert(&mut self, mention: &str, entity: &str, prior: f32) {
        let e = self.aliases.entry(mention.to_lowercase()).or_default();
        e.push((entity.to_string(), prior));
    }
}

const DAY: i64 = 86_400;

/// Function words excluded from the "content" front of `terms`.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "of", "to", "in", "on", "for", "and", "or", "is", "are", "was", "were",
    "be", "been", "being", "with", "at", "by", "from", "as", "that", "this", "these", "those",
    "it", "its", "i", "you", "we", "they", "he", "she", "his", "her", "not", "no", "do", "does",
    "did", "but", "if", "then", "so", "than", "into", "over", "about", "after", "before", "who",
    "which", "what", "when", "where", "why", "how", "will", "would", "should", "could", "can",
    "may", "might", "must", "have", "has", "had", "my", "your", "our", "their", "them",
];

/// Connective words that join independent requirements or subordinate clauses.
const RELATION_WORDS: &[&str] = &[
    "and", "plus", "also", "then", "from", "to", "because", "who", "which", "after", "or",
    "between", "than",
];

/// Parse a query into a [`QueryAnalysis`]. `now` is the query start time (unix seconds);
/// relative dates resolve against it using `tz_offset_min`.
pub fn analyze_query(query: &str, now: i64, tz_offset_min: i32) -> QueryAnalysis {
    let normalized = normalize(query);
    let phrases = extract_phrases(query);
    let identifiers = extract_identifiers(query);

    let (hard_predicates, soft_preferences) = extract_operators(query);

    // Phrases and identifiers are additionally hard predicates (§6).
    let mut hard = hard_predicates;
    for p in &phrases {
        hard.push(Predicate {
            kind: PredicateKind::ExactPhrase,
            value: p.clone(),
        });
    }
    for id in &identifiers {
        hard.push(Predicate {
            kind: PredicateKind::Identifier,
            value: id.clone(),
        });
    }

    let (content_words, terms) = tokenize_terms(&normalized);
    let entities = extract_entities(query, &identifiers);
    let concepts = content_words.clone();
    let language = guess_language(&normalized);
    let temporal = resolve_temporal(&normalized, now, tz_offset_min);

    // intent features — see the reasoning in `intent_features` (§6 "features, not rigid rules").
    let clauses = crate::units::clause_segments(query).len();
    let intent = intent_features(&normalized, content_words.len(), &phrases, &identifiers, clauses);

    let mut analysis = QueryAnalysis {
        original: query.to_string(),
        normalized,
        language,
        phrases,
        identifiers,
        terms,
        entities,
        concepts,
        hard_predicates: hard,
        soft_preferences,
        temporal,
        intent,
        units: Vec::new(),
    };
    analysis.units = crate::units::decompose(&analysis);
    analysis
}

// ---------------------------------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------------------------------

/// A deliberately small, deterministic subset of Unicode NFKC (§6): fullwidth forms, the
/// ideographic space, common ligatures, superscript digits, NBSP and the ellipsis are folded.
/// std exposes no NFKC tables and we take no dependencies, so this covers the compatibility
/// cases that matter for search text and is documented as an approximation.
fn nfkc_fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\u{00A0}' | '\u{3000}' => out.push(' '),
            '\u{FB00}' => out.push_str("ff"),
            '\u{FB01}' => out.push_str("fi"),
            '\u{FB02}' => out.push_str("fl"),
            '\u{FB03}' => out.push_str("ffi"),
            '\u{FB04}' => out.push_str("ffl"),
            '\u{00B9}' => out.push('1'),
            '\u{00B2}' => out.push('2'),
            '\u{00B3}' => out.push('3'),
            '\u{2026}' => out.push_str("..."),
            c if ('\u{FF01}'..='\u{FF5E}').contains(&c) => {
                if let Some(ac) = char::from_u32(c as u32 - 0xFEE0) {
                    out.push(ac);
                }
            }
            c => out.push(c),
        }
    }
    out
}

fn normalize(query: &str) -> String {
    let folded = nfkc_fold(query).to_lowercase();
    folded.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------------------------
// Lexical extraction
// ---------------------------------------------------------------------------------------------

/// Byte spans of maximal alphanumeric runs in `s`.
fn word_spans(s: &str) -> Vec<(usize, usize)> {
    let mut v = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in s.char_indices() {
        if c.is_alphanumeric() {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(st) = start.take() {
            v.push((st, i));
        }
    }
    if let Some(st) = start {
        v.push((st, s.len()));
    }
    v
}

/// Extract quoted spans verbatim — case and punctuation preserved (§6). Both `"..."` and
/// single-quoted `'...'` spans are recognised; a single quote embedded inside a word (an
/// apostrophe) is not treated as a quote delimiter.
fn extract_phrases(query: &str) -> Vec<String> {
    let chars: Vec<(usize, char)> = query.char_indices().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let (bs, c) = chars[i];
        if c == '"' || c == '\'' {
            // For an apostrophe (single quote) only open a span at a word boundary.
            if c == '\'' {
                let prev_alnum = i > 0 && chars[i - 1].1.is_alphanumeric();
                let next_alnum = i + 1 < chars.len() && chars[i + 1].1.is_alphanumeric();
                if prev_alnum && next_alnum {
                    i += 1;
                    continue;
                }
            }
            let mut j = i + 1;
            let mut closed = None;
            while j < chars.len() {
                if chars[j].1 == c {
                    closed = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(cj) = closed {
                let start = bs + c.len_utf8();
                let end = chars[cj].0;
                if end > start {
                    if let Some(span) = query.get(start..end) {
                        let t = span.trim();
                        if !t.is_empty() {
                            out.push(t.to_string());
                        }
                    }
                }
                i = cj + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn is_version(w: &str) -> bool {
    let body = w.strip_prefix('v').or_else(|| w.strip_prefix('V')).unwrap_or(w);
    if body.is_empty() {
        return false;
    }
    let mut digits = 0usize;
    let mut dots = 0usize;
    for c in body.chars() {
        if c.is_ascii_digit() {
            digits += 1;
        } else if c == '.' {
            dots += 1;
        } else {
            return false;
        }
    }
    digits >= 2 && dots >= 1
}

fn is_error_code(w: &str) -> bool {
    let bytes = w.as_bytes();
    let mut idx = 0usize;
    let mut letters = 0usize;
    // Bound the scan: any word longer than 32 bytes cannot be an error code.
    let hard_cap = bytes.len().min(32);
    while idx < hard_cap && bytes[idx].is_ascii_uppercase() {
        idx += 1;
        letters += 1;
    }
    if letters == 0 {
        return false;
    }
    let mut digits = 0usize;
    while idx < hard_cap && bytes[idx].is_ascii_digit() {
        idx += 1;
        digits += 1;
    }
    if digits < 2 {
        return false;
    }
    while idx < hard_cap && bytes[idx].is_ascii_uppercase() {
        idx += 1;
    }
    idx == bytes.len()
}

fn is_symbol(w: &str) -> bool {
    if is_operator_token(w) {
        return false;
    }
    let has_alnum = w.chars().any(|c| c.is_alphanumeric());
    let has_sym = w.contains('#')
        || w.contains('@')
        || w.contains('+')
        || w.contains('/')
        || w.contains("::");
    has_alnum && has_sym
}

fn is_case_significant(w: &str) -> bool {
    let chars: Vec<char> = w.chars().collect();
    if chars.len() < 2 {
        return false;
    }
    let upper = chars.iter().filter(|c| c.is_uppercase()).count();
    if upper == 0 {
        return false;
    }
    // All-caps acronyms (GPU, API) or mixed-case symbols (iPhone, GraphQL).
    if chars.iter().all(|c| !c.is_lowercase()) {
        return true;
    }
    chars.iter().skip(1).any(|c| c.is_uppercase())
}

/// Whitespace-delimited tokens with surrounding pure punctuation trimmed, but keeping
/// identifier-internal characters (`.`, `:`, `+`, `#`, `@`, `/`, `-`, `_`). Used for identifier
/// detection so `V1.2.3`, `site:example.com` and `C++` stay intact.
fn raw_tokens(s: &str) -> Vec<String> {
    s.split_whitespace()
        .map(|t| {
            t.trim_matches(|c: char| {
                matches!(c, '"' | '\'' | '(' | ')' | ',' | ';' | '!' | '?' | '[' | ']')
            })
        })
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

fn extract_identifiers(query: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in raw_tokens(query) {
        if is_version(&w) || is_error_code(&w) || is_symbol(&w) || is_case_significant(&w) {
            if !out.iter().any(|x| x == &w) {
                out.push(w);
            }
        }
    }
    out
}

fn extract_entities(query: &str, identifiers: &[String]) -> Vec<EntityMention> {
    let spans = word_spans(query);
    let mut out = Vec::new();
    for (idx, (s, e)) in spans.iter().enumerate() {
        // Skip the first word (sentence-initial capitalisation is not evidence of an entity).
        if idx == 0 {
            continue;
        }
        let w = match query.get(*s..*e) {
            Some(w) => w,
            None => continue,
        };
        if w.len() < 2 {
            continue;
        }
        let first_upper = w.chars().next().map(|c| c.is_uppercase()).unwrap_or(false);
        if !first_upper {
            continue;
        }
        if identifiers.iter().any(|x| x == w) {
            continue;
        }
        out.push(EntityMention {
            text: w.to_string(),
            start: *s,
            end: *e,
        });
    }
    out
}

fn tokenize_terms(normalized: &str) -> (Vec<String>, Vec<String>) {
    let mut all: Vec<String> = Vec::new();
    for t in normalized.split(|c: char| !c.is_alphanumeric()) {
        if t.is_empty() {
            continue;
        }
        if !all.iter().any(|x| x == t) {
            all.push(t.to_string());
        }
    }
    let mut content: Vec<String> = Vec::new();
    let mut rest: Vec<String> = Vec::new();
    for t in all {
        if STOPWORDS.contains(&t.as_str()) {
            rest.push(t);
        } else {
            content.push(t);
        }
    }
    let mut terms = content.clone();
    terms.extend(rest);
    (content, terms)
}

// ---------------------------------------------------------------------------------------------
// Operators -> predicates and preferences
// ---------------------------------------------------------------------------------------------

fn is_operator_token(w: &str) -> bool {
    if let Some(colon) = w.find(':') {
        let p = w[..colon].to_ascii_lowercase();
        return matches!(
            p.as_str(),
            "site" | "domain" | "lang" | "after" | "before" | "prefer" | "should" | "would-like"
        );
    }
    false
}

fn extract_operators(original: &str) -> (Vec<Predicate>, Vec<Preference>) {
    let mut preds: Vec<Predicate> = Vec::new();
    let mut prefs: Vec<Preference> = Vec::new();
    for tok in original.split_whitespace() {
        let colon = match tok.find(':') {
            Some(c) => c,
            None => continue,
        };
        let pre = &tok[..colon];
        let val = &tok[colon + 1..];
        let val = val.trim_end_matches(|c: char| matches!(c, ',' | ';' | ')' | '('));
        if val.is_empty() {
            continue;
        }
        match pre.to_ascii_lowercase().as_str() {
            "site" | "domain" => preds.push(Predicate {
                kind: PredicateKind::Domain,
                value: val.to_string(),
            }),
            "lang" => preds.push(Predicate {
                kind: PredicateKind::Language,
                value: val.to_string(),
            }),
            "after" => preds.push(Predicate {
                kind: PredicateKind::DateAfter,
                value: val.to_string(),
            }),
            "before" => preds.push(Predicate {
                kind: PredicateKind::DateBefore,
                value: val.to_string(),
            }),
            "prefer" | "should" | "would-like" => prefs.push(Preference {
                text: val.to_string(),
                weight: 0.6,
            }),
            _ => {}
        }
    }
    // Bare first-person / stylistic hints become weak preferences.
    let low = original.to_lowercase();
    for hint in ["first-person", "in my opinion", "ideally"] {
        if low.contains(hint) {
            prefs.push(Preference {
                text: hint.to_string(),
                weight: 0.4,
            });
        }
    }
    (preds, prefs)
}

// ---------------------------------------------------------------------------------------------
// Language
// ---------------------------------------------------------------------------------------------

fn guess_language(normalized: &str) -> Option<String> {
    if normalized
        .chars()
        .any(|c| ('\u{0400}'..='\u{04FF}').contains(&c))
    {
        return Some("ru".to_string());
    }
    if normalized
        .chars()
        .any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c))
    {
        return Some("zh".to_string());
    }
    let en_stop = ["the", "of", "and", "is", "to", "in", "for", "with", "that", "a"];
    for w in normalized.split_whitespace() {
        let ww = w.trim_matches(|c: char| !c.is_alphanumeric());
        if en_stop.contains(&ww) {
            return Some("en".to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Temporal resolution
// ---------------------------------------------------------------------------------------------

/// Days since the UNIX epoch (1970-01-01) for a civil date. Howard Hinnant's algorithm.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Civil date (year, month, day) from days since the epoch.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// UTC seconds of the most recent local midnight at or before `now`.
fn local_midnight_utc(now: i64, tz_offset_min: i32) -> i64 {
    let off = tz_offset_min as i64 * 60;
    let local = now + off;
    let day = local.div_euclid(DAY);
    day * DAY - off
}

fn has_word(hay: &str, w: &str) -> bool {
    hay.split_whitespace()
        .any(|t| t.trim_matches(|c: char| !c.is_alphanumeric()) == w)
}

fn resolve_temporal(normalized: &str, now: i64, tz_offset_min: i32) -> Option<TimeInterval> {
    let midnight = local_midnight_utc(now, tz_offset_min);

    // "last N days" / "past N days" — a rolling window ending at `now`.
    let toks: Vec<&str> = normalized.split_whitespace().collect();
    for i in 0..toks.len() {
        let lead = toks[i].trim_matches(|c: char| !c.is_alphanumeric());
        if (lead == "last" || lead == "past") && i + 1 < toks.len() {
            if let Ok(n) = toks[i + 1].parse::<i64>() {
                if (1..=3650).contains(&n) {
                    return Some(TimeInterval {
                        start: now - n * DAY,
                        end: now,
                    });
                }
            }
        }
    }

    if normalized.contains("last week") {
        return Some(TimeInterval {
            start: midnight - 7 * DAY,
            end: midnight,
        });
    }
    if has_word(normalized, "today") {
        return Some(TimeInterval {
            start: midnight,
            end: midnight + DAY,
        });
    }
    if has_word(normalized, "yesterday") {
        return Some(TimeInterval {
            start: midnight - DAY,
            end: midnight,
        });
    }
    if normalized.contains("this year") {
        let local = now + tz_offset_min as i64 * 60;
        let (y, _, _) = civil_from_days(local.div_euclid(DAY));
        let off = tz_offset_min as i64 * 60;
        return Some(TimeInterval {
            start: days_from_civil(y, 1, 1) * DAY - off,
            end: days_from_civil(y + 1, 1, 1) * DAY - off,
        });
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Intent features
// ---------------------------------------------------------------------------------------------

/// §6: "a routing classifier estimates direct, semantic, ambiguous, and multi-hop query
/// classes. These are features, not rigid rules". The four values are cheap, monotone signals:
///
/// * `direct` — an exact anchor (a quoted phrase or an identifier) makes the query
///   lexically answerable; a single bare term with no anchor is not direct.
/// * `semantic` — question words and "why/how/because/explain" ask for reasoning, not matching.
/// * `ambiguous` — a single short term with no anchor is underspecified.
/// * `multihop` — connectives and multiple clauses imply composed requirements.
///
/// All outputs are clamped to [0, 1]; nothing here consults the network or the clock.
fn intent_features(
    normalized: &str,
    content_count: usize,
    phrases: &[String],
    identifiers: &[String],
    clauses: usize,
) -> Intent {
    let has_phrase = !phrases.is_empty();
    let has_id = !identifiers.is_empty();
    let single_unanchored = content_count <= 1 && !has_phrase && !has_id;

    let relations = RELATION_WORDS
        .iter()
        .filter(|w| has_word(normalized, w))
        .count();

    let qword = ["why", "how", "what", "when", "where", "which", "who"]
        .iter()
        .filter(|w| has_word(normalized, w))
        .count();
    let reasoning = ["why", "how", "because", "explain", "reason"]
        .iter()
        .any(|w| has_word(normalized, w));

    let direct = clamp01(
        0.15 + if has_phrase { 0.45 } else { 0.0 } + if has_id { 0.45 } else { 0.0 }
            - if single_unanchored { 0.2 } else { 0.0 },
    );
    let semantic = clamp01(
        0.1 + 0.25 * qword as f32
            + if content_count >= 6 { 0.15 } else { 0.0 }
            + if reasoning { 0.2 } else { 0.0 },
    );
    let ambiguous = clamp01(
        if single_unanchored { 0.7 } else { 0.0 }
            + if content_count <= 2 && !has_phrase && !has_id { 0.3 } else { 0.0 }
            + if has_word(normalized, "or") { 0.2 } else { 0.0 },
    );
    let multihop = clamp01(
        0.3 * relations as f32
            + if clauses >= 2 { 0.3 } else { 0.0 }
            + if clauses >= 3 { 0.2 } else { 0.0 },
    );

    Intent {
        direct,
        semantic,
        ambiguous,
        multihop,
    }
}

fn clamp01(x: f32) -> f32 {
    if x.is_nan() {
        0.0
    } else if x < 0.0 {
        0.0
    } else if x > 1.0 {
        1.0
    } else {
        x
    }
}

// ---------------------------------------------------------------------------------------------
// Entity linking
// ---------------------------------------------------------------------------------------------

/// §6 entity linking: retain several hypotheses in descending prior order and always keep an
/// "unknown/other" option. `chosen` is set only for a confident winner: top prior >= 0.5 AND
/// more than 0.15 above the runner-up.
pub fn link_entities(mentions: &[EntityMention], priors: &AliasPriors) -> Vec<EntityHypothesis> {
    const UNKNOWN: &str = "unknown/other";
    mentions
        .iter()
        .map(|m| {
            let mut cands: Vec<(String, f32)> = priors
                .aliases
                .get(&m.text.to_lowercase())
                .cloned()
                .unwrap_or_default();
            for c in cands.iter_mut() {
                if c.1.is_nan() {
                    c.1 = 0.0;
                }
            }
            cands.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            cands.push((UNKNOWN.to_string(), 0.0));

            let chosen = if cands.len() >= 2 {
                let top = cands[0].1;
                let runner = cands[1].1;
                if top >= 0.5 && (top - runner) > 0.15 {
                    Some(cands[0].0.clone())
                } else {
                    None
                }
            } else {
                None
            };

            EntityHypothesis {
                mention: m.text.clone(),
                candidates: cands,
                chosen,
            }
        })
        .collect()
}
