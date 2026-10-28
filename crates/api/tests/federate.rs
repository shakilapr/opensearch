//! Federation: several independent shards answering as one.
//!
//! These are the properties a federated searcher has to have before it may sit in front of four
//! real shards: it merges by rank (scores from different indexes are not comparable), it collapses
//! the same page seen twice, it survives one shard being down, and it never reports a document
//! count or generation that overstates what it can actually see.

use std::sync::Arc;

use search_api::federate::FederatedSearcher;
use search_api::{IndexHit, Searcher};
use search_core::contract::Signal;
use search_core::{AppError, AppResult, IndexManifest, Provenance, SearchFilters};

/// A shard stub: fixed hits, an optional failure, and whatever text/manifest it is given.
struct StubShard {
    hits: Vec<IndexHit>,
    fail: bool,
    text: Option<String>,
    manifest: IndexManifest,
}

impl StubShard {
    fn new(ids: &[(&str, &str)]) -> Self {
        Self {
            hits: ids.iter().map(|(id, url)| hit(id, url)).collect(),
            fail: false,
            text: None,
            manifest: IndexManifest {
                manifest_id: "stub".into(),
                generation: 7,
                doc_count: 100,
                base_segments: 1,
                watermark: 50,
                embedding_model_ids: vec!["stub-embed".into()],
                metadata_version: 3,
                created_at: 1_700_000_000,
                published_at: Some(1_700_000_100),
            },
        }
    }

    fn failing(mut self) -> Self {
        self.fail = true;
        self
    }

    fn with_text(mut self, text: &str) -> Self {
        self.text = Some(text.to_string());
        self
    }

    fn with_manifest(mut self, f: impl FnOnce(&mut IndexManifest)) -> Self {
        f(&mut self.manifest);
        self
    }
}

fn hit(doc_id: &str, url: &str) -> IndexHit {
    IndexHit {
        doc_id: doc_id.into(),
        version: 1,
        resource_id: format!("res-{doc_id}"),
        work_id: None,
        content_id: None,
        url: url.into(),
        title: format!("title {doc_id}"),
        snippet: format!("snippet {doc_id}"),
        source_id: Some("src".into()),
        doc_type: Some("html".into()),
        language: Some("en".into()),
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
    }
}

impl Searcher for StubShard {
    fn search(
        &self,
        _q: &str,
        limit: usize,
        _filters: Option<&SearchFilters>,
    ) -> AppResult<Vec<IndexHit>> {
        if self.fail {
            return Err(AppError::Internal("shard is down".into()));
        }
        Ok(self.hits.iter().take(limit).cloned().collect())
    }

    fn doc_text(&self, _doc_id: &str) -> AppResult<Option<String>> {
        if self.fail {
            return Err(AppError::Internal("shard is down".into()));
        }
        Ok(self.text.clone())
    }

    fn manifest(&self) -> AppResult<IndexManifest> {
        if self.fail {
            return Err(AppError::Internal("shard is down".into()));
        }
        Ok(self.manifest.clone())
    }
}

fn federated(shards: Vec<(&str, StubShard)>) -> FederatedSearcher {
    FederatedSearcher::new(
        shards
            .into_iter()
            .map(|(name, s)| (name.to_string(), Arc::new(s) as Arc<dyn Searcher + Send + Sync>))
            .collect(),
    )
    .unwrap()
}

/// Two shards, different documents: both are reachable, and a document both shards return ranks
/// above a document only one of them has.
#[test]
fn results_from_every_shard_are_merged_by_rank() {
    let a = StubShard::new(&[("a1", "https://a.test/1"), ("shared", "https://s.test/x")]);
    let b = StubShard::new(&[("b1", "https://b.test/1"), ("shared", "https://s.test/x")]);
    let f = federated(vec![("shard-a", a), ("shard-b", b)]);

    let hits = f.search("anything", 10, None).unwrap();
    let urls: Vec<&str> = hits.iter().map(|h| h.url.as_str()).collect();
    assert_eq!(hits.len(), 3, "three distinct documents: {urls:?}");
    assert_eq!(
        hits[0].url, "https://s.test/x",
        "the document both shards returned must rank first: {urls:?}"
    );
    // Agreement is evidence: the shared document's fused score exceeds either single-shard
    // contribution.
    let single = hits
        .iter()
        .find(|h| h.url != "https://s.test/x")
        .unwrap()
        .score;
    assert!(hits[0].score > single, "shared doc {} vs {}", hits[0].score, single);
}

/// The same page reached through two shards is one result, and the copy kept is the one that
/// ranked better — so a `#fragment` or a trailing slash cannot produce a duplicate result.
#[test]
fn the_same_url_from_two_shards_is_one_result() {
    let a = StubShard::new(&[("a1", "https://Example.test/page")]);
    let b = StubShard::new(&[("b1", "https://example.test/page/#section")]);
    let f = federated(vec![("shard-a", a), ("shard-b", b)]);

    let hits = f.search("anything", 10, None).unwrap();
    assert_eq!(hits.len(), 1, "one page, not two: {hits:#?}");
}

/// One shard failing must not fail the query: the others answer and the failure is reported.
#[test]
fn a_dead_shard_degrades_instead_of_failing_the_query() {
    let a = StubShard::new(&[("a1", "https://a.test/1")]);
    let b = StubShard::new(&[("b1", "https://b.test/1")]).failing();
    let f = federated(vec![("shard-a", a), ("shard-b", b)]);

    let hits = f.search("anything", 10, None).unwrap();
    assert_eq!(hits.len(), 1, "the live shard still answers");
    let degradations = f.degradations();
    assert!(
        degradations.iter().any(|d| d.contains("shard-b") && d.contains("unavailable")),
        "a down shard must be visible: {degradations:?}"
    );
}

/// Every shard down is an error, not an empty result set: "no results" would be a lie.
#[test]
fn all_shards_down_is_an_error() {
    let f = federated(vec![
        ("shard-a", StubShard::new(&[]).failing()),
        ("shard-b", StubShard::new(&[]).failing()),
    ]);
    let err = f.search("anything", 10, None).unwrap_err();
    assert!(err.to_string().contains("every shard failed"), "{err}");
}

/// Stored text is shard-local: the federated searcher finds it in whichever shard has it.
#[test]
fn doc_text_is_found_in_whichever_shard_holds_it() {
    let a = StubShard::new(&[]);
    let b = StubShard::new(&[]).with_text("the body");
    let f = federated(vec![("shard-a", a), ("shard-b", b)]);
    assert_eq!(f.doc_text("doc-x").unwrap().as_deref(), Some("the body"));

    let empty = federated(vec![("shard-a", StubShard::new(&[]))]);
    assert_eq!(empty.doc_text("doc-x").unwrap(), None);
}

/// The aggregate manifest sums counts, reports the *oldest* watermark, and refuses to claim a
/// generation the shards disagree about.
#[test]
fn the_manifest_never_overstates_the_federated_view() {
    let a = StubShard::new(&[]);
    let b = StubShard::new(&[]).with_manifest(|m| {
        m.doc_count = 250;
        m.watermark = 20;
        m.generation = 9;
    });
    let f = federated(vec![("shard-a", a), ("shard-b", b)]);
    let m = f.manifest().unwrap();
    assert_eq!(m.doc_count, 350, "counts are summed");
    assert_eq!(m.watermark, 20, "the oldest watermark, not the newest");
    // Shards crawled independently sit at different generations by design. The honest report is
    // the oldest one (every shard is at least this fresh), not 0 — which the API reads as
    // "unpublished" and echoes to /health as a false statement about the corpus.
    assert_eq!(m.generation, 7, "the oldest generation, not 0 and not the newest");
    assert!(m.manifest_id.contains("federated"), "{}", m.manifest_id);

    let same = federated(vec![
        ("shard-a", StubShard::new(&[])),
        ("shard-b", StubShard::new(&[])),
    ]);
    assert_eq!(
        same.manifest().unwrap().generation,
        7,
        "agreement is still the minimum"
    );
}

/// A searcher with no shards is a misconfiguration, not an empty corpus.
#[test]
fn zero_shards_is_refused() {
    match FederatedSearcher::new(Vec::new()) {
        Err(err) => assert!(err.to_string().contains("at least one shard"), "{err}"),
        Ok(_) => panic!("a searcher with no shards must be refused"),
    }
}
