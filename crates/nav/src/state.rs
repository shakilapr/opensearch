//! `Action`, `SearchState`, and the action frontier entry `SearchAction`.

use std::collections::{BTreeMap, BTreeSet};

/// One executable frontier action (engine-arch §15). The set is *closed*: the engine only
/// ever offers these, and a model may only pick among offered ids — never invent
/// infrastructure or a URL.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    /// Run a (possibly reformulated) retrieval for `query`.
    Retrieve { query: String },
    /// Expand the entity graph from a bound entity.
    ExpandGraph { entity_id: String },
    /// Re-fetch/revalidate a known document by URL.
    Refresh { url: String },
    /// Follow a known document to its outbound/related evidence.
    FollowLink { doc_id: String },
    /// Re-score the current candidate pool.
    Rerank,
    /// Verify a specific join key binds two units (multi-hop).
    VerifyJoin { key: String },
    /// Broaden recall (aliases, global lane, new sources).
    Widen,
}

impl Action {
    /// Short stable kind tag used in structured model prompts.
    pub fn slug(&self) -> &'static str {
        match self {
            Action::Retrieve { .. } => "retrieve",
            Action::ExpandGraph { .. } => "expand_graph",
            Action::Refresh { .. } => "refresh",
            Action::FollowLink { .. } => "follow_link",
            Action::Rerank => "rerank",
            Action::VerifyJoin { .. } => "verify_join",
            Action::Widen => "widen",
        }
    }
}

/// Lowercase, whitespace-collapsed canonical form of a payload string. Used only for
/// canonical keys (cycle detection); never for fetching.
fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// The search state carried through navigation (engine-arch §15).
///
/// `action_keys` is the exact visited/action-key set: a canonical key per executed action,
/// so re-issuing any action is a no-op. `visited_docs` dedupes evidence; `results` is the
/// best-first document ordering; `hops` bounds depth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchState {
    pub query: String,
    pub visited_docs: BTreeSet<String>,
    pub action_keys: BTreeSet<String>,
    pub hops: u8,
    pub results: Vec<String>,
}

impl SearchState {
    /// Fresh state for `query`.
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            visited_docs: BTreeSet::new(),
            action_keys: BTreeSet::new(),
            hops: 0,
            results: Vec::new(),
        }
    }

    /// Stable canonical key for an action. Two actions with equal keys are the *same*
    /// action for cycle-detection purposes and the second must never execute.
    pub fn key_of(action: &Action) -> String {
        match action {
            Action::Retrieve { query } => format!("retrieve:{}", norm(query)),
            Action::ExpandGraph { entity_id } => format!("expand_graph:{}", norm(entity_id)),
            Action::Refresh { url } => format!("refresh:{}", norm(url)),
            Action::FollowLink { doc_id } => format!("follow_link:{}", norm(doc_id)),
            Action::Rerank => "rerank".to_string(),
            Action::VerifyJoin { key } => format!("verify_join:{}", norm(key)),
            Action::Widen => "widen".to_string(),
        }
    }

    /// Whether this action (by canonical key) has already been issued in this state.
    pub fn seen(&self, action: &Action) -> bool {
        self.action_keys.contains(&Self::key_of(action))
    }

    /// Record that `action` was issued. Idempotent.
    pub fn record(&mut self, action: &Action) {
        self.action_keys.insert(Self::key_of(action));
    }

    /// Add a document id to `visited_docs` and, when new, to the best-first `results`.
    /// Returns `true` when the document was new.
    pub fn add_result(&mut self, doc_id: impl Into<String>) -> bool {
        let doc_id = doc_id.into();
        let fresh = self.visited_docs.insert(doc_id.clone());
        if fresh {
            self.results.push(doc_id);
        }
        fresh
    }
}

/// A frontier entry: an action plus its estimated value/cost and admissibility.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchAction {
    pub action: Action,
    pub expected_gain: f32,
    pub cost: f32,
    /// A model may only pick admissible actions; the navigator re-checks this at dispatch.
    pub admissible: bool,
}

impl SearchAction {
    /// Convenience constructor for an admissible action.
    pub fn new(action: Action, expected_gain: f32, cost: f32) -> Self {
        Self {
            action,
            expected_gain,
            cost,
            admissible: true,
        }
    }

    /// expected_gain / cost, with sane handling of zero cost.
    pub fn ratio(&self) -> f32 {
        ratio_of(self)
    }
}

/// expected_gain / cost; a zero-cost positive-gain action is treated as infinitely good.
pub(crate) fn ratio_of(a: &SearchAction) -> f32 {
    if a.cost <= 0.0 {
        if a.expected_gain > 0.0 {
            f32::INFINITY
        } else {
            0.0
        }
    } else {
        a.expected_gain / a.cost
    }
}

/// A set of candidates sharing one interpretation key (one reading of the query).
pub(crate) struct Group {
    pub tag: String,
    pub best: SearchAction,
    pub best_ratio: f32,
    pub members: Vec<SearchAction>,
}

/// Group candidate actions by canonical key (= interpretation, since distinct targets are
/// distinct readings). Groups are returned best-first by `best_ratio`, ties by tag.
pub(crate) fn group_by_key(actions: &[SearchAction]) -> Vec<Group> {
    let mut map: BTreeMap<String, Vec<SearchAction>> = BTreeMap::new();
    for a in actions {
        map.entry(SearchState::key_of(&a.action)).or_default().push(a.clone());
    }
    let mut groups: Vec<Group> = map
        .into_iter()
        .map(|(tag, members)| {
            let (best, best_ratio) = members
                .iter()
                .map(|m| (m, ratio_of(m)))
                .fold(None, |acc: Option<(SearchAction, f32)>, (m, r)| match acc {
                    Some((_, br)) if br >= r => acc,
                    _ => Some((m.clone(), r)),
                })
                .expect("group is non-empty");
            Group {
                tag,
                best,
                best_ratio,
                members,
            }
        })
        .collect();
    groups.sort_by(|a, b| {
        b.best_ratio
            .partial_cmp(&a.best_ratio)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    groups
}
