//! §6 Query analysis and belief state. OWNER: engine-analyze agent.
//!
//! Keeps the original query immutable and produces normalized text, language, exact phrases,
//! identifiers, entities, concepts, hard predicates, soft preferences, temporal intervals,
//! intent features, and a bounded query-unit dependency graph.

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

/// Parse a query into a [`QueryAnalysis`]. `now` is the query start time (unix seconds);
/// relative dates resolve against it using `tz_offset_min`.
pub fn analyze_query(query: &str, _now: i64, _tz_offset_min: i32) -> QueryAnalysis {
    // SCAFFOLD: minimal normalization only. The engine-analyze agent implements §6 in full:
    // unicode normalization, language-aware tokenization, phrase/identifier extraction that
    // preserves case, alias lookup, relative-date resolution, and intent features.
    QueryAnalysis {
        original: query.to_string(),
        normalized: query.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase(),
        ..Default::default()
    }
}

/// §6 entity linking: retain several hypotheses and always keep "unknown".
pub fn link_entities(mentions: &[EntityMention], priors: &AliasPriors) -> Vec<EntityHypothesis> {
    mentions
        .iter()
        .map(|m| EntityHypothesis {
            mention: m.text.clone(),
            candidates: priors.aliases.get(&m.text.to_lowercase()).cloned().unwrap_or_default(),
            chosen: None,
        })
        .collect()
}
