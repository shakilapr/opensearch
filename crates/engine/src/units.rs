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

/// Decompose ONLY when relationships or several independent requirements justify it (§7).
/// A simple query yields exactly one unit; the result never exceeds [`MAX_UNITS`].
pub fn decompose(analysis: &QueryAnalysis) -> Vec<QueryUnit> {
    // SCAFFOLD: one unit holding the whole query. The engine-analyze agent implements the
    // §7 rules: detect several independent requirements / relations, emit U1..Un with
    // explicit `depends_on`, mark mandatory predicates, and cap at MAX_UNITS.
    let mut units = vec![QueryUnit {
        id: "U1".to_string(),
        text: analysis.original.clone(),
        depends_on: Vec::new(),
        mandatory: true,
        weight: 1.0,
        kind: UnitKind::Other,
    }];
    units.truncate(MAX_UNITS);
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
