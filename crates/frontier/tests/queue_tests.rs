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

    // A lease moves the row to FETCHING, so counting the whole queue means draining it:
    // `default_concurrency` is 2 per origin, and each retained lease excludes its row from the
    // next call until it is completed.
    let mut leased = 0usize;
    for i in 0..128 {
        let batch = f.lease("worker", 1_000 + i, 100).unwrap();
        if batch.is_empty() {
            break;
        }
        leased += batch.len();
        for l in &batch {
            f.complete(l, 1_000 + i).unwrap();
        }
    }
    assert_eq!(
        leased, 51,
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

/// The lease query must use the dispatch index without a temp B-tree.
///
/// Measured on a 2.3M-row shard: 522 ms per lease call with `status IN (...)` planning through
/// `idx_frontier_dispatch` plus a temp B-tree, versus 0.4 ms through the partial
/// `idx_frontier_ready_dispatch`. The crawl leases in a loop, so this one plan was most of a
/// crawler process's CPU. A result-only test would not catch a regression — the rows are
/// identical either way — so this asserts the plan.
#[test]
fn the_lease_query_uses_the_partial_dispatch_index() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("meta.sqlite");
    let f = Frontier::new(&path);
    // Enough rows that the planner's choice is meaningful, then statistics.
    let batch: Vec<FrontierEntry> = (0..500)
        .map(|i| {
            FrontierEntry::new(
                &format!("res-{i}"),
                format!("https://e{i}.test/x"),
                &format!("e{i}.test"),
                Priority::Normal,
                0,
            )
        })
        .collect();
    f.push_many(&batch).unwrap();
    f.conn_analyze().expect("analyze");

    let plan = f.lease_query_plan(1_000, 64).expect("plan");
    assert!(
        plan.contains("idx_frontier_ready_dispatch"),
        "lease must use the partial dispatch index; plan was:\n{plan}"
    );
    assert!(
        !plan.to_ascii_uppercase().contains("TEMP B-TREE FOR ORDER BY"),
        "no temp B-tree sort for ORDER BY; plan was:\n{plan}"
    );
}
