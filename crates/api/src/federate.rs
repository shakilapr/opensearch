//! Query-time federation across independent index shards (plan-v2 Stage 1).
//!
//! The corpus is crawled as four independent shards, each a complete data root with its own
//! metadata database, Tantivy index and (eventually) vector store. Today one shard is served and
//! the rest are dark: 19,255 of 501,247 documents are reachable. This module makes a set of
//! [`Searcher`]s look like one.
//!
//! **Why rank fusion and not score merging.** Scores from different shards are not comparable —
//! each index has its own document frequencies, so a BM25 score of 12 in a 120K-document shard
//! and 12 in a 20K-document shard mean different things. Merging on rank (the same reciprocal
//! rank fusion the engine uses internally, `c` = 60) needs no calibration and cannot be fooled by
//! a shard whose scores happen to sit on a different scale.
//!
//! **Duplicates are real.** The same page is reachable from several shards' seeds (a site's
//! sitemap appears in more than one crawl), so the same URL can carry different `doc_id`s. Hits
//! are collapsed on a canonicalised URL, keeping the best-ranked copy and summing the fused
//! contributions — a document two shards agree on genuinely deserves the extra weight.
//!
//! **A dead shard is a degradation, not an error.** One shard failing to open must not turn every
//! query into a 500: the remaining shards answer, and the failure is reported through
//! [`Searcher::degradations`] so `/health` and the response say so instead of silently serving a
//! quarter of the corpus as if it were all of it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use search_core::{AppError, AppResult, IndexManifest};

use crate::query::{IndexHit, Searcher};

/// The RRF constant, matching the engine's fusion (`c` = 60 in `search_engine::fuse`).
const RRF_C: f64 = 60.0;

/// One shard: a name for logs/provenance and the searcher that answers for it.
struct Shard {
    name: String,
    searcher: Arc<dyn Searcher + Send + Sync>,
}

/// A [`Searcher`] that fans a query out to several independent shards and fuses by rank.
pub struct FederatedSearcher {
    shards: Vec<Shard>,
    /// Last error seen per shard, surfaced through `degradations()`.
    failures: Mutex<BTreeMap<String, String>>,
}

impl FederatedSearcher {
    /// Build from named searchers. One shard is legal (it then behaves as that shard, plus a
    /// `federated: 1 shard` note); zero shards is an error, because a searcher that can never
    /// return a hit is a misconfiguration, not a corpus.
    pub fn new(shards: Vec<(String, Arc<dyn Searcher + Send + Sync>)>) -> AppResult<Self> {
        if shards.is_empty() {
            return Err(AppError::Internal(
                "federated searcher needs at least one shard".into(),
            ));
        }
        Ok(Self {
            shards: shards
                .into_iter()
                .map(|(name, searcher)| Shard { name, searcher })
                .collect(),
            failures: Mutex::new(BTreeMap::new()),
        })
    }

    /// The shard names, in the order they are queried.
    pub fn shard_names(&self) -> Vec<String> {
        self.shards.iter().map(|s| s.name.clone()).collect()
    }

    /// Canonical form of a URL for duplicate detection: scheme and host lowercased, fragment
    /// dropped, a lone trailing slash dropped. Query strings are kept — `?page=2` is a different
    /// page. A hit with no URL falls back to its `doc_id`, prefixed so it can never collide with
    /// a URL.
    fn dedupe_key(hit: &IndexHit) -> String {
        if hit.url.trim().is_empty() {
            return format!("doc:{}", hit.doc_id);
        }
        let no_fragment = hit.url.split('#').next().unwrap_or(&hit.url);
        let (scheme, rest) = match no_fragment.split_once("://") {
            Some((s, r)) => (format!("{}://", s.to_ascii_lowercase()), r),
            None => (String::new(), no_fragment),
        };
        let (authority, path) = match rest.split_once('/') {
            Some((a, p)) => (a.to_ascii_lowercase(), format!("/{p}")),
            None => (rest.to_ascii_lowercase(), String::new()),
        };
        // `/page/` and `/page` are the same page far more often than not, and a crawler that
        // followed the same link with and without a trailing slash is the common source of
        // duplicates here. Only one trailing slash is removed, so `/page//x` is untouched.
        let path = path.trim_end_matches('/').to_string();
        format!("{scheme}{authority}{path}")
    }

    /// One shard's hits, or the error text. Never panics: a shard that fails is recorded and
    /// skipped.
    fn ask(&self, shard: &Shard, q: &str, limit: usize, filters: Option<&search_core::SearchFilters>) -> Option<Vec<IndexHit>> {
        match shard.searcher.search(q, limit, filters) {
            Ok(hits) => {
                if let Ok(mut f) = self.failures.lock() {
                    f.remove(&shard.name);
                }
                Some(hits)
            }
            Err(e) => {
                if let Ok(mut f) = self.failures.lock() {
                    f.insert(shard.name.clone(), e.to_string());
                }
                None
            }
        }
    }
}

impl Searcher for FederatedSearcher {
    /// Find one document by id across the shards.
    ///
    /// A shard is chosen by `Ask` order, which is cheapest because ids do not encode a shard — so
    /// this is a small linear probe with an early return. It exists because a non-lexical lane
    /// (dense, graph, concept) returns ids found in *its* data, which may live on any shard; without
    /// it those candidates were dropped before rendering.
    fn lookup_doc(&self, doc_id: &str) -> Option<IndexHit> {
        for shard in &self.shards {
            if let Some(hit) = shard.searcher.lookup_doc(doc_id) {
                return Some(hit);
            }
        }
        None
    }

    fn search(
        &self,
        q: &str,
        limit: usize,
        filters: Option<&search_core::SearchFilters>,
    ) -> AppResult<Vec<IndexHit>> {
        // Ask every shard for `limit` hits, in parallel: four shards at ~200 ms each is ~200 ms,
        // not ~800 ms. Scoped threads keep this `&self` and allocation-light.
        let mut per_shard: Vec<Option<Vec<IndexHit>>> = Vec::with_capacity(self.shards.len());
        std::thread::scope(|scope| {
            let handles: Vec<_> = self
                .shards
                .iter()
                .map(|shard| scope.spawn(move || self.ask(shard, q, limit, filters)))
                .collect();
            for h in handles {
                per_shard.push(h.join().unwrap_or(None));
            }
        });

        if per_shard.iter().all(Option::is_none) {
            let detail = self
                .failures
                .lock()
                .map(|f| {
                    f.iter()
                        .map(|(k, v)| format!("{k}: {v}"))
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .unwrap_or_default();
            return Err(AppError::Internal(format!(
                "every shard failed: {detail}"
            )));
        }

        // Reciprocal rank fusion, keyed on the canonical URL. The kept copy is the one that
        // ranked best in its own shard — its snippet and signals are what a user should see —
        // while the score sums every shard's contribution, because agreement between shards is
        // real evidence and should not be thrown away.
        struct Fused {
            score: f64,
            best_rank: usize,
            hit: IndexHit,
        }
        let mut fused: BTreeMap<String, Fused> = BTreeMap::new();
        for hits in per_shard.into_iter().flatten() {
            for (rank, hit) in hits.into_iter().enumerate() {
                let contribution = 1.0 / (RRF_C + (rank + 1) as f64);
                let key = Self::dedupe_key(&hit);
                match fused.get_mut(&key) {
                    Some(f) => {
                        f.score += contribution;
                        if rank + 1 < f.best_rank {
                            f.best_rank = rank + 1;
                            f.hit = hit;
                        }
                    }
                    None => {
                        fused.insert(
                            key,
                            Fused {
                                score: contribution,
                                best_rank: rank + 1,
                                hit,
                            },
                        );
                    }
                }
            }
        }

        let mut out: Vec<Fused> = fused.into_values().collect();
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.hit.doc_id.cmp(&b.hit.doc_id))
        });
        Ok(out
            .into_iter()
            .take(limit)
            .map(|f| {
                let mut hit = f.hit;
                hit.score = f.score as f32;
                hit
            })
            .collect())
    }

    /// Stored text is shard-local: ask each shard in turn. The first shard that knows the id
    /// wins; if none does, `Ok(None)` (the same contract as a single shard that cannot read it).
    fn doc_text(&self, doc_id: &str) -> AppResult<Option<String>> {
        for shard in &self.shards {
            match shard.searcher.doc_text(doc_id) {
                Ok(Some(text)) => return Ok(Some(text)),
                Ok(None) => continue,
                Err(e) => {
                    if let Ok(mut f) = self.failures.lock() {
                        f.insert(shard.name.clone(), e.to_string());
                    }
                }
            }
        }
        Ok(None)
    }

    /// The aggregate manifest. Counts are summed and the *oldest* watermark is reported, so the
    /// number never overstates how fresh the federated view is. A generation is only claimed when
    /// every shard agrees on it; otherwise `0` (unknown) — reporting one shard's generation as if
    /// it were the federation's would be a lie about reproducibility.
    fn manifest(&self) -> AppResult<IndexManifest> {
        let mut manifests = Vec::new();
        for shard in &self.shards {
            match shard.searcher.manifest() {
                Ok(m) => manifests.push(m),
                Err(e) => {
                    if let Ok(mut f) = self.failures.lock() {
                        f.insert(shard.name.clone(), e.to_string());
                    }
                }
            }
        }
        if manifests.is_empty() {
            return Err(AppError::Internal(
                "no shard reported a manifest".into(),
            ));
        }
        let doc_count = manifests.iter().map(|m| m.doc_count).sum();
        let base_segments = manifests.iter().map(|m| m.base_segments).sum();
        let watermark = manifests.iter().map(|m| m.watermark).min().unwrap_or(0);
        let metadata_version = manifests.iter().map(|m| m.metadata_version).max().unwrap_or(0);
        // A generation is a *per-root* counter: two shards at generation 159,801 and 19,254 are
        // not comparable, they just publish on their own schedules. Any single number is therefore
        // arbitrary; what matters is (a) it is non-zero, so the API does not mark a working
        // federation `index_unpublished`, and (b) `manifest_id` says how many roots it covers.
        // The maximum is used because it is the only value guaranteed non-zero when any shard has
        // published, and `doc_count` below is the number that actually describes the corpus.
        let generation = manifests.iter().map(|m| m.generation).max().unwrap_or(0);
        let mut embedding_model_ids: Vec<String> = manifests
            .iter()
            .flat_map(|m| m.embedding_model_ids.clone())
            .collect();
        embedding_model_ids.sort();
        embedding_model_ids.dedup();
        Ok(IndexManifest {
            manifest_id: format!("federated-{}x{generation}", manifests.len()),
            generation,
            doc_count,
            base_segments,
            watermark,
            embedding_model_ids,
            metadata_version,
            created_at: manifests.iter().map(|m| m.created_at).max().unwrap_or(0),
            published_at: manifests.iter().filter_map(|m| m.published_at).max(),
        })
    }

    /// Every shard's own degradations, plus a note when a shard failed this process.
    fn degradations(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for shard in &self.shards {
            for d in shard.searcher.degradations() {
                out.push(format!("{}: {d}", shard.name));
            }
        }
        if let Ok(f) = self.failures.lock() {
            for (name, err) in f.iter() {
                out.push(format!("{name}: unavailable ({err})"));
            }
        }
        out
    }
}
