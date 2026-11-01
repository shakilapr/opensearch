//! §7 Sparse expert routing — the rule-based router (v2 §7, `PLAN-v2` Phase E).
//!
//! v2 §7 asks for a router that "identifies relevant search strategies" and picks "a small
//! subset of experts", while §7 also requires that the architecture "preserve a shared global
//! retrieval path to prevent router errors from excluding relevant documents". Those two
//! requirements pull in opposite directions, so the design here is deliberately asymmetric:
//!
//! * The **lexical expert is never removed**. A router that can turn off the only lane which
//!   matches the literal query is a router that can lose a document containing the exact string
//!   the user typed, and no amount of measured average recall makes that acceptable.
//! * Every other expert is **additive and skippable**. Skipping one costs latency and whatever
//!   recall that lane alone would have contributed; it cannot cost the query itself.
//!
//! v2 §7 also requires routing quality and fallback coverage to be **measured**, not asserted.
//! Hence: the router is off unless `SEARCH_ROUTE=1`, so the default path is byte-identical to
//! the unrouted one, and the eval harness can A/B the same query set with and without it.
//!
//! This is Model E at its cheapest useful form — the plan's "rule-based router over query
//! features" — not a learned model. The rules below are the whole policy; there is no hidden
//! second stage.

use crate::analyze::QueryAnalysis;
use crate::LaneId;

/// A retrieval strategy the router can switch on or off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Expert {
    /// BM25 over the full corpus. Never removed — the global recall path.
    Lexical,
    /// Restrict-and-rank within sources the query names.
    Source,
    /// Brute-force scan of the document-vector sidecar.
    Dense,
    /// Inverted scan of learned sparse features (v2 §5.2/§9).
    Features,
    /// Weighted document↔concept matrix.
    Concept,
    /// Entity-graph expansion.
    Graph,
}

impl Expert {
    /// Every expert, in the order the plan lists them.
    pub const ALL: [Expert; 6] = [
        Expert::Lexical,
        Expert::Source,
        Expert::Dense,
        Expert::Features,
        Expert::Concept,
        Expert::Graph,
    ];

    /// Short name for traces and the eval matrix. Matches `Lane::as_str` where they overlap.
    pub fn as_str(self) -> &'static str {
        match self {
            Expert::Lexical => "lexical",
            Expert::Source => "source",
            Expert::Dense => "dense",
            Expert::Features => "features",
            Expert::Concept => "concept",
            Expert::Graph => "graph",
        }
    }
}

/// What kind of query the router thinks it is looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryClass {
    /// Carries a literal surface the user wants matched: a quoted phrase, an identifier, an
    /// exact-phrase predicate, or a direct intent that outranks semantic intent.
    ExactSurface,
    /// One or two content words with no surface marker. Too little text to trust a single lane.
    ShortAmbiguous,
    /// Everything else: a description of what the user wants, with no literal target.
    Conceptual,
}

impl QueryClass {
    pub fn as_str(self) -> &'static str {
        match self {
            QueryClass::ExactSurface => "exact",
            QueryClass::ShortAmbiguous => "short",
            QueryClass::Conceptual => "conceptual",
        }
    }
}

/// The router's decision for one query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub class: QueryClass,
    /// Experts to run, in [`Expert::ALL`] order. Always contains [`Expert::Lexical`].
    pub experts: Vec<Expert>,
}

impl Route {
    /// Whether this route runs `expert`.
    pub fn runs(&self, expert: Expert) -> bool {
        self.experts.contains(&expert)
    }

    /// A compact form for the response trace: `conceptual[lexical,dense,features]`.
    pub fn summary(&self) -> String {
        let names: Vec<&str> = self.experts.iter().map(|e| e.as_str()).collect();
        format!("{}[{}]", self.class.as_str(), names.join(","))
    }
}

/// Classify a query. Public so the eval harness can report the distribution it actually saw
/// rather than the distribution the rules expected.
pub fn classify(analysis: &QueryAnalysis) -> QueryClass {
    // Surface markers only. `intent.direct > intent.semantic` is deliberately *not* used here:
    // `direct` has a 0.15 floor and `semantic` is 0.1 for any query without a question word, so
    // that comparison is true for nearly every query — it would route the whole workload down
    // the exact-surface branch. The markers below are the ones that actually name a literal.
    let exact_surface = !analysis.identifiers.is_empty()
        || !analysis.phrases.is_empty()
        || analysis
            .hard_predicates
            .iter()
            .any(|p| p.kind == crate::analyze::PredicateKind::ExactPhrase);
    if exact_surface {
        return QueryClass::ExactSurface;
    }
    // Content words, not raw token count and not `units`: `units` is a single unit unless the
    // query is explicitly conjunctive (`x and y`, `compare a with b`), so on its own it calls
    // every query short. "what is the capital of" is eight tokens and two content words.
    if analysis.concepts.len() <= 2 {
        return QueryClass::ShortAmbiguous;
    }
    QueryClass::Conceptual
}

/// Route a query to a set of experts (v2 §7).
///
/// The policy, and why each line exists:
///
/// * `ExactSurface` → lexical + source. The user named a literal; a semantic lane can only
///   re-rank what the lexical lane already found, and on an exact-string query the semantic
///   lanes' paraphrase tolerance is the failure mode, not the feature. Source is kept because
///   an exact-surface query is often a navigational one (`example.com/changelog`).
/// * `ShortAmbiguous` → lexical + dense + features. Two words give the lexical lane almost no
///   signal (a one-word BM25 query is barely a filter) and no paraphrase anchor, so the
///   semantic lanes carry the disambiguation. Features rather than concept: the feature lane
///   answers from an inverted index, so adding it costs a posting scan, not a corpus scan.
/// * `Conceptual` → lexical + dense + features + concept. The full semantic set; concept is
///   included because a conceptual query is exactly the class where the document↔concept
///   matrix was measured to contribute.
/// * `Graph` is never routed on by a rule. Graph expansion adds entities *not mentioned* by the
///   query, and on the graded set that is a precision loss the other lanes already pay for;
///   it stays available by explicit configuration (`SEARCH_ROUTE_GRAPH=1`) so it can be
///   measured rather than assumed.
pub fn route(analysis: &QueryAnalysis) -> Route {
    let class = classify(analysis);
    let mut experts = vec![Expert::Lexical];
    match class {
        QueryClass::ExactSurface => experts.push(Expert::Source),
        QueryClass::ShortAmbiguous => {
            experts.push(Expert::Dense);
            experts.push(Expert::Features);
        }
        QueryClass::Conceptual => {
            experts.push(Expert::Dense);
            experts.push(Expert::Features);
            experts.push(Expert::Concept);
        }
    }
    if std::env::var("SEARCH_ROUTE_GRAPH").ok().as_deref() == Some("1") {
        experts.push(Expert::Graph);
    }
    experts.sort();
    experts.dedup();
    Route { class, experts }
}

/// Whether routing is enabled at all.
///
/// Off by default: the plan's gate for Phase E is that the router "never reduces recall@20
/// versus searching everything", and until that is measured on the graded set, changing the
/// default path would be a regression risk with no evidence behind it.
pub fn enabled() -> bool {
    matches!(
        std::env::var("SEARCH_ROUTE").ok().as_deref(),
        Some("1") | Some("true")
    )
}

/// Map a fusion lane onto its expert. `None` for a lane with no hint, which callers must treat
/// as "run it" — an unidentified lane is never dropped.
pub fn expert_for(lane: LaneId) -> Option<Expert> {
    Some(match lane {
        LaneId::Bm25 => Expert::Lexical,
        LaneId::SourceFirst => Expert::Source,
        LaneId::Dense => Expert::Dense,
        LaneId::Features => Expert::Features,
        LaneId::Concept => Expert::Concept,
        LaneId::Graph => Expert::Graph,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::analyze_query;

    fn a(q: &str) -> QueryAnalysis {
        analyze_query(q, 1_700_000_000, 0)
    }

    #[test]
    fn the_lexical_expert_is_never_routed_away() {
        // Every class, including the one that excludes everything else, keeps lexical: §7's
        // global fallback is a property of the router, not of the caller.
        for q in [
            "\"connection reset by peer\"",
            "RUSTSEC-2024-0043",
            "tokio",
            "how do i make a rust future cancel safe when it holds a lock across an await",
        ] {
            let r = route(&a(q));
            assert!(
                r.runs(Expert::Lexical),
                "{q:?} routed without the lexical expert: {r:?}"
            );
        }
    }

    #[test]
    fn an_exact_surface_query_does_not_pay_for_semantic_lanes() {
        let r = route(&a("\"connection reset by peer\""));
        assert_eq!(r.class, QueryClass::ExactSurface);
        assert!(
            !r.runs(Expert::Dense),
            "a quoted phrase must not pay for a corpus scan: {r:?}"
        );
        assert!(!r.runs(Expert::Features));
        assert!(r.runs(Expert::Source));
    }

    #[test]
    fn an_identifier_query_routes_lexically() {
        assert_eq!(
            route(&a("RUSTSEC-2024-0043")).class,
            QueryClass::ExactSurface
        );
        assert_eq!(
            route(&a("tokio::sync::Mutex")).class,
            QueryClass::ExactSurface
        );
    }

    #[test]
    fn a_one_word_query_is_not_treated_as_conceptual() {
        let r = route(&a("borrowck"));
        assert_eq!(r.class, QueryClass::ShortAmbiguous);
        assert!(r.runs(Expert::Dense));
        assert!(
            !r.runs(Expert::Concept),
            "a single word has no concept to expand and must not pay for the concept lane"
        );
    }

    #[test]
    fn a_conceptual_query_gets_the_semantic_experts() {
        let r = route(&a(
            "how do i make a rust future cancel safe when it holds a lock across an await",
        ));
        assert_eq!(r.class, QueryClass::Conceptual);
        for e in [Expert::Dense, Expert::Features, Expert::Concept] {
            assert!(r.runs(e), "conceptual query missing {:?}: {r:?}", e);
        }
        assert!(
            !r.runs(Expert::Graph),
            "graph expansion is opt-in, never rule-driven"
        );
    }

    #[test]
    fn routes_are_ordered_and_deduplicated() {
        let r = route(&a(
            "how do i make a rust future cancel safe when it holds a lock",
        ));
        let mut sorted = r.experts.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(r.experts, sorted);
    }

    #[test]
    fn summary_names_class_and_experts() {
        let r = route(&a("\"exact phrase here\""));
        assert_eq!(r.summary(), "exact[lexical,source]");
    }
}
