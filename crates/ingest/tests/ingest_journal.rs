//! Idempotency / journal / watermark integration tests (spec §3, §10).
//!
//! Hermetic: each test gets its own tempfile SQLite database and never touches
//! the network or any shared path.

use search_core::{OpKey, ResourceId};
use search_ingest::journal::IngestStage;
use search_ingest::{ingest_once, journal_event_count, resume, IngestCoordinator, Watermark};
use search_metadata::{MetadataReader, MetadataWriter};
use std::path::PathBuf;

struct Db {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Db {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("meta.sqlite");
        Db { _dir: dir, path }
    }

    fn writer(&self) -> MetadataWriter {
        MetadataWriter::from_path(&self.path).expect("open writer")
    }

    fn reader(&self) -> MetadataReader {
        MetadataReader::from_path(&self.path).expect("open reader")
    }
}

fn op_key() -> OpKey {
    OpKey::from_parts(&ResourceId::new("res_test_1"), 1, 1)
}

/// (a) full walk FETCHED..=VISIBLE yields state Visible and exactly 7 events.
#[test]
fn full_walk_reaches_visible_with_seven_events() {
    let db = Db::new();
    let mut w = db.writer();
    let r = db.reader();
    let key = op_key();

    let state = ingest_once(&mut w, &r, &key, "res_test_1").expect("ingest_once");
    assert_eq!(state.stage, IngestStage::Visible);
    assert_eq!(state.stage.as_str(), "VISIBLE");
    assert_eq!(journal_event_count(&r, &key).unwrap(), 7);

    // The projection and the journal replay agree.
    let view = IngestCoordinator::state_view(&r, &key).unwrap();
    assert!(view.agree(), "projected and replayed states diverged");
    assert_eq!(view.projected.unwrap().stage, IngestStage::Visible);
    assert_eq!(view.replayed.unwrap().stage, IngestStage::Visible);
}

/// (b) running the driver twice is idempotent: still 7 events, same state.
#[test]
fn ingest_once_is_idempotent_on_second_run() {
    let db = Db::new();
    let mut w = db.writer();
    let r = db.reader();
    let key = op_key();

    let first = ingest_once(&mut w, &r, &key, "res_test_1").expect("first run");
    let second = ingest_once(&mut w, &r, &key, "res_test_1").expect("second run");

    assert_eq!(first.stage, IngestStage::Visible);
    assert_eq!(second.stage, IngestStage::Visible);
    assert_eq!(
        journal_event_count(&r, &key).unwrap(),
        7,
        "second run must not append duplicate journal events"
    );
    let view = IngestCoordinator::state_view(&r, &key).unwrap();
    assert!(view.agree());
}

/// (c) backwards transitions are rejected; duplicate stages are silent no-ops.
#[test]
fn backwards_transition_is_rejected_and_duplicate_is_noop() {
    let db = Db::new();
    let mut w = db.writer();
    let r = db.reader();
    let key = op_key();

    // Jump straight to METADATA_COMMITTED (allowed: a resumed pipeline may
    // start partway through).
    IngestCoordinator::record(&mut w, &key, "res_test_1", IngestStage::MetadataCommitted)
        .unwrap();
    assert_eq!(journal_event_count(&r, &key).unwrap(), 1);

    // Re-applying the same stage is a no-op: no new event, Ok.
    IngestCoordinator::record(&mut w, &key, "res_test_1", IngestStage::MetadataCommitted)
        .unwrap();
    assert_eq!(journal_event_count(&r, &key).unwrap(), 1);

    // A strictly-lower, never-recorded stage is a backwards transition.
    let err = IngestCoordinator::record(&mut w, &key, "res_test_1", IngestStage::Normalized)
        .unwrap_err();
    assert!(
        matches!(err, search_core::AppError::Invalid(_)),
        "expected Invalid, got {err:?}"
    );
    // The journal was not moved backwards.
    assert_eq!(journal_event_count(&r, &key).unwrap(), 1);
    assert_eq!(
        IngestCoordinator::state(&r, &key).unwrap().unwrap().stage,
        IngestStage::MetadataCommitted
    );
}

/// (d) replayed state equals projected state after every recorded step.
#[test]
fn projected_and_replayed_agree_after_each_step() {
    let db = Db::new();
    let mut w = db.writer();
    let r = db.reader();
    let key = op_key();

    for stage in IngestStage::ALL {
        IngestCoordinator::record(&mut w, &key, "res_test_1", stage).unwrap();
        let view = IngestCoordinator::state_view(&r, &key).unwrap();
        assert!(view.agree(), "diverged after {stage:?}: {view:?}");
        assert_eq!(view.projected.as_ref().unwrap().stage, stage);
        assert_eq!(view.replayed.as_ref().unwrap().stage, stage);
    }

    // Re-walking every stage must not change either view.
    for stage in IngestStage::ALL {
        IngestCoordinator::record(&mut w, &key, "res_test_1", stage).unwrap();
        assert!(IngestCoordinator::state_view(&r, &key).unwrap().agree());
    }
    assert_eq!(journal_event_count(&r, &key).unwrap(), 7);
}

/// (e) resume() returns the furthest committed stage after a mid-walk crash.
#[test]
fn resume_returns_furthest_committed_stage_after_crash() {
    let db = Db::new();
    let mut w = db.writer();
    let r = db.reader();
    let key = op_key();

    // Simulate a crash after three stages.
    for stage in [IngestStage::Fetched, IngestStage::Normalized, IngestStage::BlobWritten] {
        IngestCoordinator::record(&mut w, &key, "res_test_1", stage).unwrap();
    }
    assert_eq!(resume(&r, &key).unwrap(), IngestStage::BlobWritten);

    // Resume: continue from the next stage. Re-recording committed stages is a
    // no-op, so a naive "walk from FETCHED again" converges without duplicates.
    let next = resume(&r, &key).unwrap().next().unwrap();
    assert_eq!(next, IngestStage::MetadataCommitted);
    assert_eq!(ingest_once(&mut w, &r, &key, "res_test_1").unwrap().stage, IngestStage::Visible);
    assert_eq!(journal_event_count(&r, &key).unwrap(), 7);
    assert_eq!(resume(&r, &key).unwrap(), IngestStage::Visible);

    // No progress -> NotFound.
    let fresh = OpKey::from_parts(&ResourceId::new("res_other"), 0, 0);
    assert!(resume(&r, &fresh).is_err());
}

/// Watermark gating: search exposes only `visible_generation <= published`.
#[test]
fn watermark_is_monotonic_and_gates_visibility() {
    let db = Db::new();
    // The DB is created by the writer; readers open read-only.
    let w = db.writer();
    let r = db.reader();

    // Nothing published yet: nothing is visible.
    assert_eq!(Watermark::visible(&r).unwrap(), 0);
    assert!(!Watermark::is_visible(1, Watermark::visible(&r).unwrap()));

    Watermark::publish(&w, 5).unwrap();
    assert_eq!(Watermark::visible(&r).unwrap(), 5);
    assert!(Watermark::is_visible(5, 5));
    assert!(Watermark::is_visible(4, 5));
    assert!(!Watermark::is_visible(6, 5));

    // Publishing a lower generation never lowers the watermark.
    Watermark::publish(&w, 3).unwrap();
    assert_eq!(Watermark::visible(&r).unwrap(), 5);

    Watermark::publish(&w, 9).unwrap();
    assert_eq!(Watermark::visible(&r).unwrap(), 9);

    // The publish path rejects lowering explicitly.
    assert!(Watermark::check_publish(8, 9).is_err());
    assert!(Watermark::check_publish(9, 9).is_ok());
    assert!(Watermark::check_publish(10, 9).is_ok());
}

/// Stage ordering helpers behave strictly.
#[test]
fn stage_order_helpers() {
    assert_eq!(IngestStage::Fetched.next(), Some(IngestStage::Normalized));
    assert_eq!(IngestStage::Visible.next(), None);
    assert_eq!(IngestStage::Fetched.ordinal(), 0);
    assert_eq!(IngestStage::Visible.ordinal(), 6);
    assert_eq!(IngestStage::parse("VISIBLE"), Some(IngestStage::Visible));
    assert_eq!(IngestStage::parse("bogus"), None);
}
