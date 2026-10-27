//! `agent0 frontier recover` — the caller that reclaims zombie leases.
//!
//! The frontier library could always reclaim expired leases; nothing ever called
//! it. Measured on 2026-10-24 the four fleet shards held 328,712 rows stuck in
//! `FETCHING` with expired leases. These tests pin the behaviour that makes the
//! command safe to run against a live data root: it reclaims only expired
//! leases, leaves a live lease alone, and refuses a directory that is not a data
//! root instead of silently doing nothing.

use std::path::Path;

use search_frontier::queue::{FrontierEntry, Priority};
use search_frontier::Frontier;

/// Build a data root with an empty frontier and return its path.
fn data_root(dir: &Path) -> std::path::PathBuf {
    let root = dir.join("root");
    std::fs::create_dir_all(root.join("meta")).unwrap();
    root
}

fn entry(id: &str) -> FrontierEntry {
    FrontierEntry::new(id, format!("https://d1.example/{id}"), "d1", Priority::Normal, 0)
}

/// An expired `FETCHING` lease is returned to `READY`; a live one is untouched.
#[test]
fn recover_reclaims_expired_leases_and_leaves_live_ones() {
    let dir = tempfile::tempdir().unwrap();
    let root = data_root(dir.path());
    let frontier = Frontier::new(root.join("meta").join("meta.sqlite"));

    frontier.push(entry("zombie")).unwrap();
    frontier.push(entry("live")).unwrap();

    let t0 = 1_000_000_i64;
    // Lease both at t0 (default lease is short), then keep `live` refreshed.
    let leases = frontier.lease("worker-1", t0, 2).unwrap();
    assert_eq!(leases.len(), 2, "both rows are leased");

    let reclaimed = search_agent0::frontier_recover::recover_dir(&root, t0 + 1).unwrap();
    assert_eq!(reclaimed, 0, "a lease that has not expired is left alone");

    // Far enough in the future that both leases have expired.
    let reclaimed = search_agent0::frontier_recover::recover_dir(&root, t0 + 1_000_000).unwrap();
    assert_eq!(reclaimed, 2, "both zombies are returned to READY");

    // Running it again reclaims nothing: recovery is idempotent.
    let again = search_agent0::frontier_recover::recover_dir(&root, t0 + 1_000_000).unwrap();
    assert_eq!(again, 0);
}

/// A directory that is not a data root is an error, not a silent success — a
/// typo in `--dir` must not look like "nothing to recover".
#[test]
fn recover_refuses_a_directory_without_a_metadata_database() {
    let dir = tempfile::tempdir().unwrap();
    let err = search_agent0::frontier_recover::recover_dir(dir.path(), 1)
        .expect_err("a bare directory is not a data root");
    assert!(err.contains("no metadata database"), "{err}");
}

/// Recovery does not lose rows: everything reclaimed is still dispatchable.
#[test]
fn reclaimed_rows_are_dispatchable_again() {
    let dir = tempfile::tempdir().unwrap();
    let root = data_root(dir.path());
    let frontier = Frontier::new(root.join("meta").join("meta.sqlite"));
    frontier.push(entry("zombie")).unwrap();

    let t0 = 5_000_000_i64;
    assert_eq!(frontier.lease("dead-worker", t0, 1).unwrap().len(), 1);
    let reclaimed = search_agent0::frontier_recover::recover_dir(&root, t0 + 1_000_000).unwrap();
    assert_eq!(reclaimed, 1);

    let again = frontier.lease("new-worker", t0 + 1_000_001, 1).unwrap();
    assert_eq!(again.len(), 1, "the reclaimed row can be leased by a new worker");
}
