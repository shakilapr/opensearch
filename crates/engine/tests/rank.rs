//! §10 + §16 hermetic, deterministic tests for `search-engine`.
//! OWNER: engine-rank agent. No network, no clock, no filesystem.

use search_engine::evidence::corroborated_units;
use search_engine::fuse::fuse_families;
use search_engine::{
    diversify, evidence_coverage, fuse, independent_origins, run_cascade, CascadeInput,
    DiversityConfig, EvidenceRecord, Fused, FusionPolicy, LaneHits, LaneId, Scored, Support,
    UnitKind, QueryUnit,
};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn policy(c: f64, cap: f64, weights: &[(LaneId, f64)]) -> FusionPolicy {
    FusionPolicy { c, weights: weights.iter().copied().collect(), variant_family_cap: cap }
}

fn lane(l: LaneId, docs: &[&str]) -> LaneHits {
    LaneHits { lane: l, ranked: docs.iter().map(|s| s.to_string()).collect() }
}

fn unit(id: &str, mandatory: bool, weight: f32) -> QueryUnit {
    QueryUnit {
        id: id.to_string(),
        text: id.to_string(),
        depends_on: Vec::new(),
        mandatory,
        weight,
        kind: UnitKind::Other,
    }
}

fn rec(unit_id: &str, doc_id: &str, status: Support) -> EvidenceRecord {
    EvidenceRecord {
        unit_id: unit_id.to_string(),
        doc_id: doc_id.to_string(),
        status,
        span: None,
        doc_version: "v1".to_string(),
    }
}

fn scored(doc_id: &str, relevance: f32, cluster: Option<&str>, domain: &str) -> Scored {
    Scored {
        doc_id: doc_id.to_string(),
        relevance,
        uncovered_gain: 0.0,
        cluster: cluster.map(|s| s.to_string()),
        domain: domain.to_string(),
    }
}

// ---------------------------------------------------------------------------
// §10 RRF
// ---------------------------------------------------------------------------

#[test]
fn rrf_single_lane_strictly_decreasing() {
    let p = policy(0.0, 2.0, &[(LaneId::Bm25, 1.0)]);
    let out = fuse(&[lane(LaneId::Bm25, &["a", "b", "c", "d"])], &p);
    let ids: Vec<&str> = out.iter().map(|f| f.doc_id.as_str()).collect();
    assert_eq!(ids, ["a", "b", "c", "d"]);
    for w in out.windows(2) {
        assert!(w[1].score < w[0].score, "scores must strictly decrease");
    }
    assert!((out[0].score - 1.0).abs() < 1e-9);
    assert!((out[3].score - 0.25).abs() < 1e-9);
}

#[test]
fn rrf_two_lanes_beats_single_high_weight_lane() {
    // Bm25 carries more weight; a doc in BOTH lanes still outranks a doc present only in the
    // higher-weighted lane.
    let p = policy(0.0, 2.0, &[(LaneId::Bm25, 1.5), (LaneId::SourceFirst, 1.0)]);
    let out = fuse(
        &[
            lane(LaneId::Bm25, &["single", "both"]),
            lane(LaneId::SourceFirst, &["both"]),
        ],
        &p,
    );
    // single = 1.5 ; both = 1.5/2 + 1.0/1 = 1.75
    assert_eq!(out[0].doc_id, "both");
    assert!(out[0].score > out[1].score);
}

#[test]
fn rrf_unknown_lane_contributes_nothing() {
    // SourceFirst is absent from the weight map -> weight 0 -> no contribution at all.
    let p = policy(0.0, 2.0, &[(LaneId::Bm25, 1.0)]);
    let out = fuse(&[lane(LaneId::SourceFirst, &["a", "b"])], &p);
    assert!(out.is_empty());
}

#[test]
fn rrf_empty_input_empty_output() {
    let p = policy(0.0, 2.0, &[(LaneId::Bm25, 1.0)]);
    assert!(fuse(&[], &p).is_empty());
    // Lanes present but all empty also yields empty output.
    assert!(fuse(&[lane(LaneId::Bm25, &[])], &p).is_empty());
}

#[test]
fn rrf_deterministic_two_calls_identical() {
    let p = policy(30.0, 2.0, &[(LaneId::Bm25, 1.0), (LaneId::SourceFirst, 0.7)]);
    let lanes = [
        lane(LaneId::Bm25, &["d", "b", "a", "c"]),
        lane(LaneId::SourceFirst, &["c", "a", "e"]),
    ];
    let first = fuse(&lanes, &p);
    let second = fuse(&lanes, &p);
    assert_eq!(first, second, "identical input must give identical Vec ordering");
}

#[test]
fn rrf_tie_break_doc_id_ascending() {
    // Equal lane weights, c = 0: "b" (Bm25 rank 1) and "a" (SourceFirst rank 1) tie at 1.0.
    let p = policy(0.0, 2.0, &[(LaneId::Bm25, 1.0), (LaneId::SourceFirst, 1.0)]);
    let out = fuse(
        &[lane(LaneId::Bm25, &["b"]), lane(LaneId::SourceFirst, &["a"])],
        &p,
    );
    assert!((out[0].score - out[1].score).abs() < 1e-12, "scores must tie");
    let ids: Vec<&str> = out.iter().map(|f| f.doc_id.as_str()).collect();
    assert_eq!(ids, ["a", "b"], "tie broken by doc_id ascending");
}

// ---------------------------------------------------------------------------
// §10 variant-family cap
// ---------------------------------------------------------------------------

#[test]
fn fuse_families_caps_rewrite_family_below_original() {
    // Original family (id 0) has one lane with one doc. The rewrite family (id 1) has 5
    // near-identical variants, each ranking the same doc first.
    let p = policy(0.0, 0.5, &[(LaneId::Bm25, 1.0)]);
    let original_lanes = [lane(LaneId::Bm25, &["orig"])];
    let variant_lanes = [
        lane(LaneId::Bm25, &["rewrite"]),
        lane(LaneId::Bm25, &["rewrite"]),
        lane(LaneId::Bm25, &["rewrite"]),
        lane(LaneId::Bm25, &["rewrite"]),
        lane(LaneId::Bm25, &["rewrite"]),
    ];

    // Uncapped, the rewrite family would outvote the original 5.0 vs 1.0.
    let uncapped = fuse(&variant_lanes, &p);
    assert!(uncapped[0].score > 1.0);

    let families: [(u8, &[LaneHits]); 2] =
        [(0u8, original_lanes.as_slice()), (1u8, variant_lanes.as_slice())];
    let out = fuse_families(&families, &p);

    assert_eq!(out[0].doc_id, "orig", "the original query's top doc must still win");
    assert!(out[0].score > out[1].score);
    // The rewrite family's total mass is capped at cap * original_total = 0.5 * 1.0 = 0.5.
    let rewrite = out.iter().find(|f| f.doc_id == "rewrite").expect("rewrite present");
    assert!((rewrite.score - 0.5).abs() < 1e-9);
}

// ---------------------------------------------------------------------------
// §10 cascade
// ---------------------------------------------------------------------------

fn fused(docs: &[&str]) -> Vec<Fused> {
    docs.iter()
        .enumerate()
        .map(|(i, d)| Fused { doc_id: d.to_string(), score: 1.0 - i as f64 * 0.1 })
        .collect()
}

#[test]
fn cascade_stage_sizes_monotone_and_five_entries() {
    let input = CascadeInput {
        fused: fused(&["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"]),
        feature_cap: 8,
        cross_encoder_cap: 5,
        verify_cap: 3,
        final_k: 2,
    };
    let out = run_cascade(input);
    assert_eq!(out.stage_sizes, vec![10, 8, 5, 3, 2]);
    assert_eq!(out.stage_sizes.len(), 5);
    for w in out.stage_sizes.windows(2) {
        assert!(w[1] <= w[0], "stage sizes must be monotone non-increasing");
    }
}

#[test]
fn cascade_final_le_final_k_and_order_preserved() {
    let input = CascadeInput {
        fused: fused(&["a", "b", "c", "d"]),
        feature_cap: 100,
        cross_encoder_cap: 100,
        verify_cap: 100,
        final_k: 2,
    };
    let out = run_cascade(input);
    assert!(out.ranked.len() <= 2);
    assert_eq!(out.ranked.len(), 2);
    // Input order is preserved.
    let ids: Vec<&str> = out.ranked.iter().map(|f| f.doc_id.as_str()).collect();
    assert_eq!(ids, ["a", "b"]);

    // A stage cap above the input size never increases the count.
    let wide = CascadeInput {
        fused: fused(&["x", "y", "z"]),
        feature_cap: 50,
        cross_encoder_cap: 50,
        verify_cap: 50,
        final_k: 50,
    };
    let out2 = run_cascade(wide);
    assert_eq!(out2.stage_sizes, vec![3, 3, 3, 3, 3]);
    assert_eq!(out2.ranked.len(), 3);
}

// ---------------------------------------------------------------------------
// §16 evidence coverage
// ---------------------------------------------------------------------------

#[test]
fn coverage_weighted_fraction_three_units() {
    let units = [unit("U1", false, 2.0), unit("U2", false, 1.0), unit("U3", false, 1.0)];
    let records = [
        rec("U1", "d1", Support::Supported),
        rec("U2", "d2", Support::Supported),
        // U3 has no evidence.
    ];
    let cov = evidence_coverage(&units, &records);
    // (2 + 1) / 4 = 0.75
    assert!((cov.coverage - 0.75).abs() < 1e-6);
    assert!(cov.mandatory_ok);
    assert!(cov.unsupported.is_empty());
    assert!(cov.contradicted_units.is_empty());
}

#[test]
fn coverage_contradicted_only_unit_not_supported() {
    let units = [unit("U1", false, 1.0), unit("U2", false, 1.0)];
    let records = [
        rec("U1", "d1", Support::Contradicted),
        rec("U2", "d2", Support::Supported),
    ];
    let cov = evidence_coverage(&units, &records);
    assert!((cov.coverage - 0.5).abs() < 1e-6, "a contradicted unit is not supported");
    assert_eq!(cov.contradicted_units, vec!["U1".to_string()]);
}

#[test]
fn coverage_both_supported_and_contradicted_is_supported_and_listed() {
    let units = [unit("U1", false, 1.0)];
    let records = [
        rec("U1", "d1", Support::Supported),
        rec("U1", "d2", Support::Contradicted),
    ];
    let cov = evidence_coverage(&units, &records);
    assert!((cov.coverage - 1.0).abs() < 1e-6, "supported anywhere wins");
    assert_eq!(cov.contradicted_units, vec!["U1".to_string()]);
    // Duplicate identical records must not double-count.
    let dup = [
        rec("U1", "d1", Support::Supported),
        rec("U1", "d1", Support::Supported),
    ];
    assert!((evidence_coverage(&units, &dup).coverage - 1.0).abs() < 1e-6);
}

#[test]
fn coverage_mandatory_unsupported_overrides_high_average() {
    // U1 is mandatory and unsupported; U2 carries most of the weight but is supported, so the
    // average is high. §16: "high average coverage never overrides an unsatisfied mandatory
    // predicate".
    let units = [unit("U1", true, 1.0), unit("U2", false, 9.0)];
    let records = [rec("U2", "d2", Support::Supported)];
    let cov = evidence_coverage(&units, &records);
    assert!(cov.coverage >= 0.9, "average coverage is high");
    assert!(!cov.mandatory_ok, "mandatory gate fails independently of coverage");
    assert_eq!(cov.unsupported, vec!["U1".to_string()]);

    // Empty inputs: coverage 0.0, mandatory_ok true (no mandatory units).
    let empty = evidence_coverage(&[], &[]);
    assert!((empty.coverage - 0.0).abs() < 1e-9);
    assert!(empty.mandatory_ok);
}

// ---------------------------------------------------------------------------
// §16 origins and corroboration
// ---------------------------------------------------------------------------

#[test]
fn independent_origins_counts_distinct_origins() {
    let records = [
        rec("U1", "d1", Support::Supported),
        rec("U1", "d2", Support::Supported),
        rec("U1", "d3", Support::Supported),
        rec("U2", "d4", Support::Supported),
        rec("U2", "d5", Support::Supported),
    ];
    let origin = |doc: &str| match doc {
        "d1" | "d2" | "d3" => "wire".to_string(),
        _ => "blog".to_string(),
    };
    assert_eq!(independent_origins(&records, &origin), 2);
}

#[test]
fn corroborated_units_requires_two_independent_origins() {
    let units = [unit("U1", false, 1.0), unit("U2", false, 1.0)];

    // U1: 3 supported records, all from one origin -> not corroboration.
    let one_origin = [
        rec("U1", "a", Support::Supported),
        rec("U1", "b", Support::Supported),
        rec("U1", "c", Support::Supported),
    ];
    let same = |_: &str| "wire".to_string();
    assert_eq!(corroborated_units(&units, &one_origin, &same), 0);

    // U2: 2 supported records from 2 distinct origins -> corroborated.
    let two_origins = [
        rec("U2", "a", Support::Supported),
        rec("U2", "b", Support::Supported),
    ];
    let distinct = |doc: &str| if doc == "a" { "wire".to_string() } else { "blog".to_string() };
    assert_eq!(corroborated_units(&units, &two_origins, &distinct), 1);
}

// ---------------------------------------------------------------------------
// §16 diversification
// ---------------------------------------------------------------------------

#[test]
fn diversify_removes_redundant_duplicate_story() {
    let cfg = DiversityConfig { redundancy_weight: 1.0, uncovered_weight: 0.0, max_per_domain: 5 };
    let cands = vec![
        scored("a", 1.0, Some("story"), "d1"),
        scored("b", 0.9, Some("story"), "d2"), // same story as a -> redundant
        scored("c", 0.5, Some("other"), "d3"),
    ];
    let out = diversify(cands, 2, &cfg);
    let ids: Vec<&str> = out.iter().map(|s| s.doc_id.as_str()).collect();
    assert_eq!(ids, ["a", "c"], "the redundant duplicate story is skipped");
}

#[test]
fn diversify_respects_max_per_domain_with_other_domains() {
    let cfg = DiversityConfig { redundancy_weight: 1.0, uncovered_weight: 0.0, max_per_domain: 1 };
    let cands = vec![
        scored("x1", 0.9, None, "x"),
        scored("x2", 0.5, None, "x"),
        scored("y1", 0.8, None, "y"),
        scored("y2", 0.3, None, "y"),
    ];
    let out = diversify(cands, 2, &cfg);
    let ids: Vec<&str> = out.iter().map(|s| s.doc_id.as_str()).collect();
    assert_eq!(ids, ["x1", "y1"], "second pick comes from a fresh domain");
}

#[test]
fn diversify_single_domain_is_not_starved() {
    // Every candidate is from one domain and max_per_domain < k. Domain diversity is a soft
    // objective (§16) so the result set must not be starved: return min(k, available).
    let cfg = DiversityConfig { redundancy_weight: 1.0, uncovered_weight: 0.0, max_per_domain: 1 };
    let cands = vec![
        scored("a", 0.9, None, "only"),
        scored("b", 0.8, None, "only"),
        scored("c", 0.7, None, "only"),
    ];
    let out = diversify(cands, 3, &cfg);
    assert_eq!(out.len(), 3);
    let ids: Vec<&str> = out.iter().map(|s| s.doc_id.as_str()).collect();
    assert_eq!(ids, ["a", "b", "c"]);
}

#[test]
fn diversify_k1_returns_the_single_best() {
    let cfg = DiversityConfig::default();
    let cands = vec![
        scored("a", 0.2, None, "d1"),
        scored("b", 0.95, None, "d2"),
        scored("c", 0.5, None, "d3"),
    ];
    let out = diversify(cands, 1, &cfg);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].doc_id, "b");
}

#[test]
fn diversify_deterministic_and_no_duplicate_doc_ids() {
    let cfg = DiversityConfig::default();
    let make = || {
        vec![
            scored("a", 0.9, Some("s"), "d1"),
            scored("a", 0.9, Some("s"), "d1"), // literal duplicate doc_id
            scored("b", 0.8, Some("s"), "d1"),
            scored("c", 0.7, None, "d2"),
        ]
    };
    let first = diversify(make(), 4, &cfg);
    let second = diversify(make(), 4, &cfg);
    assert_eq!(first, second, "identical input must give identical ordering");
    let mut ids: Vec<&str> = first.iter().map(|s| s.doc_id.as_str()).collect();
    let len_before = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), len_before, "no doc_id may be returned twice");
}
