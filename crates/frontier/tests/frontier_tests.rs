//! Frontier integration tests (spec §5, §10 fault items 5 & 7).
//!
//! Each test gets its own tempfile SQLite database — no network, no secrets.

use std::path::PathBuf;
use std::sync::{Arc, Barrier};

use search_core::AppError;
use search_frontier::{Frontier, FrontierConfig, FrontierEntry, Priority};

fn tmp_frontier() -> (tempfile::TempDir, PathBuf, Frontier) {
    tmp_frontier_cfg(FrontierConfig::default())
}

fn tmp_frontier_cfg(cfg: FrontierConfig) -> (tempfile::TempDir, PathBuf, Frontier) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("meta.db");
    let f = Frontier::with_config(db.clone(), cfg);
    (dir, db, f)
}

fn entry(rid: &str, domain: &str, prio: Priority, at: i64) -> FrontierEntry {
    FrontierEntry::new(rid, format!("http://{domain}/{rid}"), domain, prio, at)
}

/// Read one scalar column for a resource from a fresh connection.
fn read_i64(db: &PathBuf, rid: &str, col: &str) -> i64 {
    let conn = search_metadata::writer::open(db).expect("open");
    conn.query_row(
        &format!("SELECT {col} FROM frontier_entry WHERE resource_id = ?1"),
        rusqlite::params![rid],
        |r| r.get(0),
    )
    .expect("row")
}

fn read_str(db: &PathBuf, rid: &str, col: &str) -> String {
    let conn = search_metadata::writer::open(db).expect("open");
    conn.query_row(
        &format!("SELECT {col} FROM frontier_entry WHERE resource_id = ?1"),
        rusqlite::params![rid],
        |r| r.get(0),
    )
    .expect("row")
}

// ── (a) two concurrent lease() calls never return the same resource_id ─────────

#[test]
fn concurrent_leases_never_duplicate() {
    let (_dir, _db, f) = tmp_frontier();
    let f = Arc::new(f);
    let n_rows = 100;
    for i in 0..n_rows {
        f.push(entry(&format!("r{i}"), "example.com", Priority::Normal, 0))
            .unwrap();
    }
    f.set_origin_concurrency("example.com", n_rows as i64).unwrap();

    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for w in 0..2 {
        let f = Arc::clone(&f);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            f.lease(&format!("worker-{w}"), 1_000, n_rows).unwrap()
        }));
    }
    let mut ids: Vec<String> = Vec::new();
    for h in handles {
        for l in h.join().unwrap() {
            ids.push(l.resource_id);
        }
    }
    assert_eq!(ids.len(), n_rows, "every row dispatched exactly once");
    let unique: std::collections::HashSet<_> = ids.iter().cloned().collect();
    assert_eq!(unique.len(), n_rows, "no resource_id leased twice");
}

// ── (b) expire then release_expired makes the row leasable again ───────────────

#[test]
fn expired_lease_is_recovered_and_releasable() {
    let cfg = FrontierConfig {
        lease_ttl: 10,
        ..FrontierConfig::default()
    };
    let (_dir, db, f) = tmp_frontier_cfg(cfg);
    f.push(entry("zombie", "d.com", Priority::Normal, 0))
        .unwrap();

    let leased = f.lease("w1", 1_000, 1).unwrap();
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].lease_until, 1_010);

    // While the lease is live, a second worker gets nothing.
    assert!(f.lease("w2", 1_005, 1).unwrap().is_empty());

    // After expiry, reclaim; then it is leasable again.
    let reclaimed = f.release_expired(1_011).unwrap();
    assert_eq!(reclaimed, 1);
    assert_eq!(read_str(&db, "zombie", "status"), "READY");

    let again = f.lease("w2", 1_011, 1).unwrap();
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].resource_id, "zombie");
}

// ── (c) fencing: complete() with a stale epoch is rejected ─────────────────────

#[test]
fn stale_epoch_is_fenced() {
    let cfg = FrontierConfig {
        lease_ttl: 10,
        ..FrontierConfig::default()
    };
    let (_dir, db, f) = tmp_frontier_cfg(cfg);
    f.push(entry("r1", "d.com", Priority::Normal, 0)).unwrap();

    let first = f.lease("w1", 1_000, 1).unwrap().remove(0);
    assert_eq!(first.epoch, 1);

    // Zombie is reclaimed and the row re-dispatched: epoch advances to 2.
    assert_eq!(f.release_expired(1_011).unwrap(), 1);
    let second = f.lease("w2", 1_011, 1).unwrap().remove(0);
    assert_eq!(second.epoch, 2);

    // The original (alive but stale) worker tries to commit -> fenced.
    let err = f.complete(&first, 1_012).unwrap_err();
    assert!(
        matches!(err, AppError::LeaseFenced(_)),
        "expected LeaseFenced, got {err:?}"
    );

    // The current lease commits cleanly.
    f.complete(&second, 1_012).unwrap();
    assert_eq!(read_str(&db, "r1", "status"), "DONE");
}

#[test]
fn wrong_owner_is_fenced() {
    let (_dir, _db, f) = tmp_frontier();
    f.push(entry("r1", "d.com", Priority::Normal, 0)).unwrap();
    let l = f.lease("w1", 1_000, 1).unwrap().remove(0);
    let forged = search_frontier::Lease {
        owner: "impostor".into(),
        ..l.clone()
    };
    let err = f.complete(&forged, 1_000).unwrap_err();
    assert!(matches!(err, AppError::LeaseFenced(_)));
}

// ── (d) backoff grows with attempt and is capped ──────────────────────────────

#[test]
fn backoff_grows_and_is_capped() {
    let cfg = FrontierConfig {
        backoff_base: 5,
        backoff_cap: 1_000,
        ..FrontierConfig::default()
    };
    let (_dir, _db, f) = tmp_frontier_cfg(cfg);

    assert_eq!(f.backoff_secs(0), 5);
    assert_eq!(f.backoff_secs(1), 10);
    assert_eq!(f.backoff_secs(2), 20);
    assert!(f.backoff_secs(0) < f.backoff_secs(1));
    assert!(f.backoff_secs(1) < f.backoff_secs(2));
    assert_eq!(f.backoff_secs(100), 1_000, "capped at backoff_cap");
    assert_eq!(f.backoff_secs(7), 640);
    assert_eq!(f.backoff_secs(8), 1_000, "5*2^8=1280 -> capped to 1000");
}

#[test]
fn retryable_fail_schedules_growing_backoff() {
    let cfg = FrontierConfig {
        backoff_base: 5,
        backoff_cap: 1_000,
        lease_ttl: 10,
        ..FrontierConfig::default()
    };
    let (_dir, db, f) = tmp_frontier_cfg(cfg);
    f.push(entry("r1", "d.com", Priority::Normal, 0)).unwrap();

    // attempt becomes 1 on the first lease.
    let l1 = f.lease("w1", 1_000, 1).unwrap().remove(0);
    assert_eq!(l1.attempt, 1);
    f.fail(&l1, 1_000, true).unwrap();
    let first_next = read_i64(&db, "r1", "earliest_fetch_at");
    assert_eq!(first_next, 1_000 + f.backoff_secs(1));
    assert_eq!(read_str(&db, "r1", "status"), "READY");

    // attempt 2 -> larger backoff.
    let l2 = f.lease("w1", first_next, 1).unwrap().remove(0);
    assert_eq!(l2.attempt, 2);
    f.fail(&l2, first_next, true).unwrap();
    let second_next = read_i64(&db, "r1", "earliest_fetch_at");
    assert_eq!(second_next, first_next + f.backoff_secs(2));
    assert!(
        second_next - first_next > first_next - 1_000,
        "backoff grew with attempt"
    );

    // Non-retryable is terminal.
    let l3 = f.lease("w1", second_next, 1).unwrap().remove(0);
    f.fail(&l3, second_next, false).unwrap();
    assert_eq!(read_str(&db, "r1", "status"), "FAILED");
}

// ── (e) per-origin concurrency is never exceeded ──────────────────────────────

#[test]
fn per_origin_concurrency_is_respected() {
    let (_dir, _db, f) = tmp_frontier();
    for i in 0..10 {
        f.push(entry(&format!("a{i}"), "a.com", Priority::Normal, 0))
            .unwrap();
    }
    for i in 0..10 {
        f.push(entry(&format!("b{i}"), "b.com", Priority::Normal, 0))
            .unwrap();
    }
    f.set_origin_concurrency("a.com", 3).unwrap();
    f.set_origin_concurrency("b.com", 2).unwrap();

    let batch = f.lease("w1", 1_000, 50).unwrap();
    let a = batch.iter().filter(|l| l.resource_id.starts_with('a')).count();
    let b = batch.iter().filter(|l| l.resource_id.starts_with('b')).count();
    assert_eq!(a, 3, "a.com capped at concurrency 3");
    assert_eq!(b, 2, "b.com capped at concurrency 2");

    // Nothing more until a lease is released.
    assert!(f.lease("w2", 1_000, 50).unwrap().is_empty());

    // Completing one a.com lease frees exactly one slot.
    let one_a = batch
        .iter()
        .find(|l| l.resource_id.starts_with('a'))
        .unwrap();
    f.complete(one_a, 1_000).unwrap();
    let next = f.lease("w2", 1_000, 50).unwrap();
    assert_eq!(next.len(), 1);
    assert!(next[0].resource_id.starts_with('a'));
}

#[test]
fn priority_orders_dispatch() {
    let (_dir, _db, f) = tmp_frontier();
    f.push(entry("backfill", "d.com", Priority::Backfill, 0))
        .unwrap();
    f.push(entry("critical", "d.com", Priority::QueryCritical, 0))
        .unwrap();
    f.push(entry("normal", "d.com", Priority::Normal, 0))
        .unwrap();
    f.set_origin_concurrency("d.com", 10).unwrap();

    let first = f.lease("w1", 1_000, 1).unwrap();
    assert_eq!(first[0].resource_id, "critical");
}

#[test]
fn polite_delay_defers_dispatch() {
    let (_dir, _db, f) = tmp_frontier();
    f.push(entry("late", "d.com", Priority::Normal, 10_000))
        .unwrap();
    assert!(f.lease("w1", 9_999, 1).unwrap().is_empty());
    assert_eq!(f.lease("w1", 10_000, 1).unwrap().len(), 1);
}
