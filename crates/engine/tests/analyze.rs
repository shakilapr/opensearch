//! Hermetic, deterministic tests for §6 (analyze) and §7 (units). No sleeps, no network,
//! no clock reads — every time value is passed explicitly.

use search_engine::analyze::{analyze_query, link_entities, AliasPriors, EntityMention, PredicateKind};
use search_engine::units::decompose;
use search_engine::MAX_UNITS;

const NOW: i64 = 86_400_000; // day 1000 since the epoch, 1972-09-27T00:00:00Z
const TZ_UTC2: i32 = 120; // UTC+02:00

/// The §7 multi-clause example, verbatim.
const MULTI: &str =
    "developers who moved from hosted APIs to local models because of cost, and the GPUs they bought";

// 1 ---------------------------------------------------------------------------------------------
#[test]
fn original_query_is_preserved_byte_for_byte() {
    let q = "Ünïcode \"Quoted Phrase\" v1.2.3 E0425 iPhone";
    let a = analyze_query(q, NOW, 0);
    assert_eq!(a.original, q, "original must be verbatim");
    // Normalization lowercases and folds, but must not touch `original`.
    assert!(a.normalized.chars().all(|c| !c.is_uppercase()));
    assert_eq!(a.original.as_bytes(), q.as_bytes());
}

// 2 ---------------------------------------------------------------------------------------------
#[test]
fn quoted_phrase_survives_into_phrases_and_predicate() {
    let a = analyze_query("Rust \"Error Code\" handling", NOW, 0);
    assert_eq!(a.phrases, vec!["Error Code".to_string()], "case preserved");
    assert!(
        a.hard_predicates
            .iter()
            .any(|p| p.kind == PredicateKind::ExactPhrase && p.value == "Error Code"),
        "quoted span must be an ExactPhrase predicate"
    );
}

// 3 ---------------------------------------------------------------------------------------------
#[test]
fn version_and_error_code_preserved_case_exact() {
    let a = analyze_query("upgrade to V1.2.3 and fix E0425", NOW, 0);
    assert!(a.identifiers.iter().any(|s| s == "V1.2.3"), "version kept: {:?}", a.identifiers);
    assert!(a.identifiers.iter().any(|s| s == "E0425"), "error code kept: {:?}", a.identifiers);
    assert!(a
        .hard_predicates
        .iter()
        .any(|p| p.kind == PredicateKind::Identifier && p.value == "V1.2.3"));
    assert!(a
        .hard_predicates
        .iter()
        .any(|p| p.kind == PredicateKind::Identifier && p.value == "E0425"));
}

// 4 ---------------------------------------------------------------------------------------------
#[test]
fn nfkc_and_whitespace_collapse() {
    // Fullwidth "Rust", ideographic space U+3000, and the ﬁ ligature.
    let a = analyze_query("\u{FF32}\u{FF55}\u{FF53}\u{FF54}\u{3000}\u{FB01}le", NOW, 0);
    assert_eq!(a.normalized, "rust file");
}

// 5 ---------------------------------------------------------------------------------------------
#[test]
fn temporal_today_and_last_n_days_resolve_to_utc_half_open() {
    // now = 86_400_000 (midnight UTC). At UTC+02:00 local midnight is 7200s earlier.
    let today = analyze_query("news today", NOW, TZ_UTC2);
    assert_eq!(today.temporal.map(|t| (t.start, t.end)), Some((86_392_800, 86_479_200)));

    let last7 = analyze_query("events last 7 days", NOW, TZ_UTC2);
    assert_eq!(last7.temporal.map(|t| (t.start, t.end)), Some((85_795_200, 86_400_000)));
}

// 6 ---------------------------------------------------------------------------------------------
#[test]
fn no_temporal_words_yields_none() {
    assert_eq!(analyze_query("rust async runtime", NOW, 0).temporal, None);
}

// 7 ---------------------------------------------------------------------------------------------
#[test]
fn multihop_feature_is_higher_for_multiclause() {
    let multi = analyze_query(MULTI, NOW, 0);
    let single = analyze_query("apple", NOW, 0);
    assert!(
        multi.intent.multihop > single.intent.multihop,
        "multihop {} should exceed {}",
        multi.intent.multihop,
        single.intent.multihop
    );
}

// 8 ---------------------------------------------------------------------------------------------
#[test]
fn ambiguous_single_term_has_lower_direct_than_phrase() {
    let single = analyze_query("apple", NOW, 0);
    let phrase = analyze_query("\"error code\" handling", NOW, 0);
    assert!(
        single.intent.direct < phrase.intent.direct,
        "direct {} should be below {}",
        single.intent.direct,
        phrase.intent.direct
    );
    assert!(single.intent.ambiguous >= phrase.intent.ambiguous);
}

// 9 ---------------------------------------------------------------------------------------------
#[test]
fn decompose_simple_query_is_one_unit() {
    let a = analyze_query("rust async runtime", NOW, 0);
    let units = decompose(&a);
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].id, "U1");
    assert_eq!(units[0].text, "rust async runtime");
    assert!(units[0].depends_on.is_empty());
    assert!(units[0].mandatory);
}

// 10 --------------------------------------------------------------------------------------------
#[test]
fn decompose_multiclause_example() {
    let a = analyze_query(MULTI, NOW, 0);
    let units = decompose(&a);
    assert!(units.len() >= 3, "expected >=3 units, got {}", units.len());
    assert!(units.len() <= MAX_UNITS);
    // Every unit is a requirement here (no preference wording), so all are mandatory.
    assert!(units.iter().all(|u| u.mandatory), "all units mandatory: {:?}", units);
    assert!(units.iter().all(|u| u.weight == 1.0));
    // At least one explicit dependency edge into an earlier unit.
    assert!(
        units.iter().any(|u| !u.depends_on.is_empty()),
        "expected an explicit depends_on edge: {:?}",
        units
    );
    // Ids are stable and ordered.
    for (i, u) in units.iter().enumerate() {
        assert_eq!(u.id, format!("U{}", i + 1));
    }
}

// 11 --------------------------------------------------------------------------------------------
#[test]
fn decompose_never_exceeds_max_units() {
    let mut q = String::from("a");
    for c in 'b'..='t' {
        q.push_str(" and ");
        q.push(c);
    }
    // 20 single-word clauses.
    let a = analyze_query(&q, NOW, 0);
    let units = decompose(&a);
    assert_eq!(units.len(), MAX_UNITS);
    assert!(units.len() <= MAX_UNITS);
    assert_eq!(units.last().map(|u| u.id.as_str()), Some("U8"));
}

// 12 --------------------------------------------------------------------------------------------
#[test]
fn link_entities_clear_winner_and_near_tie() {
    let mut priors = AliasPriors::empty();
    priors.insert("apple", "Apple Inc.", 0.9);
    priors.insert("jaguar", "Jaguar Cars", 0.6);
    priors.insert("jaguar", "Jaguar Animal", 0.5);

    let m = |t: &str| EntityMention {
        text: t.to_string(),
        start: 0,
        end: t.len(),
    };
    let hyps = link_entities(&[m("Apple"), m("Jaguar")], &priors);
    assert_eq!(hyps.len(), 2);

    // Clear winner (0.9 vs 0.0 unknown).
    assert_eq!(hyps[0].chosen.as_deref(), Some("Apple Inc."));
    // Unknown/other is always retained.
    assert!(hyps[0].candidates.iter().any(|(e, _)| e == "unknown/other"));

    // 0.6 vs 0.5 is within 0.15 -> no chosen.
    assert_eq!(hyps[1].chosen, None);
    // Candidates ordered by descending prior.
    let ps: Vec<f32> = hyps[1].candidates.iter().map(|(_, p)| *p).collect();
    assert!(ps.windows(2).all(|w| w[0] >= w[1]));
}

// 13 --------------------------------------------------------------------------------------------
#[test]
fn operator_predicates_and_language_guess() {
    let a = analyze_query(
        "rust site:example.com lang:en after:2020-01-01",
        NOW,
        0,
    );
    assert!(a
        .hard_predicates
        .iter()
        .any(|p| p.kind == PredicateKind::Domain && p.value == "example.com"));
    assert!(a
        .hard_predicates
        .iter()
        .any(|p| p.kind == PredicateKind::Language && p.value == "en"));
    assert!(a
        .hard_predicates
        .iter()
        .any(|p| p.kind == PredicateKind::DateAfter && p.value == "2020-01-01"));

    assert_eq!(analyze_query("the history of rust", NOW, 0).language, Some("en".into()));
}
