//! Hermetic Stage-6 tests: every adapter parses its committed fixture offline, OAI-PMH follows
//! its `resumptionToken` to exhaustion, and packs round-trip (including tamper rejection).

use std::path::PathBuf;

use search_dataset::{
    build_from_adapter, validate_pack, write_pack, CommonCrawlAdapter, DatasetAdapter,
    DatasetError, OaiPmhAdapter, OpenAlexAdapter, Page, Record, WikidataAdapter,
};

fn fixtures() -> PathBuf {
    // crates/dataset -> repo root -> datasets/fixtures
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../datasets/fixtures")
}

fn commoncrawl_fixture() -> PathBuf {
    fixtures().join("commoncrawl/sample.jsonl")
}

/// Drain an adapter into one vector, asserting the cursor chain terminates.
fn drain(adapter: &mut dyn DatasetAdapter) -> Vec<Record> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    let mut guard = 0;
    loop {
        guard += 1;
        assert!(guard < 100, "adapter paging did not terminate");
        match adapter.page(cursor.as_deref()) {
            Ok(page) => {
                out.extend(page.records);
                match page.next {
                    Some(token) => cursor = Some(token),
                    None => break,
                }
            }
            Err(DatasetError::Exhausted) => break,
            Err(e) => panic!("unexpected adapter error: {e}"),
        }
    }
    out
}

#[test]
fn commoncrawl_fixture_parses_into_records() {
    let mut adapter = CommonCrawlAdapter::new(commoncrawl_fixture());
    assert_eq!(adapter.name(), "commoncrawl");
    let records = drain(&mut adapter);
    assert_eq!(records.len(), 5);

    let first = &records[0];
    assert_eq!(first.source, "commoncrawl");
    assert_eq!(first.url.as_deref(), Some("https://example.com/a"));
    assert_eq!(first.title.as_deref(), Some("Example A"));
    assert!(first.text.contains("Alpha page"));
    // 2024-01-02T03:04:05Z
    assert_eq!(first.published_at, Some(1_704_164_645));
    assert_eq!(first.extras.get("status").map(String::as_str), Some("200"));
    assert_eq!(first.extras.get("mime").map(String::as_str), Some("text/html"));
    // `content` is used when `text` is absent.
    assert!(records[2].text.contains("Not found"));
}

#[test]
fn commoncrawl_pages_by_cursor() {
    let mut adapter = CommonCrawlAdapter::with_page_size(commoncrawl_fixture(), 2);
    let p1 = adapter.page(None).unwrap();
    assert_eq!(p1.records.len(), 2);
    assert_eq!(p1.next.as_deref(), Some("2"));

    let p2 = adapter.page(Some("2")).unwrap();
    assert_eq!(p2.records.len(), 2);
    assert_eq!(p2.next.as_deref(), Some("4"));

    let p3 = adapter.page(Some("4")).unwrap();
    assert_eq!(p3.records.len(), 1);
    assert_eq!(p3.next, None);

    // Paging past the end is Exhausted.
    assert!(matches!(adapter.page(Some("5")), Err(DatasetError::Exhausted)));
}

#[test]
fn wikidata_fixture_parses_into_records() {
    let mut adapter = WikidataAdapter::new(fixtures().join("wikidata/entities.json"));
    assert_eq!(adapter.name(), "wikidata");
    let records = drain(&mut adapter);
    assert_eq!(records.len(), 4);

    let q42 = &records[0];
    assert_eq!(q42.id, "Q42");
    assert_eq!(q42.source, "wikidata");
    assert_eq!(q42.title.as_deref(), Some("Douglas Adams"));
    assert_eq!(q42.text, "English writer and humorist");
    assert_eq!(
        q42.url.as_deref(),
        Some("https://en.wikipedia.org/wiki/Douglas_Adams")
    );
    assert_eq!(q42.extras.get("type").map(String::as_str), Some("item"));
    assert_eq!(q42.extras.get("claims").map(String::as_str), Some("2"));

    // Non-English-only entity still yields a label (any-language fallback).
    assert_eq!(records[2].id, "Q1");
    assert_eq!(records[2].title.as_deref(), Some("Univers"));
    // Property entity carries its datatype.
    assert_eq!(records[3].extras.get("datatype").map(String::as_str), Some("wikibase-item"));
}

#[test]
fn openalex_fixture_parses_into_records() {
    let mut adapter = OpenAlexAdapter::new(fixtures().join("openalex/works.json"));
    assert_eq!(adapter.name(), "openalex");
    let records = drain(&mut adapter);
    assert_eq!(records.len(), 3);

    let w1 = &records[0];
    assert_eq!(w1.id, "https://openalex.org/W1");
    assert_eq!(w1.title.as_deref(), Some("Search Engines at Scale"));
    assert_eq!(w1.url.as_deref(), Some("https://journal.example.com/w1"));
    // Abstract reconstructed from the inverted index.
    assert_eq!(w1.text, "Search engines at scale index the web");
    assert_eq!(w1.published_at, Some(1_619_827_200)); // 2021-05-01
    assert_eq!(w1.extras.get("doi").map(String::as_str), Some("https://doi.org/10.1/abc"));
    assert_eq!(w1.extras.get("cited_by_count").map(String::as_str), Some("42"));

    // No abstract -> title stands in as text.
    assert_eq!(records[1].text, "Crawling the Web");
}

#[test]
fn oai_pmh_follows_resumption_token_to_exhaustion() {
    let mut adapter = OaiPmhAdapter::new(fixtures().join("oai-pmh"));
    assert_eq!(adapter.name(), "oai-pmh");

    let p1 = adapter.page(None).unwrap();
    assert_eq!(p1.records.len(), 2);
    assert_eq!(p1.next.as_deref(), Some("token2"));

    let p2 = adapter.page(Some("token2")).unwrap();
    assert_eq!(p2.records.len(), 2);
    assert_eq!(p2.next.as_deref(), Some("token3"));

    // token3 carries an empty <resumptionToken/> -> end of the harvest.
    let p3 = adapter.page(Some("token3")).unwrap();
    assert_eq!(p3.records.len(), 1);
    assert_eq!(p3.next, None);

    // Re-drain from the top to count the full set.
    let mut fresh = OaiPmhAdapter::new(fixtures().join("oai-pmh"));
    let all = drain(&mut fresh);
    assert_eq!(all.len(), 5);

    let first = &all[0];
    assert_eq!(first.source, "oai-pmh");
    assert_eq!(first.id, "oai:repo.example.com:1");
    assert_eq!(first.title.as_deref(), Some("First Article"));
    assert_eq!(first.url.as_deref(), Some("https://repo.example.com/items/1"));
    assert_eq!(first.text, "An abstract for the first article about indexing.");
    assert_eq!(first.published_at, Some(1_704_153_600)); // 2024-01-02T00:00:00Z
    assert_eq!(all[4].id, "oai:repo.example.com:5");
}

/// An adapter with no records at all.
struct EmptyAdapter;

impl DatasetAdapter for EmptyAdapter {
    fn name(&self) -> &str {
        "empty"
    }
    fn page(&mut self, _cursor: Option<&str>) -> Result<Page, DatasetError> {
        Err(DatasetError::Exhausted)
    }
}

#[test]
fn empty_adapter_is_exhausted() {
    let mut adapter = EmptyAdapter;
    assert!(matches!(adapter.page(None), Err(DatasetError::Exhausted)));
}

fn sample_records(n: usize) -> Vec<Record> {
    (0..n)
        .map(|i| {
            let mut r = Record::new(format!("id-{i}"), "test");
            r.url = Some(format!("https://example.com/{i}"));
            r.text = format!("body {i}");
            r
        })
        .collect()
}

#[test]
fn write_and_validate_pack_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let manifest = write_pack(tmp.path(), "release", "1.0.0", sample_records(5), 2).unwrap();

    assert_eq!(manifest.name, "release");
    assert_eq!(manifest.version, "1.0.0");
    assert_eq!(manifest.records, 5);
    assert_eq!(manifest.schema, search_dataset::PACK_SCHEMA);
    assert_eq!(manifest.shards.len(), 3); // 2 + 2 + 1
    assert_eq!(manifest.shards[0].records, 2);
    assert_eq!(manifest.shards[2].records, 1);
    assert!(manifest.created_at > 0);
    assert!(manifest.shards[0].checksum.starts_with("fnv1a64:"));

    let revalidated = validate_pack(tmp.path()).unwrap();
    assert_eq!(revalidated, manifest);
}

#[test]
fn tampered_shard_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let manifest = write_pack(tmp.path(), "release", "1.0.0", sample_records(4), 2).unwrap();
    validate_pack(tmp.path()).unwrap();

    // Tamper with the first shard's bytes in a way that keeps it valid JSONL.
    let shard = tmp.path().join(&manifest.shards[0].path);
    let original = std::fs::read_to_string(&shard).unwrap();
    let tampered = original.replacen("body 0", "body 999", 1);
    assert_ne!(original, tampered);
    std::fs::write(&shard, tampered).unwrap();

    let err = validate_pack(tmp.path()).unwrap_err();
    assert!(matches!(err, DatasetError::Parse(msg) if msg.contains("checksum mismatch")));
}

#[test]
fn missing_shard_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let manifest = write_pack(tmp.path(), "release", "1.0.0", sample_records(2), 2).unwrap();
    std::fs::remove_file(tmp.path().join(&manifest.shards[0].path)).unwrap();
    assert!(validate_pack(tmp.path()).is_err());
}

#[test]
fn build_from_adapter_honours_max_records() {
    let tmp = tempfile::tempdir().unwrap();
    let mut adapter = CommonCrawlAdapter::with_page_size(commoncrawl_fixture(), 2);

    let manifest = build_from_adapter(&mut adapter, tmp.path(), "cc", "2024", 2, 3).unwrap();
    assert_eq!(manifest.records, 3);
    assert_eq!(manifest.shards.len(), 2); // 2 + 1
    assert_eq!(validate_pack(tmp.path()).unwrap().records, 3);

    // max_records = 0 -> an empty (but valid) pack.
    let empty_dir = tmp.path().join("empty");
    let mut adapter = CommonCrawlAdapter::new(commoncrawl_fixture());
    let manifest = build_from_adapter(&mut adapter, &empty_dir, "cc-empty", "0", 10, 0).unwrap();
    assert_eq!(manifest.records, 0);
    assert!(manifest.shards.is_empty());
    assert_eq!(validate_pack(&empty_dir).unwrap().records, 0);
}

/// End-to-end: build a pack from every fixture and validate it.
#[test]
fn builds_a_pack_from_each_fixture() {
    let tmp = tempfile::tempdir().unwrap();

    let cases: Vec<(&str, Box<dyn Fn() -> Vec<Record>>, u64)> = vec![
        (
            "commoncrawl",
            Box::new(|| {
                let mut a = CommonCrawlAdapter::new(commoncrawl_fixture());
                drain(&mut a)
            }),
            5,
        ),
        (
            "wikidata",
            Box::new(|| {
                let mut a = WikidataAdapter::new(fixtures().join("wikidata/entities.json"));
                drain(&mut a)
            }),
            4,
        ),
        (
            "openalex",
            Box::new(|| {
                let mut a = OpenAlexAdapter::new(fixtures().join("openalex/works.json"));
                drain(&mut a)
            }),
            3,
        ),
        (
            "oai-pmh",
            Box::new(|| {
                let mut a = OaiPmhAdapter::new(fixtures().join("oai-pmh"));
                drain(&mut a)
            }),
            5,
        ),
    ];

    for (label, collect, expected) in cases {
        let dir = tmp.path().join(label);
        let records = collect();
        assert_eq!(records.len() as u64, expected, "{label}: fixture record count");
        let manifest = write_pack(&dir, label, "1.0.0", records, 2).unwrap();
        assert_eq!(manifest.records, expected, "{label}");
        let revalidated = validate_pack(&dir).unwrap();
        assert_eq!(revalidated.records, expected, "{label}");
        assert_eq!(revalidated, manifest, "{label}");
    }
}

/// build_from_adapter against each fixture's real adapter type (dyn dispatch path).
#[test]
fn build_from_adapter_dyn_dispatch_for_each_source() {
    let tmp = tempfile::tempdir().unwrap();

    let mut cc = CommonCrawlAdapter::new(commoncrawl_fixture());
    let mut wd = WikidataAdapter::new(fixtures().join("wikidata/entities.json"));
    let mut oa = OpenAlexAdapter::new(fixtures().join("openalex/works.json"));
    let mut oai = OaiPmhAdapter::new(fixtures().join("oai-pmh"));

    let adapters: Vec<(&str, &mut dyn DatasetAdapter, u64)> = vec![
        ("commoncrawl", &mut cc, 5),
        ("wikidata", &mut wd, 4),
        ("openalex", &mut oa, 3),
        ("oai-pmh", &mut oai, 5),
    ];

    for (name, adapter, expected) in adapters {
        assert_eq!(adapter.name(), name);
        let dir = tmp.path().join(name);
        let manifest = build_from_adapter(adapter, &dir, name, "1.0.0", 3, u64::MAX).unwrap();
        assert_eq!(manifest.records, expected, "{name}");
        assert_eq!(validate_pack(&dir).unwrap().records, expected, "{name}");
    }
}

#[test]
fn invalid_shard_size_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let err = write_pack(tmp.path(), "x", "1", sample_records(1), 0);
    assert!(matches!(err, Err(DatasetError::Parse(_))));
}
