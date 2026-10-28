use search_frontier::{Frontier, FrontierEntry, Priority};


// ---------------------------------------------------------------------------
// Batched push
// ---------------------------------------------------------------------------

/// `push_many` must be exactly `push` repeated: same rows, same conflict behaviour, and a
/// re-push of a known URL updates rather than duplicating.
///
/// It exists for throughput — one autocommit INSERT per discovered URL was ~80% of a crawl
/// process's CPU — so the only thing that can go wrong is behaviour drift.
#[test]
fn push_many_matches_push_and_stays_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("meta.sqlite");
    let f = Frontier::new(&path);

    let batch: Vec<FrontierEntry> = (0..50)
        .map(|i| {
            FrontierEntry::new(
                &format!("res-{i}"),
                format!("https://example.test/{i}"),
                "example.test",
                Priority::Normal,
                0,
            )
        })
        .collect();

    let pushed = f.push_many(&batch).unwrap();
    assert_eq!(pushed, 50);

    // Re-pushing the whole batch changes nothing: the conflict clause updates in place.
    assert_eq!(f.push_many(&batch).unwrap(), 50);

    // A single push of a new entry lands the same way push_many's rows did.
    f.push(FrontierEntry::new(
        "res-solo",
        "https://example.test/solo",
        "example.test",
        Priority::Normal,
        0,
    ))
    .unwrap();

    let leased = f.lease("worker", 1_000, 100).unwrap();
    assert_eq!(
        leased.len(),
        51,
        "50 batched + 1 single, no duplicates from the re-push"
    );
}

/// An empty batch is a no-op, not an error: discovery often finds nothing.
#[test]
fn push_many_of_nothing_is_fine() {
    let dir = tempfile::tempdir().unwrap();
    let f = Frontier::new(dir.path().join("meta.sqlite"));
    assert_eq!(f.push_many(&[]).unwrap(), 0);
    assert!(f.lease("worker", 1, 10).unwrap().is_empty());
}
