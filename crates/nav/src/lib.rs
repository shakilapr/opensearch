//! Agent navigation (plan Stage 4, engine §7/§18/§19, engine-arch §12/§15/§17).
//!
//! `SearchState` + action frontier with exact cycle detection, best-first traversal, beam
//! search for ambiguity, evidence joins for multi-hop, and a policy hierarchy (rules →
//! classifier → local model → bounded LLM) that escalates only when a cheaper tier is not
//! sufficient. Interfaces are frozen in `docs/STAGE4_SPEC.md`.
//!
//! ## Escalation rule (as implemented)
//!
//! Policies are consulted cheapest-first (`Rules < Classifier < LocalModel < Llm`). The
//! cheapest tier that returns `Some` wins and its [`PolicyTier`] is recorded on the
//! [`Step`]. A tier above `Rules` is only consulted when every cheaper tier returned `None`
//! **and** [`Navigator::escalation_ok`] holds: there exists a positive-value
//! (`expected_gain > cost`), admissible, unseen action and the [`Budget`] is not exhausted.
//! Because a tier is only reached after cheaper tiers abstained, the existence of such an
//! action is the machine-checkable form of "an important constraint / interpretation is
//! unresolved" (engine-arch §12.4); callers may additionally set
//! [`NavContext::uncovered`].
//!
//! ## Beam bound (as implemented)
//!
//! A node whose candidates span ≥2 credible interpretations expands one child per
//! interpretation, bounded by `beam`, with at least `reserve_per_interpretation` slot(s)
//! reserved per interpretation. The frontier is then re-selected to ≤ `beam` nodes by
//! round-robin across interpretation groups. This is a bounded beam: at most `beam` live
//! states, at most `beam` expansions per round, at most `max_hops` deep — never an
//! unbounded search tree. When a query is unambiguous the beam collapses to a single
//! best-first path.

#![forbid(unsafe_code)]

mod budget;
mod error;
mod executor;
mod navigator;
mod policy;
mod provider;
mod state;

pub use budget::Budget;
pub use error::NavError;
pub use executor::{NavContext, SearchExecutor};
pub use navigator::{evidence_join, plan_budget, NavConfig, NavOutcome, Navigator, StopReason, Step};
pub use policy::{
    AlgorithmPolicy, CallerAgentPolicy, PolicyTier, RulesPolicy, SearchPolicy,
};
pub use provider::{build_prompt, ModelClient, Provider, ProviderPolicy};
pub use state::{Action, SearchAction, SearchState};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_of_is_stable_and_normalizes() {
        let a = Action::Retrieve {
            query: "  Rust   Ownership ".into(),
        };
        let b = Action::Retrieve {
            query: "rust ownership".into(),
        };
        assert_eq!(SearchState::key_of(&a), SearchState::key_of(&b));
        assert_eq!(SearchState::key_of(&a), "retrieve:rust ownership");
    }

    #[test]
    fn tier_ordering_is_the_hierarchy() {
        assert!(PolicyTier::Rules < PolicyTier::Classifier);
        assert!(PolicyTier::Classifier < PolicyTier::LocalModel);
        assert!(PolicyTier::LocalModel < PolicyTier::Llm);
    }

    #[test]
    fn budget_exhaustion_and_affordability() {
        let mut b = Budget::new(2, 100, 1.0);
        assert!(!b.exhausted());
        assert!(b.can_afford(0.5));
        b.charge(0.5);
        b.charge(0.5);
        assert!(b.exhausted(), "cost cap reached");
        assert!(!b.can_afford(0.0), "exhausted budget affords nothing");
    }
}
