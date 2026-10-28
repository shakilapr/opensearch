//! Federation wiring: the env var must be parsed predictably, and a bad shard must not stop
//! the server. These tests need no index, so they pin the wiring rather than the index.

use std::path::PathBuf;

use search_api::{IndexHit, Searcher};
use search_core::contract::Signal;
use search_core::{AppResult, IndexManifest, Provenance, SearchFilters};

use search_agent0::fedserve::{dirs_from_env, searcher};

/// Unset: no shards, so `serve` keeps the single-root path exactly as it was.
#[test]
fn no_env_var_means_no_federated_shards() {
    std::env::remove_var("SEARCH_FEDERATE_DIRS");
    assert!(dirs_from_env(std::path::Path::new("/var/lib/search")).is_empty());
}

/// The list is colon-separated, blank entries are ignored, the primary root is skipped (it is
/// already the first shard, and listing it twice would double every one of its hits), and a
/// repeated root is skipped too.
#[test]
fn dirs_are_parsed_in_order_without_the_primary_or_repeats() {
    // A private variable name per test would be better, but the process env is global to the
    // binary under test; this test is the only one that sets it.
    std::env::set_var(
        "SEARCH_FEDERATE_DIRS",
        "/var/lib/search-fleet/shard-0:/var/lib/search:/var/lib/search-fleet/shard-1::/var/lib/search-fleet/shard-0:",
    );
    let dirs = dirs_from_env(std::path::Path::new("/var/lib/search"));
    assert_eq!(
        dirs,
        vec![
            PathBuf::from("/var/lib/search-fleet/shard-0"),
            PathBuf::from("/var/lib/search-fleet/shard-1"),
        ],
        "order kept, primary and repeats dropped, blanks ignored"
    );
    std::env::remove_var("SEARCH_FEDERATE_DIRS");
}

/// A shard that cannot be opened is skipped and the server still gets a working searcher made of
/// the primary — the alternative is a server that refuses to start.
#[test]
fn an_unopenable_shard_does_not_stop_the_server() {
    std::env::set_var("SEARCH_FEDERATE_DIRS", "/nonexistent/shard-does-not-exist");
    let primary: std::sync::Arc<dyn Searcher + Send + Sync> =
        std::sync::Arc::new(StubPrimary::default());
    let built = searcher(primary, std::path::Path::new("/var/lib/search"))
        .expect("a bad shard is a degradation, not a failure");
    let hits = built.search("rust", 5, None).unwrap();
    assert_eq!(hits.len(), 1, "the primary still answers");
    std::env::remove_var("SEARCH_FEDERATE_DIRS");
}

/// Opening a shard must not create it. `Config::ensure_dirs` would build an empty index at a
/// typo'd path, and a shard that exists but holds nothing answers "no results" forever — a much
/// worse outcome than a server that logs one loud refusal.
#[test]
fn a_missing_shard_root_is_not_created() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("typo");
    std::env::set_var("SEARCH_FEDERATE_DIRS", missing.to_str().unwrap());
    let primary: std::sync::Arc<dyn Searcher + Send + Sync> =
        std::sync::Arc::new(StubPrimary::default());
    searcher(primary, std::path::Path::new("/var/lib/search")).unwrap();
    assert!(!missing.exists(), "a shard root must never be created by the reader");
    std::env::remove_var("SEARCH_FEDERATE_DIRS");
}

/// With no shards configured the primary is returned as-is: a single-root deployment must not
/// pay for a fusion layer it does not need.
#[test]
fn with_no_shards_the_primary_is_returned_untouched() {
    std::env::remove_var("SEARCH_FEDERATE_DIRS");
    let primary: std::sync::Arc<dyn Searcher + Send + Sync> =
        std::sync::Arc::new(StubPrimary::default());
    let built = searcher(primary, std::path::Path::new("/var/lib/search")).unwrap();
    assert_eq!(built.manifest().unwrap().manifest_id, "primary-stub");
}

#[derive(Default)]
struct StubPrimary;

impl Searcher for StubPrimary {
    fn search(
        &self,
        _q: &str,
        limit: usize,
        _filters: Option<&SearchFilters>,
    ) -> AppResult<Vec<IndexHit>> {
        let hit = IndexHit {
            doc_id: "primary-1".into(),
            version: 1,
            resource_id: "res-1".into(),
            work_id: None,
            content_id: None,
            url: "https://primary.test/1".into(),
            title: "primary".into(),
            snippet: "primary".into(),
            source_id: None,
            doc_type: None,
            language: None,
            published_at: None,
            modified_at: None,
            fetched_at: 1_700_000_000,
            lane: "lexical_base".into(),
            provenance: Provenance::Snapshot,
            score: 1.0,
            signals: vec![Signal {
                name: "bm25".into(),
                value: 1.0,
                note: "stub".into(),
            }],
        };
        Ok(vec![hit].into_iter().take(limit).collect())
    }

    fn manifest(&self) -> AppResult<IndexManifest> {
        Ok(IndexManifest {
            manifest_id: "primary-stub".into(),
            generation: 1,
            doc_count: 1,
            base_segments: 1,
            watermark: 1,
            embedding_model_ids: Vec::new(),
            metadata_version: 1,
            created_at: 1,
            published_at: None,
        })
    }
}

/// A federated manifest must report the shards' real generations and counts.
///
/// Live defect 2026-10-28: the federation reported `generation 0` and `index_unpublished`
/// while returning real results, because a shard whose index was open reported
/// `manifest().is_err()` — swallowed by the sum — instead of its published generation.
#[test]
fn stub_shards_report_through_the_federation() {
    std::env::remove_var("SEARCH_FEDERATE_DIRS");
    let a: std::sync::Arc<dyn Searcher + Send + Sync> = std::sync::Arc::new(StubPrimary);
    let b: std::sync::Arc<dyn Searcher + Send + Sync> = std::sync::Arc::new(StubPrimary);
    let f = search_api::FederatedSearcher::new(vec![
        ("a".to_string(), a),
        ("b".to_string(), b),
    ])
    .unwrap();
    let m = f.manifest().unwrap();
    assert_eq!(m.generation, 1, "both stubs agree: {m:?}");
    assert_eq!(m.doc_count, 2, "counts are summed: {m:?}");
}
