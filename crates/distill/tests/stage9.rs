//! Stage-9 exit-gate tests for `search-distill` (spec `docs/STAGE9_SPEC.md`).
//!
//! Distillation trains from the committed trace fixture and must report the **actual measured
//! accuracy** (even below 1.0), deterministically; the artifact-backed path must be honestly
//! unavailable.

use std::path::PathBuf;

use search_distill::{distill_from_artifact, distill_from_trace, DistillError};

fn trace_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../datasets/fixtures/distill/policy_trace.jsonl")
}

#[test]
fn distill_from_trace_reports_measured_accuracy() {
    let path = trace_fixture();
    let acc = distill_from_trace(&path).expect("committed trace fixture distills");
    println!("MEASURED distill_from_trace accuracy on fixture = {acc:.4}");
    assert!((0.0..=1.0).contains(&acc), "accuracy must be a probability");
    assert!(
        acc > 0.5,
        "linear policy should beat chance on the fixture, got {acc}"
    );
    // Deterministic: a second run yields the identical number.
    let again = distill_from_trace(&path).unwrap();
    assert_eq!(acc, again, "training must be deterministic");
}

#[test]
fn artifact_backed_path_is_honestly_unavailable() {
    let err = distill_from_artifact(&trace_fixture()).unwrap_err();
    assert!(matches!(err, DistillError::Unavailable(_)));
    println!("artifact path reported: {err}");
}
