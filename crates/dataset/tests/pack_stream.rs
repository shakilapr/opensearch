//! The incremental pack writer: what an interrupted export leaves behind, and what a re-run does.
//!
//! The behaviour under test is the one that made `agent0 export` unable to ever finish: records
//! must reach disk *before* the run ends, and a later run must continue the same pack rather than
//! start over.

use search_dataset::{validate_pack, PackWriter, Record};

fn rec(id: &str) -> Record {
    let mut r = Record::new(id.to_string(), "web");
    r.url = Some(format!("https://example.test/{id}"));
    r.text = format!("body of {id}");
    r
}

/// A checkpointed write is a *valid* pack even though the run never called `finish` — this is the
/// killed-by-timeout case: the work so far survives and validates.
#[test]
fn flushed_shards_form_a_valid_pack_without_finishing() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path();

    {
        let mut w = PackWriter::open(path, "web", "1", 2).unwrap();
        w.push(rec("a")).unwrap();
        w.push(rec("b")).unwrap(); // reaches shard_size -> written immediately
        w.push(rec("c")).unwrap();
        w.flush().unwrap(); // checkpoint: writes "c" too
        // no finish(): the writer is dropped, as a SIGTERM would drop it
    }

    let manifest = validate_pack(path).expect("an interrupted pack still validates");
    assert_eq!(manifest.records, 3);
    assert_eq!(manifest.shards.len(), 2, "two shards: [a,b] and [c]");
}

/// A re-run resumes: the ids already on disk come back, and appending continues the same pack.
#[test]
fn reopen_resumes_and_continues_the_same_pack() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path();

    {
        let mut w = PackWriter::open(path, "web", "1", 2).unwrap();
        w.push(rec("a")).unwrap();
        w.push(rec("b")).unwrap();
        w.flush().unwrap();
    }

    let mut w = PackWriter::open(path, "web", "1", 2).unwrap();
    let seen = w.existing_ids().unwrap();
    assert!(seen.contains("a") && seen.contains("b"), "resume keys: {seen:?}");
    assert_eq!(w.records(), 2, "the writer resumes with the existing total");

    // A page already exported is skipped by the caller; only the new one is pushed.
    assert!(!seen.contains("c"));
    w.push(rec("c")).unwrap();
    let manifest = w.finish().unwrap();

    assert_eq!(manifest.records, 3);
    assert_eq!(manifest.shards.len(), 2, "the new shard continues the index");
    let reread = validate_pack(path).unwrap();
    assert_eq!(reread.records, 3);
}

/// An empty pack is still a pack: `finish` with nothing written leaves a manifest (count 0) so a
/// caller can tell "nothing to do" from "no pack at all".
#[test]
fn finish_with_no_records_writes_a_zero_count_manifest() {
    let dir = tempfile::TempDir::new().unwrap();
    let w = PackWriter::open(dir.path(), "web", "1", 10).unwrap();
    let manifest = w.finish().unwrap();
    assert_eq!(manifest.records, 0);
    assert_eq!(validate_pack(dir.path()).unwrap().records, 0);
}

/// A pack whose manifest no longer matches its shards must fail loudly: silently restarting would
/// leave stale shards behind and collide with the new run's shard names.
#[test]
fn a_corrupt_pack_is_an_error_not_a_silent_restart() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path();

    {
        let mut w = PackWriter::open(path, "web", "1", 2).unwrap();
        w.push(rec("a")).unwrap();
        w.push(rec("b")).unwrap();
        w.finish().unwrap();
    }

    // Tamper with a shard: its checksum in the manifest no longer matches its bytes.
    let shard = path.join("web-00000.jsonl");
    let mut bytes = std::fs::read(&shard).unwrap();
    bytes.extend_from_slice(b"{\"id\":\"injected\"}\n");
    std::fs::write(&shard, bytes).unwrap();

    assert!(
        PackWriter::open(path, "web", "1", 2).is_err(),
        "a corrupt pack must not be silently restarted"
    );
}
