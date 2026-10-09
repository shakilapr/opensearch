//! Hermetic tests for `Pipeline::import_record`: out-of-band ingest that must
//! make a document searchable with NO network I/O (no SSRF, robots, or frontier
//! lease), be idempotent by content, and advance the index generation as new
//! records land.
//!
//! No fixture server is started: the pipeline is opened with the loopback SSRF
//! exemption *disabled* (`open_with(&cfg, false)`), so any stray network call
//! would be blocked/failed rather than silently succeed. Everything runs against
//! a tempdir data dir.

use std::collections::BTreeMap;
use std::path::Path;

use search_agent0::pipeline::ImportRecord;
use search_agent0::{IngestOutcome, Pipeline};
use search_core::config::Config;

fn test_config(data_dir: &Path) -> Config {
    let mut cfg = Config::default();
    cfg.data_dir = data_dir.to_path_buf();
    cfg.user_agent = "agent0-import-test/0.1 (+test)".to_string();
    cfg.bind = "127.0.0.1:0".to_string();
    cfg
}

fn record(url: &str, text: &str) -> ImportRecord {
    ImportRecord {
        url: url.to_string(),
        title: Some("Imported Record".to_string()),
        text: text.to_string(),
        source: "dataset-pack".to_string(),
        published_at: Some(1_700_000_000),
        extras: BTreeMap::new(),
    }
}

fn dv_count(cfg: &Config, resource_id: &str) -> i64 {
    let conn = rusqlite::Connection::open(cfg.meta_db()).expect("open metadata db");
    conn.query_row(
        "SELECT COUNT(*) FROM document_version WHERE resource_id = ?1",
        rusqlite::params![resource_id],
        |r| r.get(0),
    )
    .expect("count document_version")
}

fn generation_of(out: &IngestOutcome) -> u64 {
    match out {
        IngestOutcome::Ingested { generation, .. } => *generation,
        other => panic!("expected Ingested, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// (1) importing a record makes it searchable, with no network at all
// ---------------------------------------------------------------------------

#[tokio::test]
async fn import_record_is_searchable_without_network() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());
    // Loopback exemption OFF: a record import needs no URL fetch whatsoever.
    let p = Pipeline::open_with(&cfg, false).unwrap();

    let url = "https://dataset.example/records/zephyrquokka-note";
    let out = p
        .import_record(&record(
            url,
            "A zephyrquokka sighting was catalogued near the quasar fields.",
        ))
        .unwrap();
    assert!(out.is_ingested(), "first import creates a document, got {out:?}");

    // The record is immediately searchable, keyed by its canonical URL.
    let resp = p.search("zephyrquokka", 10).await.unwrap();
    assert_eq!(resp.results.len(), 1, "imported record is searchable");
    let hit = &resp.results[0];
    assert_eq!(hit.canonical_url, url, "canonical url round-trips");
    assert_eq!(hit.title, "Imported Record", "title round-trips");
    assert!(resp.overlay_watermark >= 1, "manifest published");

    // The full ingest walk was journalled, like any other document.
    let rid = p.resource_id_for(url).unwrap();
    let conn = rusqlite::Connection::open(cfg.meta_db()).unwrap();
    let mut stmt = conn
        .prepare("SELECT stage FROM ingest_event WHERE resource_id = ?1 ORDER BY seq")
        .unwrap();
    let stages: Vec<String> = stmt
        .query_map(rusqlite::params![&rid], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(
        stages,
        vec![
            "FETCHED",
            "NORMALIZED",
            "BLOB_WRITTEN",
            "METADATA_COMMITTED",
            "LEXICAL_INDEXED",
            "GRAPH_INDEXED",
            "VISIBLE",
        ],
        "import records the standard ingest lifecycle"
    );
}

// ---------------------------------------------------------------------------
// (2) importing identical text twice is Unchanged and adds no version
// ---------------------------------------------------------------------------

#[tokio::test]
async fn importing_same_record_twice_is_unchanged() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());
    let p = Pipeline::open_with(&cfg, false).unwrap();

    let url = "https://dataset.example/records/duplicate";
    let rec = record(url, "duplicate zephyrquokka payload token");
    let rid = p.resource_id_for(url).unwrap();

    assert!(p.import_record(&rec).unwrap().is_ingested());
    assert_eq!(dv_count(&cfg, &rid), 1);

    // Re-importing the identical record is idempotent: Unchanged, no new version.
    let second = p.import_record(&rec).unwrap();
    assert!(
        second.is_unchanged(),
        "re-importing identical text must be Unchanged, got {second:?}"
    );
    assert_eq!(
        dv_count(&cfg, &rid),
        1,
        "no second DocumentVersion for identical imported text"
    );

    // Still exactly one searchable document.
    let resp = p.search("zephyrquokka", 10).await.unwrap();
    assert_eq!(resp.results.len(), 1);
}

// ---------------------------------------------------------------------------
// (3) a second, different record on another host indexes and advances gen
// ---------------------------------------------------------------------------

#[tokio::test]
async fn second_record_on_another_host_advances_generation() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());
    let p = Pipeline::open_with(&cfg, false).unwrap();

    let a = p
        .import_record(&record("https://alpha.example/a", "alpha pulsar token"))
        .unwrap();
    let g1 = generation_of(&a);

    let b = p
        .import_record(&record("https://beta.example/b", "beta quasar token"))
        .unwrap();
    let g2 = generation_of(&b);

    assert!(g2 > g1, "index generation advances: {g1} -> {g2}");

    // Both records are independently searchable, and the shared token matches both.
    assert_eq!(p.search("pulsar", 10).await.unwrap().results.len(), 1);
    assert_eq!(p.search("quasar", 10).await.unwrap().results.len(), 1);
    let all = p.search("token", 10).await.unwrap();
    assert_eq!(all.results.len(), 2, "both hosts contribute a document");
    let urls: Vec<&str> = all.results.iter().map(|h| h.canonical_url.as_str()).collect();
    assert!(urls.contains(&"https://alpha.example/a"));
    assert!(urls.contains(&"https://beta.example/b"));
}

// ---------------------------------------------------------------------------
// (4) empty text is rejected honestly (not indexed as an empty document)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_record_text_is_rejected() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());
    let p = Pipeline::open_with(&cfg, false).unwrap();

    let err = p
        .import_record(&record("https://dataset.example/empty", "   "))
        .unwrap_err();
    assert!(
        matches!(err, search_core::AppError::Invalid(_)),
        "empty record text must be Invalid, got {err:?}"
    );
}
