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

// ---------------------------------------------------------------------------
// The structure the organizing model produced reaches the INDEX (searchable)
// ---------------------------------------------------------------------------
//
// NOTE on read-back: `headings`, `doc_type` and `language` are INDEXED-not-STORED in the frozen
// schema, and `Indexer::all_docs` rebuilds documents through a converter that hardcodes all four
// (including `entities`, which IS stored). So these tests assert what the index *promises* —
// that the structured fields are searchable — and not what `all_docs` echoes back. The read-back
// loss is a separate, known gap.

#[tokio::test]
async fn import_makes_the_structured_extras_searchable() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());
    let p = Pipeline::open_with(&cfg, false).unwrap();

    let url = "https://dataset.example/records/structured-note";
    let mut rec = record(url, "Body prose that names nothing in particular.");
    rec.extras.insert("summary".to_string(), "A short summary about kelvin optics.".to_string());
    rec.extras.insert("topics".to_string(), "kelvinoptics, zeta-ray".to_string());
    rec.extras
        .insert("key_entities".to_string(), "Kelvin Optics Institute, zeta-ray".to_string());
    rec.extras.insert("doc_type".to_string(), "research-paper".to_string());
    rec.extras.insert("language".to_string(), "fr".to_string());
    p.import_record(&rec).unwrap();

    // A topic that appears ONLY in the model's output must be findable: that is what makes the
    // structured pack a richer index rather than the same index with extra bytes.
    for term in ["zeta-ray", "kelvinoptics"] {
        let resp = p.search(term, 10).await.unwrap();
        assert_eq!(resp.results.len(), 1, "topics must be indexed; {term:?} found {:?}", resp.results);
        assert_eq!(resp.results[0].canonical_url, url);
    }
    // The summary is indexed with the headings, so a phrase from it matches.
    let resp = p.search("kelvin optics", 10).await.unwrap();
    assert_eq!(resp.results.len(), 1, "the summary must be indexed: {:?}", resp.results);
    // And the model's entities are indexed as entities.
    let resp = p.search("Kelvin Optics Institute", 10).await.unwrap();
    assert_eq!(resp.results.len(), 1, "key_entities must be indexed: {:?}", resp.results);

    // Control: a term from nowhere must not match.
    let resp = p.search("unrelatedquokka", 10).await.unwrap();
    assert!(resp.results.is_empty(), "nothing may be invented: {:?}", resp.results);
}

#[tokio::test]
async fn an_abandoned_generation_is_never_reused() {
    // The crash window: a generation is allocated durably, the batch is written, and the process
    // dies before the publish. If the next ingest derives its generation from the in-memory
    // manifest (`visible + 1`) it hands out the same number again, and the abandoned documents
    // become visible with the new batch. Allocation must be `MAX(generation) + 1` over every row,
    // published or not.
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());
    let p = Pipeline::open_with(&cfg, false).unwrap();

    let first = p
        .import_record(&record("https://example.com/a", "alpha body text"))
        .unwrap();
    let g1 = generation_of(&first);

    // Simulate the crash: a reservation with no matching publish.
    let abandoned = g1 + 1;
    {
        let conn = rusqlite::Connection::open(cfg.meta_db()).expect("open metadata db");
        conn.execute(
            "INSERT INTO manifest(manifest_id, generation, published, doc_count, base_segments,
                    created_at, published_at)
             VALUES (?1, ?2, 0, 0, 0, 1, NULL)",
            rusqlite::params![format!("gen-{abandoned}"), abandoned as i64],
        )
        .expect("reserve an abandoned generation");
    }

    let second = p
        .import_record(&record("https://example.com/b", "beta body text"))
        .unwrap();
    let g2 = generation_of(&second);
    assert_ne!(g2, abandoned, "an abandoned generation must never be reused");
    assert_eq!(
        g2,
        abandoned + 1,
        "allocation is MAX(generation) + 1 across published and unpublished rows"
    );

    // And the abandoned generation must still be unpublished in the metadata store.
    let conn = rusqlite::Connection::open(cfg.meta_db()).expect("open metadata db");
    let published: i64 = conn
        .query_row(
            "SELECT published FROM manifest WHERE generation = ?1",
            rusqlite::params![abandoned as i64],
            |r| r.get(0),
        )
        .expect("read the abandoned row");
    assert_eq!(published, 0, "the abandoned attempt stays unpublished");
}

#[tokio::test]
async fn import_without_extras_still_indexes_a_plain_record() {
    // A raw pack (`export --raw`) carries no structured extras; it must keep working and acquire
    // no fabricated structure.
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());
    let p = Pipeline::open_with(&cfg, false).unwrap();
    let url = "https://dataset.example/records/plain-note";
    p.import_record(&record(url, "A plain record about plainwombat subjects.")).unwrap();
    let resp = p.search("plainwombat", 10).await.unwrap();
    assert_eq!(resp.results.len(), 1, "a plain record is still searchable: {:?}", resp.results);
    for term in ["kelvinoptics", "zeta-ray", "research-paper"] {
        let resp = p.search(term, 10).await.unwrap();
        assert!(resp.results.is_empty(), "no structure may be invented: {term:?} -> {:?}", resp.results);
    }
}
