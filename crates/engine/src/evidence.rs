//! §16 Evidence scoring. OWNER: engine-rank agent.

use crate::units::QueryUnit;

/// A claim is supported, contradicted, or unknown (§16) — never a probability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    Supported,
    Contradicted,
    Unknown,
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
}

/// Compute coverage (§16). High average coverage never overrides an unsatisfied mandatory
/// predicate — `mandatory_ok` is an independent, hard gate.
pub fn evidence_coverage(units: &[QueryUnit], records: &[EvidenceRecord]) -> Coverage {
    let supported: std::collections::BTreeSet<&str> = records
        .iter()
        .filter(|r| r.status == Support::Supported)
        .map(|r| r.unit_id.as_str())
        .collect();

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

    Coverage {
        coverage: if total > 0.0 { got / total } else { 0.0 },
        mandatory_ok: unsupported.is_empty(),
        unsupported,
    }
}

/// Number of independent origins, not raw record count (§16: syndicated copies are not
/// corroboration). `origin_of` maps a doc_id to its publishing origin.
pub fn independent_origins(records: &[EvidenceRecord], origin_of: &dyn Fn(&str) -> String) -> usize {
    records
        .iter()
        .map(|r| origin_of(&r.doc_id))
        .collect::<std::collections::BTreeSet<_>>()
        .len()
}
