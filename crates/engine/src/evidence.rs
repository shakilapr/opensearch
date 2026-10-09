//! §16 Evidence scoring. OWNER: engine-rank agent.
//!
//! Frozen contract: `docs/ENGINE_SPEC.md` §16 and `engine/engine-architecture.md` §16.

use crate::units::QueryUnit;
use std::collections::{BTreeMap, BTreeSet};

/// A claim is supported, contradicted, or unknown (§16) — never a probability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    Supported,
    Contradicted,
    Unknown,
}

/// Verifies whether a retrieved document supports a query unit. Implemented by the
/// layer above (which can see document text); the engine only calls it.
///
/// The engine's `Deps` carries only document *titles* (what the retrieval lanes return), so
/// content-level verification (§16) cannot be derived here. The caller injects this trait and
/// the coordinator consults it first, falling back to title matching when it is absent or
/// unhelpful. Implementations must be deterministic and side-effect free.
pub trait EvidenceVerifier: Send + Sync {
    /// Return true when `doc_id`'s content supports `unit_text`.
    fn supports(&self, unit_text: &str, doc_id: &str) -> bool;
}

/// Maximum number of normalised unit tokens considered by [`title_supports_unit`]; longer
/// units are truncated to their first `MAX_UNIT_TOKENS` tokens so matching stays bounded.
pub const MAX_UNIT_TOKENS: usize = 15;

/// Units with at most this many normalised tokens must match *every* token in the title;
/// longer units are allowed to fall short (see [`title_supports_unit`]).
pub const ALL_TOKENS_AT_OR_BELOW: usize = 4;

/// Lowercase `s`, map every non-alphanumeric character to a space, and split on whitespace,
/// yielding the normalised token list. Unicode-aware lowercasing; deterministic and bounded
/// by the input length.
pub fn normalize_tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Token-based, punctuation-normalised title match — the deterministic §16 fallback used when
/// no [`EvidenceVerifier`] is injected (or it reports support for no candidate).
///
/// Exact rule (documented as implemented):
/// * Both `unit_text` and `title` are normalised with [`normalize_tokens`]: lowercased, every
///   non-alphanumeric character becomes a space, then split on whitespace. So
///   `"retrieval augmented generation"` and `"Retrieval-Augmented Generation: A Survey"` both
///   normalise to `["retrieval", "augmented", "generation", ...]` and the token `generation`
///   in either matches the other — punctuation (the hyphen) is not significant.
/// * The unit's token list is capped at [`MAX_UNIT_TOKENS`] tokens (keeping the first ones);
///   a unit that normalises to no tokens (empty or punctuation/whitespace-only) supports
///   nothing.
/// * The title supports the unit when at least `required` of the unit's tokens are also tokens
///   of the normalised title, where `required` is **all** of the unit's tokens when it has
///   `<= ALL_TOKENS_AT_OR_BELOW` tokens, and otherwise `ceil(0.8 * token_count)` (i.e. at
///   least 80% of the tokens) so a long unit does not require a title to contain every word.
/// * Complexity is `O(unit_tokens * title_tokens)` — no regex, no recursion; the caller bounds
///   the number of titles.
pub fn title_supports_unit(unit_text: &str, title: &str) -> bool {
    let mut need = normalize_tokens(unit_text);
    if need.is_empty() {
        return false;
    }
    if need.len() > MAX_UNIT_TOKENS {
        need.truncate(MAX_UNIT_TOKENS);
    }
    let have = normalize_tokens(title);
    let matched = need.iter().filter(|t| have.iter().any(|h| h == *t)).count();
    let required = if need.len() <= ALL_TOKENS_AT_OR_BELOW {
        need.len()
    } else {
        // ceil(0.8 * n) == (4*n + 4) / 5 for integer n.
        (need.len() * 4 + 4) / 5
    };
    matched >= required
}

/// Evidence for one unit from one document version, with the exact span when known.
#[derive(Debug, Clone, PartialEq)]
pub struct EvidenceRecord {
    pub unit_id: String,
    pub doc_id: String,
    pub status: Support,
    pub span: Option<(usize, usize)>,
    pub doc_version: String,
}

/// Coverage plus the mandatory-unit verdict.
#[derive(Debug, Clone, PartialEq)]
pub struct Coverage {
    /// `sum(unit_weight * supported) / sum(unit_weight)` (§16).
    pub coverage: f32,
    /// True only when *every* mandatory unit passed individually.
    pub mandatory_ok: bool,
    /// Mandatory units with no supporting evidence (drives the reported gap).
    pub unsupported: Vec<String>,
    /// Units with at least one `Contradicted` record, in `units` order. A unit may be
    /// supported *and* contradicted; §16 requires presenting "disagreements when trustworthy
    /// evidence conflicts", so this stays observable even though support wins the coverage
    /// vote.
    pub contradicted_units: Vec<String>,
}

/// Compute coverage (§16). High average coverage never overrides an unsatisfied mandatory
/// predicate — `mandatory_ok` is an independent, hard gate.
///
/// Semantics:
/// * A unit is *supported* when any record for it has `status == Supported`; duplicates and
///   re-statements of the same record never double-count (sets, not sums).
/// * A unit that has both `Supported` and `Contradicted` records is counted as supported and
///   additionally listed in `contradicted_units`.
/// * A `Contradicted`-only (or `Unknown`-only) unit is not supported and contributes no
///   weight to coverage.
/// * Weight-0 units add nothing to the fraction but a mandatory weight-0 unit still has to
///   be supported (`mandatory_ok` gates independently of weight).
/// * Empty `units` yields `coverage == 0.0` and `mandatory_ok == true` (no mandatory units).
pub fn evidence_coverage(units: &[QueryUnit], records: &[EvidenceRecord]) -> Coverage {
    let mut supported: BTreeSet<&str> = BTreeSet::new();
    let mut contradicted: BTreeSet<&str> = BTreeSet::new();
    for r in records {
        match r.status {
            Support::Supported => {
                supported.insert(r.unit_id.as_str());
            }
            Support::Contradicted => {
                contradicted.insert(r.unit_id.as_str());
            }
            Support::Unknown => {}
        }
    }

    let total: f32 = units.iter().map(|u| u.weight).sum();
    let got: f32 = units
        .iter()
        .filter(|u| supported.contains(u.id.as_str()))
        .map(|u| u.weight)
        .sum();

    let unsupported: Vec<String> = units
        .iter()
        .filter(|u| u.mandatory && !supported.contains(u.id.as_str()))
        .map(|u| u.id.clone())
        .collect();

    let contradicted_units: Vec<String> = units
        .iter()
        .filter(|u| contradicted.contains(u.id.as_str()))
        .map(|u| u.id.clone())
        .collect();

    Coverage {
        coverage: if total > 0.0 { got / total } else { 0.0 },
        mandatory_ok: unsupported.is_empty(),
        unsupported,
        contradicted_units,
    }
}

/// Number of independent origins, not raw record count (§16: syndicated copies are not
/// corroboration). `origin_of` maps a doc_id to its publishing origin.
pub fn independent_origins(records: &[EvidenceRecord], origin_of: &dyn Fn(&str) -> String) -> usize {
    records
        .iter()
        .map(|r| origin_of(&r.doc_id))
        .collect::<BTreeSet<_>>()
        .len()
}

/// Number of units proven by at least two **independent** origins (§16: "Track independent
/// origins rather than counting syndicated copies as corroboration"). Only `Supported`
/// records count; a unit whose supporting documents all trace to one origin is *not*
/// corroborated, and duplicate records from the same origin never inflate the count. Only
/// units present in `units` are considered.
pub fn corroborated_units(
    units: &[QueryUnit],
    records: &[EvidenceRecord],
    origin_of: &dyn Fn(&str) -> String,
) -> usize {
    let mut origins_by_unit: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for r in records {
        if r.status == Support::Supported {
            origins_by_unit
                .entry(r.unit_id.as_str())
                .or_default()
                .insert(origin_of(&r.doc_id));
        }
    }
    units
        .iter()
        .filter(|u| {
            origins_by_unit
                .get(u.id.as_str())
                .map_or(false, |origins| origins.len() >= 2)
        })
        .count()
}
