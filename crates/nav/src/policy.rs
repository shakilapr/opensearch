//! The decision hierarchy (engine-arch §12.1) and the built-in deterministic policies.
//!
//! Cheapest sufficient tier wins: `Rules` -> `Classifier` -> `LocalModel` -> `Llm`. A policy
//! only gets a turn when every cheaper tier returned `None`; tiers above `Rules` additionally
//! require the escalation conditions (an important uncovered constraint and a gainful,
//! admissible, affordable action) before they may choose.

use std::sync::Mutex;

use crate::state::{group_by_key, ratio_of, SearchAction, SearchState};

/// The policy tier. Ordered: `Rules < Classifier < LocalModel < Llm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PolicyTier {
    Rules,
    Classifier,
    LocalModel,
    Llm,
}

/// A navigation policy: given structured state + admissible actions, pick one or abstain.
pub trait SearchPolicy: Send + Sync {
    fn tier(&self) -> PolicyTier;
    fn choose(&self, state: &SearchState, actions: &[SearchAction]) -> Option<SearchAction>;
}

/// Minimum expected_gain/cost ratio for the rules tier to commit on its own.
const RULES_MIN_RATIO: f32 = 0.5;
/// If the two best interpretations are within this factor, rules defers (ambiguous).
const RULES_AMBIGUITY_MARGIN: f32 = 1.5;

/// Deterministic heuristics that never need the network (engine-arch §12.1, level 0).
///
/// Rules only commits to *local* actions (retrieve / expand / follow / rerank / widen) and
/// abstains — forcing escalation — when (a) only network actions (refresh / verify_join) are
/// offered, (b) two interpretations are within [`RULES_AMBIGUITY_MARGIN`], or (c) the best
/// ratio is below [`RULES_MIN_RATIO`].
#[derive(Debug, Default, Clone, Copy)]
pub struct RulesPolicy;

fn rules_local(a: &SearchAction) -> bool {
    !matches!(
        a.action,
        crate::state::Action::Refresh { .. } | crate::state::Action::VerifyJoin { .. }
    )
}

impl SearchPolicy for RulesPolicy {
    fn tier(&self) -> PolicyTier {
        PolicyTier::Rules
    }

    fn choose(&self, state: &SearchState, actions: &[SearchAction]) -> Option<SearchAction> {
        let considered: Vec<SearchAction> = actions
            .iter()
            .filter(|a| a.admissible && rules_local(a) && !state.seen(&a.action) && a.expected_gain > 0.0)
            .cloned()
            .collect();
        if considered.is_empty() {
            return None;
        }
        let groups = group_by_key(&considered);
        if groups.len() >= 2 {
            let top = groups[0].best_ratio;
            let second = groups[1].best_ratio;
            // Comparable top interpretations -> ambiguous -> defer to a higher tier / beam.
            if top <= second * RULES_AMBIGUITY_MARGIN {
                return None;
            }
        }
        let best = &groups[0].best;
        if best.ratio() < RULES_MIN_RATIO {
            return None;
        }
        Some(best.clone())
    }
}

/// Best-first by expected_gain/cost — the deterministic algorithmic tier (classifier level).
///
/// Always picks among admissible candidates when any exist (ties broken by lower cost, then
/// the caller's order); this is the tier that makes an ambiguous query productive without a
/// model call.
#[derive(Debug, Default, Clone, Copy)]
pub struct AlgorithmPolicy;

impl SearchPolicy for AlgorithmPolicy {
    fn tier(&self) -> PolicyTier {
        PolicyTier::Classifier
    }

    fn choose(&self, state: &SearchState, actions: &[SearchAction]) -> Option<SearchAction> {
        actions
            .iter()
            .filter(|a| a.admissible && !state.seen(&a.action))
            .max_by(|a, b| {
                ratio_of(a)
                    .partial_cmp(&ratio_of(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    // lower cost wins on ratio ties
                    .then_with(|| b.cost.partial_cmp(&a.cost).unwrap_or(std::cmp::Ordering::Equal))
            })
            .cloned()
    }
}

/// The calling agent supplies the choice (structured state only, no invented actions).
///
/// The choice is consumed once: the first node asked in a run takes it. Assigned the `Llm`
/// tier, so it is subject to the same escalation gate as any model tier.
#[derive(Default)]
pub struct CallerAgentPolicy {
    choice: Mutex<Option<SearchAction>>,
}

impl std::fmt::Debug for CallerAgentPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallerAgentPolicy").finish_non_exhaustive()
    }
}

impl CallerAgentPolicy {
    pub fn new() -> Self {
        Self {
            choice: Mutex::new(None),
        }
    }

    /// Supply the caller's choice for the next run.
    pub fn set(&self, action: SearchAction) {
        *self.choice.lock().expect("caller policy mutex poisoned") = Some(action);
    }

    /// Drop any pending choice.
    pub fn clear(&self) {
        *self.choice.lock().expect("caller policy mutex poisoned") = None;
    }
}

impl SearchPolicy for CallerAgentPolicy {
    fn tier(&self) -> PolicyTier {
        PolicyTier::Llm
    }

    fn choose(&self, state: &SearchState, _actions: &[SearchAction]) -> Option<SearchAction> {
        let mut guard = self.choice.lock().expect("caller policy mutex poisoned");
        match guard.take() {
            Some(a) if a.admissible && !state.seen(&a.action) => Some(a),
            _ => None,
        }
    }
}
