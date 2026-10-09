//! §7 Constraint decomposition and evidence joins. OWNER: engine-analyze agent.

use crate::analyze::QueryAnalysis;
use crate::MAX_UNITS;

/// One independent requirement of the query. `depends_on` makes the dependency graph explicit.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryUnit {
    pub id: String,
    pub text: String,
    pub depends_on: Vec<String>,
    pub mandatory: bool,
    pub weight: f32,
    pub kind: UnitKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitKind {
    Identify,
    Reason,
    Destination,
    Quantity,
    Attribute,
    Relation,
    Other,
}

/// A verified shared key that two units must agree on to join (§7).
#[derive(Debug, Clone, PartialEq)]
pub struct JoinKey {
    pub unit_a: String,
    pub unit_b: String,
    pub key: String,
}

/// Words after which a new clause begins (§7 "multiple clauses joined by and/plus/also/then"
/// and explicit subordinate relations).
const BOUNDARY_WORDS: &[&str] = &["and", "plus", "also", "then", "because", "who", "which"];

/// Pronouns that refer back to an earlier clause's entity (§7 explicit dependencies).
const PRONOUNS: &[&str] = &[
    "they", "them", "their", "it", "its", "those", "these", "he", "she", "him", "her",
];

/// Words signalling a continuation/subordinate clause that depends on what precedes it.
const DEP_CUES: &[&str] = &[
    "of", "because", "after", "which", "who", "from", "to", "that", "with", "for", "in",
];

fn words(s: &str) -> Vec<(usize, usize)> {
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

fn word_at(s: &str, span: (usize, usize)) -> &str {
    s.get(span.0..span.1).unwrap_or("")
}

fn is_boundary(w: &str) -> bool {
    BOUNDARY_WORDS.contains(&w.to_ascii_lowercase().as_str())
}

fn has_token(low: &str, t: &str) -> bool {
    low.split_whitespace()
        .any(|x| x.trim_matches(|c: char| !c.is_alphanumeric()) == t)
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    hay.contains(needle)
}

/// Split the original query into clause segments at connective/subordinate boundaries. Each
/// segment keeps the original casing and punctuation (verbatim slices of `original`). A query
/// with no boundary words yields a single segment equal to the whole query.
pub(crate) fn clause_segments(original: &str) -> Vec<String> {
    let spans = words(original);
    if spans.is_empty() {
        return vec![original.to_string()];
    }
    let mut segs: Vec<String> = Vec::new();
    let mut last = 0usize; // index into spans: first word of the current segment
    for (i, span) in spans.iter().enumerate() {
        let w = word_at(original, *span);
        if is_boundary(w) {
            if last < i {
                let start = spans[last].0;
                let end = spans[i - 1].1;
                if let Some(seg) = original.get(start..end) {
                    let t = seg.trim();
                    if !t.is_empty() {
                        segs.push(t.to_string());
                    }
                }
            }
            last = i + 1;
        }
    }
    if last < spans.len() {
        let start = spans[last].0;
        let end = spans[spans.len() - 1].1;
        if let Some(seg) = original.get(start..end) {
            let t = seg.trim();
            if !t.is_empty() {
                segs.push(t.to_string());
            }
        }
    }
    if segs.is_empty() {
        segs.push(original.to_string());
    }
    segs
}

/// Classify one clause's wording into a [`UnitKind`] (§7).
fn classify_segment(text: &str) -> UnitKind {
    let low = text.to_ascii_lowercase();
    if contains_ci(&low, "because")
        || has_token(&low, "reason")
        || has_token(&low, "why")
        || contains_ci(&low, "due to")
        || has_token(&low, "cost")
    {
        return UnitKind::Reason;
    }
    if low.chars().any(|c| c.is_ascii_digit())
        || contains_ci(&low, "how many")
        || contains_ci(&low, "number of")
        || has_token(&low, "count")
        || has_token(&low, "bought")
        || has_token(&low, "purchase")
        || has_token(&low, "quantity")
    {
        return UnitKind::Quantity;
    }
    if has_token(&low, "to")
        || has_token(&low, "into")
        || has_token(&low, "toward")
        || has_token(&low, "destination")
    {
        return UnitKind::Destination;
    }
    if has_token(&low, "who")
        || has_token(&low, "which")
        || has_token(&low, "whom")
        || has_token(&low, "identify")
        || has_token(&low, "find")
    {
        return UnitKind::Identify;
    }
    if has_token(&low, "from")
        || has_token(&low, "between")
        || has_token(&low, "after")
        || contains_ci(&low, "relate")
        || contains_ci(&low, "relationship")
        || has_token(&low, "join")
        || has_token(&low, "moved")
        || has_token(&low, "migration")
    {
        return UnitKind::Relation;
    }
    if has_token(&low, "prefer")
        || has_token(&low, "should")
        || contains_ci(&low, "first-person")
        || has_token(&low, "ideally")
    {
        return UnitKind::Attribute;
    }
    UnitKind::Other
}

fn is_preference_clause(text: &str) -> bool {
    let low = text.to_ascii_lowercase();
    has_token(&low, "prefer")
        || has_token(&low, "should")
        || contains_ci(&low, "first-person")
        || contains_ci(&low, "in my opinion")
        || has_token(&low, "ideally")
}

/// Explicit dependency edges for unit `k` (0-based): a clause that refers back to an earlier
/// entity (a pronoun) depends on the subject unit U1; a subordinate continuation depends on
/// the immediately preceding unit (a unit that requires an earlier unit's entity lists it).
fn deps_for(k: usize, text: &str, ids: &[String]) -> Vec<String> {
    if k == 0 || ids.is_empty() {
        return Vec::new();
    }
    let low = text.to_ascii_lowercase();
    if PRONOUNS.iter().any(|p| has_token(&low, p)) {
        return vec![ids[0].clone()];
    }
    if DEP_CUES.iter().any(|p| has_token(&low, p)) {
        return vec![ids[k - 1].clone()];
    }
    Vec::new()
}

/// Decompose ONLY when relationships or several independent requirements justify it (§7).
/// A simple query yields exactly one unit; the result never exceeds [`MAX_UNITS`].
pub fn decompose(analysis: &QueryAnalysis) -> Vec<QueryUnit> {
    let segs = clause_segments(&analysis.original);

    // A simple query (no conjunction or subordinate relation) is a single unit holding the
    // whole query verbatim.
    if segs.len() <= 1 {
        return vec![QueryUnit {
            id: "U1".to_string(),
            text: analysis.original.clone(),
            depends_on: Vec::new(),
            mandatory: true,
            weight: 1.0,
            kind: UnitKind::Other,
        }];
    }

    let n = segs.len().min(MAX_UNITS);
    let mut units: Vec<QueryUnit> = Vec::with_capacity(n);
    for (k, text) in segs.iter().take(n).enumerate() {
        let id = format!("U{}", k + 1);
        let kind = if k == 0 {
            UnitKind::Identify
        } else {
            classify_segment(text)
        };
        let mandatory = !is_preference_clause(text);
        let weight = if mandatory { 1.0 } else { 0.5 };
        let ids: Vec<String> = units.iter().map(|u| u.id.clone()).collect();
        let depends_on = deps_for(k, text, &ids);
        units.push(QueryUnit {
            id,
            text: text.clone(),
            depends_on,
            mandatory,
            weight,
            kind,
        });
    }
    units
}

/// Filter declared join keys to those whose units actually exist (§7).
pub fn joins(units: &[QueryUnit], shared: &[JoinKey]) -> Vec<JoinKey> {
    shared
        .iter()
        .filter(|j| {
            units.iter().any(|u| u.id == j.unit_a) && units.iter().any(|u| u.id == j.unit_b)
        })
        .cloned()
        .collect()
}
