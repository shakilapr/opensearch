//! Unit tests for §16 evidence matching (`crates/engine/src/evidence.rs`): the token-based,
//! punctuation-normalised title matcher and the injected `EvidenceVerifier` contract.
//!
//! Hermetic: no network, no clock, no I/O.

use search_engine::evidence::{normalize_tokens, title_supports_unit};
use search_engine::EvidenceVerifier;

#[test]
fn hyphen_and_punctuation_are_not_significant() {
    // The original defect: a case-insensitive *substring* test failed because the title
    // punctuated the phrase ("Retrieval-Augmented Generation"). Token matching must ignore the
    // hyphen (and case, and any other non-alphanumeric character).
    assert!(title_supports_unit(
        "retrieval augmented generation",
        "Retrieval-Augmented Generation: A Survey"
    ));
    assert!(title_supports_unit("error code E0425", "E0425 - Error Code [fixed]"));
    // A title that shares only some tokens does not support a short unit.
    assert!(!title_supports_unit(
        "retrieval augmented generation",
        "Retrieval-Augmented Retrieval"
    ));
}

#[test]
fn short_units_require_all_tokens_long_units_eighty_percent() {
    // <= 4 tokens: every token must appear.
    assert!(!title_supports_unit("rust async runtime", "rust async")); // 2 of 3
    assert!(title_supports_unit("rust async runtime", "the rust async runtime guide"));

    // > 4 tokens: at least 80% (ceil) of the tokens must appear.
    // 5 tokens -> need ceil(4.0) = 4.
    assert!(title_supports_unit("one two three four five", "one two three four"));
    assert!(!title_supports_unit("one two three four five", "one two three"));
    // 6 tokens -> need ceil(4.8) = 5.
    assert!(title_supports_unit("one two three four five six", "one two three four five"));
    assert!(!title_supports_unit("one two three four five six", "one two three four"));
}

#[test]
fn empty_and_punctuation_only_units_support_nothing() {
    assert!(!title_supports_unit("", "anything"));
    assert!(!title_supports_unit("   ... ,,, --- ", "anything"));
    assert!(!title_supports_unit("!!!", "!!!"));
}

#[test]
fn very_long_units_are_capped_and_bounded() {
    // 20 tokens; the matcher considers only the first 15 and requires ceil(0.8*15) = 12.
    let long = "t1 t2 t3 t4 t5 t6 t7 t8 t9 t10 t11 t12 t13 t14 t15 t16 t17 t18 t19 t20";
    assert!(title_supports_unit(long, "t1 t2 t3 t4 t5 t6 t7 t8 t9 t10 t11 t12"));
    assert!(!title_supports_unit(long, "t1 t2 t3 t4 t5 t6 t7 t8 t9 t10 t11"));
    // Tokens beyond the cap alone cannot satisfy the match.
    assert!(!title_supports_unit(long, "t16 t17 t18 t19 t20"));
}

#[test]
fn normalisation_maps_non_alphanumeric_to_spaces() {
    assert_eq!(
        normalize_tokens("Retrieval-Augmented  GENERATION!"),
        vec!["retrieval", "augmented", "generation"]
    );
    // Unicode alphanumerics are preserved (only lowercased).
    assert_eq!(normalize_tokens("café/résumé"), vec!["café", "résumé"]);
    assert!(normalize_tokens("  ,;.  ").is_empty());
}

#[test]
fn evidence_verifier_is_object_safe() {
    struct Yes;
    impl EvidenceVerifier for Yes {
        fn supports(&self, unit_text: &str, doc_id: &str) -> bool {
            unit_text == "q" && doc_id == "d"
        }
    }
    struct No;
    impl EvidenceVerifier for No {
        fn supports(&self, _unit_text: &str, _doc_id: &str) -> bool {
            false
        }
    }

    let boxed: Box<dyn EvidenceVerifier> = Box::new(Yes);
    assert!(boxed.supports("q", "d"));
    assert!(!boxed.supports("q", "other"));

    let borrowed: &dyn EvidenceVerifier = &No;
    assert!(!borrowed.supports("q", "d"));
}
