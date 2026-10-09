//! Hermetic, deterministic tests for §8 (`plan`) and §17 (`budget`).
//!
//! No network, no clock reads: time is always an explicit argument, and the ledger is anchored
//! with `start(...)`. Every assertion is on pure function output.

use search_engine::{
    build_plan, variant_preserves_constraints, Budget, IndexStats, Intent, Mode, Predicate,
    PredicateKind, QueryAnalysis, StopReason, VariantOrigin,
};
use search_engine::plan::{mode_deadline_ms, MAX_QUERY_VARIANTS};
use search_engine::LaneId;

fn stats() -> IndexStats {
    IndexStats {
        docs: 10_000,
        avg_doc_len: 512.0,
        updated_at: 0,
    }
}

/// An exact error-code / identifier query: lexical, high direct intent.
fn lexical_analysis() -> QueryAnalysis {
    let mut a = QueryAnalysis::default();
    a.original = "Error code E1234 in version 3.2".to_string();
    a.normalized = a.original.to_lowercase();
    a.identifiers = vec!["E1234".to_string(), "3.2".to_string()];
    a.terms = vec!["error".to_string(), "code".to_string()];
    a.intent = Intent {
        direct: 0.9,
        semantic: 0.1,
        ambiguous: 0.0,
        multihop: 0.0,
    };
    a
}

/// A conceptual query: no identifiers, semantic intent dominates.
fn conceptual_analysis() -> QueryAnalysis {
    let mut a = QueryAnalysis::default();
    a.original = "why do local models reduce inference cost".to_string();
    a.normalized = a.original.clone();
    a.concepts = vec![
        "local model inference".to_string(),
        "cost reduction".to_string(),
    ];
    a.intent = Intent {
        direct: 0.1,
        semantic: 0.9,
        ambiguous: 0.0,
        multihop: 0.0,
    };
    a
}

// ---------------------------------------------------------------------------
// §8 planning
// ---------------------------------------------------------------------------

#[test]
fn exact_identifier_query_gets_smaller_rerank_than_conceptual() {
    // §8: "an exact error-code query may need lexical retrieval and a small rerank; a
    // conceptual query may justify dense and sparse lanes."
    let a = build_plan(&lexical_analysis(), &stats(), Mode::Auto);
    let b = build_plan(&conceptual_analysis(), &stats(), Mode::Auto);
    assert!(
        a.rerank_count < b.rerank_count,
        "lexical rerank {} should be smaller than conceptual rerank {}",
        a.rerank_count,
        b.rerank_count
    );
    // Both stay inside the §17 Auto reranked-document cap of 100.
    assert!(a.rerank_count <= 100 && b.rerank_count <= 100);
}

#[test]
fn budget_dimensions_are_monotone_across_modes() {
    // §17 ceilings must be non-decreasing Fast <= Auto <= Deep for the conceptual (largest) plan.
    let s = stats();
    let fast = build_plan(&conceptual_analysis(), &s, Mode::Fast);
    let auto = build_plan(&conceptual_analysis(), &s, Mode::Auto);
    let deep = build_plan(&conceptual_analysis(), &s, Mode::Deep);

    assert!(fast.per_lane_top_k <= auto.per_lane_top_k);
    assert!(auto.per_lane_top_k <= deep.per_lane_top_k);

    assert!(fast.rerank_count <= auto.rerank_count);
    assert!(auto.rerank_count <= deep.rerank_count);

    assert!(fast.source_route_count <= auto.source_route_count);
    assert!(auto.source_route_count <= deep.source_route_count);

    assert!(fast.refresh_count <= auto.refresh_count);
    assert!(auto.refresh_count <= deep.refresh_count);

    // And the exact §17 ceilings.
    assert_eq!(deep.rerank_count, 300);
    assert_eq!(deep.source_route_count, 100);
    assert_eq!(deep.refresh_count, 30);
    assert_eq!(fast.refresh_count, 0);
}

#[test]
fn global_bm25_lane_is_always_present() {
    // §8: "The normal plan still keeps a global recall path instead of making source routing an
    // irreversible gate." Lane::Bm25 is that global path and must never be dropped.
    let kinds = [lexical_analysis(), conceptual_analysis()];
    for mode in [Mode::Fast, Mode::Auto, Mode::Deep] {
        for a in &kinds {
            let plan = build_plan(a, &stats(), mode);
            assert!(
                plan.lanes.contains(&LaneId::Bm25),
                "global BM25 lane missing for {mode:?}"
            );
            assert!(!plan.lanes.is_empty());
        }
    }
}

#[test]
fn exactly_one_original_variant_and_it_is_first() {
    // §8: "Search the original query in its own lane." Exactly one Original, verbatim, family 0.
    let kinds = [lexical_analysis(), conceptual_analysis()];
    for mode in [Mode::Fast, Mode::Auto, Mode::Deep] {
        for a in &kinds {
            let plan = build_plan(a, &stats(), mode);
            let originals = plan
                .query_variants
                .iter()
                .filter(|v| v.origin == VariantOrigin::Original)
                .count();
            assert_eq!(originals, 1, "exactly one Original variant");
            let first = &plan.query_variants[0];
            assert_eq!(first.origin, VariantOrigin::Original, "Original must be first");
            assert_eq!(first.text, a.original, "Original text is verbatim");
            assert_eq!(first.family, 0, "Original is family 0");
        }
    }
}

#[test]
fn alias_variants_are_tagged_and_capped() {
    // §8: "Cap expansions, record their origin." Conceptual analysis supplies concept strings;
    // each accepted one is an Alias variant under a shared non-zero family.
    let plan = build_plan(&conceptual_analysis(), &stats(), Mode::Auto);
    assert!(plan.query_variants.len() <= MAX_QUERY_VARIANTS);
    assert_eq!(plan.query_variants[0].origin, VariantOrigin::Original);
    let aliases: Vec<&search_engine::QueryVariant> = plan
        .query_variants
        .iter()
        .filter(|v| v.origin == VariantOrigin::Alias)
        .collect();
    assert!(
        !aliases.is_empty(),
        "concept-supplied aliases should survive constraint preservation"
    );
    for alias in &aliases {
        assert_ne!(alias.family, 0, "alias family id must be non-zero");
        assert!(!alias.text.is_empty());
    }
    // All aliases share one family.
    let fams: std::collections::BTreeSet<u8> =
        aliases.iter().map(|v| v.family).collect();
    assert_eq!(fams.len(), 1, "all alias variants share a single family id");
}

#[test]
fn stage_deadlines_sum_within_overall_deadline() {
    // §17: "Reserve finalization time before scheduling work"; stage timeouts are "shorter than
    // the overall deadline".
    for mode in [Mode::Fast, Mode::Auto, Mode::Deep] {
        let plan = build_plan(&conceptual_analysis(), &stats(), mode);
        let d = plan.stage_deadlines_ms;
        let sum = d.retrieve_ms + d.fuse_ms + d.rerank_ms + d.finalize_ms;
        let overall = mode_deadline_ms(mode);
        assert!(
            sum <= overall,
            "{mode:?}: stage deadlines {sum} exceed overall {overall}"
        );
        assert!(d.finalize_ms > 0, "finalize time must be reserved");
    }
    // Sanity on the exact §17 overall deadlines.
    assert_eq!(mode_deadline_ms(Mode::Fast), 200);
    assert_eq!(mode_deadline_ms(Mode::Auto), 3_000);
    assert_eq!(mode_deadline_ms(Mode::Deep), 20_000);
}

// ---------------------------------------------------------------------------
// §8 variant_preserves_constraints
// ---------------------------------------------------------------------------

#[test]
fn constraints_reject_dropped_identifier() {
    let a = lexical_analysis();
    assert!(variant_preserves_constraints("Error code in version", &a) == false);
    // Case-insensitive: lowercasing the identifier still counts as present.
    assert!(variant_preserves_constraints("error code e1234 version 3.2", &a));
}

#[test]
fn constraints_reject_dropped_negation() {
    let mut a = QueryAnalysis::default();
    a.original = "rust not async".to_string();
    a.normalized = a.original.clone();
    assert!(!variant_preserves_constraints("rust async", &a));

    let mut b = QueryAnalysis::default();
    b.original = "why this doesn't work".to_string();
    b.normalized = b.original.clone();
    assert!(!variant_preserves_constraints("why this works", &b));
}

#[test]
fn constraints_reject_dropped_quoted_phrase() {
    let mut a = QueryAnalysis::default();
    a.original = "search for \"exact match\" now".to_string();
    a.normalized = a.original.clone();
    a.hard_predicates = vec![Predicate {
        kind: PredicateKind::ExactPhrase,
        value: "exact match".to_string(),
    }];
    assert!(!variant_preserves_constraints("search for something now", &a));
    assert!(variant_preserves_constraints("search for \"exact match\" later", &a));
}

#[test]
fn constraints_reject_lost_year_when_dated() {
    let mut a = QueryAnalysis::default();
    a.original = "release notes 2024".to_string();
    a.normalized = a.original.clone();
    // Original carries a 4-digit year; a rewrite losing every digit is rejected.
    assert!(!variant_preserves_constraints("release notes", &a));
    assert!(variant_preserves_constraints("release notes 2024", &a));
}

#[test]
fn constraints_accept_benign_alias_rewrite() {
    let mut a = QueryAnalysis::default();
    a.original = "nyc weather".to_string();
    a.normalized = a.original.clone();
    // A rewrite that keeps every constraint is accepted.
    assert!(variant_preserves_constraints("new york city weather", &a));
}

// ---------------------------------------------------------------------------
// §17 budget
// ---------------------------------------------------------------------------

#[test]
fn fast_has_one_round_deep_has_five() {
    let fast = Budget::for_mode(Mode::Fast, mode_deadline_ms(Mode::Fast));
    let auto = Budget::for_mode(Mode::Auto, mode_deadline_ms(Mode::Auto));
    let deep = Budget::for_mode(Mode::Deep, mode_deadline_ms(Mode::Deep));
    assert_eq!(fast.rounds_remaining, 1);
    assert_eq!(auto.rounds_remaining, 2);
    assert_eq!(deep.rounds_remaining, 5);
    // §17 other caps.
    assert_eq!((fast.fetches_remaining, fast.selector_calls_remaining, fast.rerank_cap), (0, 0, 30));
    assert_eq!((auto.fetches_remaining, auto.selector_calls_remaining, auto.rerank_cap), (8, 1, 100));
    assert_eq!((deep.fetches_remaining, deep.selector_calls_remaining, deep.rerank_cap), (30, 4, 300));
}

#[test]
fn charge_round_returns_false_exactly_once_rounds_are_gone() {
    let mut b = Budget::for_mode(Mode::Fast, mode_deadline_ms(Mode::Fast));
    assert!(b.charge_round(), "the single Fast round charges");
    assert_eq!(b.rounds_remaining, 0);
    assert!(!b.charge_round(), "no rounds remain -> false");
    assert!(!b.charge_round(), "still false, and never underflows");
    assert_eq!(b.rounds_remaining, 0);
}

#[test]
fn should_stop_returns_deadline_then_budget_exhausted() {
    // Deadline: past the deadline with rounds still on the ledger.
    let mut b = Budget::for_mode(Mode::Deep, 100);
    b.start(0);
    assert_eq!(b.should_stop(50), None, "time remains and rounds remain");
    assert_eq!(b.should_stop(100), Some(StopReason::Deadline));
    assert_eq!(b.should_stop(1_000), Some(StopReason::Deadline));

    // BudgetExhausted: rounds gone but time remains (Deep has 5 rounds, 10s deadline).
    let mut c = Budget::for_mode(Mode::Deep, 10_000);
    c.start(0);
    for _ in 0..5 {
        assert!(c.charge_round());
    }
    assert_eq!(c.rounds_remaining, 0);
    assert_eq!(c.should_stop(1_000), Some(StopReason::BudgetExhausted));
    // Deadline takes precedence when both apply.
    assert_eq!(c.should_stop(10_000), Some(StopReason::Deadline));
}

#[test]
fn remaining_ms_saturates_and_never_panics_before_start() {
    let mut b = Budget::for_mode(Mode::Fast, 100);
    b.start(1_000);
    assert_eq!(b.remaining_ms(1_000), 100);
    assert_eq!(b.remaining_ms(1_050), 50);
    assert_eq!(b.remaining_ms(1_200), 0, "past the deadline saturates at 0");
    assert_eq!(b.remaining_ms(99_999), 0);
    // now_ms strictly before started_ms must not underflow.
    assert_eq!(b.remaining_ms(0), 100);
}

#[test]
fn stop_no_action_names_the_reason() {
    let b = Budget::for_mode(Mode::Auto, mode_deadline_ms(Mode::Auto));
    assert_eq!(b.stop_no_action(), Some(StopReason::NoAdmissibleAction));
}
