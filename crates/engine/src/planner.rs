//! §6/§8 — the optional, injected query planner.
//!
//! `analyze_query` is deterministic and purely lexical: it can split a query, pull out
//! identifiers and quoted phrases, and classify intent, but it cannot read a conversational
//! turn, resolve a pronoun, or name the canonical entity behind a paraphrase. Those are model
//! jobs. This module is the seam the layer above injects them through, exactly as
//! [`crate::Answerer`] is the seam for answer generation and [`crate::EvidenceVerifier`] is the
//! seam for content-level verification.
//!
//! The engine consumes **only the planner's output** — a bounded list of expansion probes that
//! §8 turns into [`crate::plan::QueryVariant`]s — so the crate stays deterministic and offline:
//! the trait is synchronous (the engine is synchronous), and an implementation that talks to a
//! model over the network owns its own runtime and its own failure handling. A planner that
//! errors returns no expansions rather than failing the search; every expansion is still subject
//! to [`crate::plan::variant_preserves_constraints`] and the `MAX_QUERY_VARIANTS` cap, so a
//! planner cannot smuggle in a rewrite that drops a negation, an identifier, or a date.

use crate::analyze::QueryAnalysis;

/// §6/§8 — deconstruct one query into bounded expansion probes.
///
/// The method is deliberately total and synchronous: it returns a [`PlannedQueries`] (possibly
/// empty) and never panics on the request path. A transport failure inside an implementation is
/// reported as *no expansions* plus a note, never as an error the search would have to handle.
pub trait QueryPlanner: Send + Sync {
    /// Deconstruct `query` into expansion probes. `analysis` is the deterministic analysis the
    /// engine already computed for the same `query`, so a planner can condition on the phrases,
    /// identifiers and intent the rule analyzer found without recomputing them.
    fn plan(&self, query: &str, analysis: &QueryAnalysis) -> PlannedQueries;
}

/// What a [`QueryPlanner`] produced for one query.
///
/// `expansions` are the *only* thing that reaches retrieval: §8 searches the original verbatim in
/// its own lane and searches each expansion as a variant in a shared family, and §10 caps that
/// family's combined weight so a handful of rewrites cannot outvote the original. A planner that
/// returns nothing changes no behaviour.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlannedQueries {
    /// Expansion probes to search *in addition to* the original query. Bounded by the planner;
    /// the engine applies `variant_preserves_constraints` and `MAX_QUERY_VARIANTS` on top, so
    /// over-returning is safe but pointless. Order is the planner's preference; the first
    /// survivors within the cap are the ones searched.
    pub expansions: Vec<String>,
    /// Human-readable notes copied into the response trace (for example "4 entities, 2 phrases
    /// with slop"). Notes never affect ranking or eligibility; they exist so a caller can see
    /// what the model did — including when it did nothing — without re-running it.
    pub notes: Vec<String>,
}

impl PlannedQueries {
    /// No expansions, no notes. The identity value: the engine launches only the original query.
    pub fn none() -> Self {
        Self {
            expansions: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// True when the planner produced nothing to search — the engine then behaves exactly as it
    /// does with no planner injected at all.
    pub fn is_empty(&self) -> bool {
        self.expansions.iter().all(|e| e.trim().is_empty())
    }
}
