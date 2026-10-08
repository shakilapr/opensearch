//! The pipeline: one reusable [`Pipeline`] that owns the frontier, fetcher,
//! metadata writer, indexer and API-facing searcher, and drives URLs through the
//! ingest lifecycle.
//!
//! ## Stage 0 lifecycle (`crawl_one`)
//!
//! 1. canonicalize the URL (stable `ResourceId`),
//! 2. SSRF-validate the target and its `robots.txt`,
//! 3. push + lease the resource in the frontier,
//! 4. fetch `robots.txt` and evaluate it **before** fetching the resource,
//! 5. fetch the resource (conditionally, when validators are known),
//! 6. **route** the response (`search_router::classify`): HTML -> parser,
//!    PDF -> `search_pdf::extract` (its text is indexed), Feed ->
//!    `search_discovery::parse_feed` (entries are pushed to the frontier),
//!    anything else -> skipped,
//! 7. normalize (`ContentId` from the exact bytes),
//! 8. write the raw blob and the normalized text under `data_dir`,
//! 9. commit metadata (domain/resource/fetch observation/content/DocumentVersion
//!    + Stage-1 freshness columns),
//! 10. index into the Tantivy overlay, publish the manifest + watermark,
//! 11. record every journal stage, refresh the per-domain `SourceProfile` and
//!     mark the resource done.
//!
//! Stages are journalled through [`IngestCoordinator`], which is idempotent per
//! `(op_key, stage)`: re-running the full walk writes nothing new.
//!
//! ## Idempotency (the no-duplicate requirement)
//!
//! `crawl_one` derives `ContentId` from the fetched bytes and compares it to the
//! latest `DocumentVersion` for the resource. If unchanged, **no** second
//! `DocumentVersion` is created: the op is already on record, so the pipeline
//! skips to `VISIBLE` and returns [`IngestOutcome::Unchanged`].
//!
//! ## Stage 1 additions
//!
//! * [`Pipeline::discover`] — robots.txt -> sitemaps (one index level) + feeds
//!   (from the seed HTML and its `<link rel="alternate">` / feed-ish paths),
//!   persisted through the metadata `Mutation` API and pushed to the frontier,
//!   with a bounded fan-out.
//! * `crawl_one` routes each response through the router.
//! * Conditional refresh via `Fetcher::fetch_conditional` + a validator store:
//!   a `304` is `Unchanged` and does **not** re-parse or re-index.
//! * Per-domain `SourceProfile` (updated with `observe_fetch`, persisted with
//!   `UpsertSourceProfile`) and per-resource freshness columns.
//! * `search` runs the source-first lane from `search_retrieval` and passes its
//!   candidates through, tagging them with a `source_first` provenance signal.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rusqlite::{params, OptionalExtension};
use url::Url;

use search_api::query::{IndexHit, Searcher};
use search_api::IndexerSearcher;
use search_core::config::Config;
use search_core::contract::Signal;
use search_core::ids::{ContentId, DocVerId, DomainId, OpKey, ResourceId};
use search_core::{AppError, AppResult, Provenance, SearchRequest, SearchResponse};
use search_discovery::{
    parse_feed, parse_sitemap, sitemap_index_urls, sitemaps_in_robots, DiscoveredResource,
    ResourceKind,
};
use search_fetcher::canon::canonicalize;
use search_fetcher::http::{FetchOutcome, Fetched, Fetcher};
use search_fetcher::robots::{self, RobotsPolicy, RobotsVerdict};
use search_fetcher::ssrf;
use search_fetcher::validators::{MemoryValidators, ValidatorStore, Validators};
use search_frontier::queue::{FrontierEntry, Priority};
use search_frontier::Frontier;
use search_indexer::{IndexDoc, Indexer, IndexerConfig};
use search_ingest::{IngestCoordinator, IngestStage, Watermark};
use search_metadata::{MetadataReader, MetadataWriter, Mutation};
use search_parser::NormalizedDoc;
use search_pdf::PdfLimits;
use search_retrieval::{Candidate, Lane, LaneSearcher, RetrievalError, SourceFirst};
use search_router::{classify, Route};
use search_sources::SourceProfile;

/// Bumped when the Stage-0 HTML normalization contract changes.
const PARSER_VERSION: &str = "stage0-html-v1";
/// Bumped when the Stage-1 PDF projection changes.
const PARSER_VERSION_PDF: &str = "stage1-pdf-v1";

/// Bounds on `discover` fan-out (no unbounded crawl).
const MAX_SITEMAPS: usize = 8;
const MAX_CHILD_SITEMAPS: usize = 16;
const MAX_FEEDS: usize = 8;
const MAX_DISCOVERED_RESOURCES: usize = 512;
/// Bound on how many sources the source-first lane will scope to per query.
const MAX_SOURCE_FIRST_SOURCES: usize = 8;

/// The result of one [`Pipeline::crawl_one`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestOutcome {
    /// A new `DocumentVersion` was created, indexed and published.
    Ingested {
        url: String,
        resource_id: String,
        docver_id: String,
        version_no: u64,
        content_id: String,
        generation: u64,
    },
    /// The content hash matched the latest version: nothing new was created.
    Unchanged {
        url: String,
        resource_id: String,
        docver_id: String,
        version_no: u64,
        content_id: String,
        generation: u64,
    },
    /// `robots.txt` disallowed the path; nothing was fetched or indexed.
    Denied {
        url: String,
        resource_id: String,
        reason: String,
    },
    /// The response was routed to a non-document path: no `DocumentVersion` was
    /// created, but discovery side effects were applied (e.g. a feed's entries
    /// were parsed and pushed to the frontier). `discovered` counts the
    /// resources learned from this response.
    Routed {
        url: String,
        resource_id: String,
        route: String,
        discovered: usize,
    },
}

impl IngestOutcome {
    pub fn is_ingested(&self) -> bool {
        matches!(self, IngestOutcome::Ingested { .. })
    }
    pub fn is_unchanged(&self) -> bool {
        matches!(self, IngestOutcome::Unchanged { .. })
    }
    pub fn is_denied(&self) -> bool {
        matches!(self, IngestOutcome::Denied { .. })
    }
    /// `true` when a new document version is searchable as a result of this call.
    pub fn is_new_document(&self) -> bool {
        self.is_ingested()
    }
    /// The route string for a [`IngestOutcome::Routed`] result, else `None`.
    pub fn route(&self) -> Option<&str> {
        match self {
            IngestOutcome::Routed { route, .. } => Some(route),
            _ => None,
        }
    }
}

/// Summary of one [`Pipeline::discover`] call.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiscoveryReport {
    /// The canonical seed URL.
    pub seed_url: String,
    /// Sitemap documents persisted (index + children).
    pub sitemaps: usize,
    /// Feed documents persisted.
    pub feeds: usize,
    /// Resources pushed into the frontier.
    pub resources: usize,
}

/// The runnable pipeline.
///
/// One process owns the metadata writer (`SQLite` single-writer rule): the
/// writer sits behind a `std::sync::Mutex` and is never held across an `.await`.
pub struct Pipeline {
    cfg: Config,
    meta: Mutex<MetadataWriter>,
    frontier: Frontier,
    fetcher: Fetcher,
    /// Used only for the loopback-only test exemption path.
    http: reqwest::Client,
    indexer: Arc<Indexer>,
    owner: String,
    /// Stage-1 conditional-refresh validators, keyed by canonical URL.
    validators: MemoryValidators,
    /// Test-only: exempt loopback targets from the SSRF guard so a hermetic
    /// local server can be crawled. Production (`open`) leaves this `false`.
    allow_loopback: bool,
}

impl Pipeline {
    /// Open the pipeline in production mode: the SSRF guard blocks loopback.
    pub fn open(cfg: &Config) -> AppResult<Self> {
        Self::open_with(cfg, false)
    }

    /// Open the pipeline, optionally allowing loopback targets (tests only).
    pub fn open_with(cfg: &Config, allow_loopback: bool) -> AppResult<Self> {
        cfg.assert_not_synced().map_err(AppError::Config)?;
        cfg.ensure_dirs()?;
        cfg.limits.validate().map_err(AppError::Config)?;

        // Metadata single writer (runs migrations on open).
        let writer = MetadataWriter::from_path(&cfg.meta_db())?;
        let frontier = Frontier::new(cfg.meta_db());
        let fetcher = Fetcher::new(cfg.limits.clone(), cfg.user_agent.clone());
        let indexer = Arc::new(Indexer::open(IndexerConfig::new(cfg.data_dir.join("tantivy")))?);
        let http = reqwest::Client::builder()
            .user_agent(cfg.user_agent.clone())
            // Redirects are not auto-followed; the loopback path is direct.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| AppError::Internal(format!("failed to build http client: {e}")))?;

        Ok(Self {
            cfg: cfg.clone(),
            meta: Mutex::new(writer),
            frontier,
            fetcher,
            http,
            indexer,
            owner: format!("agent0-{}", std::process::id()),
            validators: MemoryValidators::new(),
            allow_loopback,
        })
    }

    /// The searcher the HTTP API should be wired to (real Tantivy indexer behind
    /// the source-first lane).
    pub fn searcher(&self) -> Arc<dyn Searcher + Send + Sync> {
        Arc::new(SourceFirstSearcher::new(
            self.indexer.clone(),
            self.cfg.meta_db(),
        ))
    }

    /// Deterministic resource id for a URL (canonicalized first), matching the
    /// id `crawl_one` uses.
    pub fn resource_id_for(&self, url: &str) -> AppResult<String> {
        let canonical = canonicalize(url)
            .map_err(|e| AppError::Invalid(format!("invalid url {url:?}: {e}")))?;
        Ok(ResourceId::from_canonical_url(&canonical).into_string())
    }

    /// Run a `/search` query against the live index, through the source-first
    /// lane so results can carry a `source_first` provenance signal.
    pub async fn search(&self, q: &str, limit: usize) -> AppResult<SearchResponse> {
        let searcher = SourceFirstSearcher::new(self.indexer.clone(), self.cfg.meta_db());
        let req = SearchRequest {
            query: q.to_string(),
            limit,
            offset: 0,
            timezone: "UTC".to_string(),
            deadline_ms: None,
            filters: None,
        };
        search_api::execute(&searcher, &req)
    }

    // ------------------------------------------------------------------ discover

    /// Discover resources for a seed URL **without fetching every page**:
    ///
    /// * fetch `robots.txt` and read its `Sitemap:` lines,
    /// * fetch each sitemap; if it is a sitemap index, follow exactly one level,
    /// * discover feeds from the seed HTML (`<link rel="alternate"
    ///   type="application/rss+xml|atom+xml">` and feed-ish hrefs),
    /// * persist sitemaps/feeds through the metadata `Mutation` API and push the
    ///   discovered Page/FeedEntry URLs into the frontier.
    ///
    /// Fan-out is bounded by [`MAX_SITEMAPS`], [`MAX_CHILD_SITEMAPS`],
    /// [`MAX_FEEDS`] and [`MAX_DISCOVERED_RESOURCES`].
    pub async fn discover(&self, seed_url: &str) -> AppResult<DiscoveryReport> {
        let canonical = canonicalize(seed_url)
            .map_err(|e| AppError::Invalid(format!("invalid url {seed_url:?}: {e}")))?;
        let parsed = Url::parse(&canonical)
            .map_err(|e| AppError::Invalid(format!("invalid canonical url {canonical:?}: {e}")))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| AppError::Invalid(format!("url {canonical:?} has no host")))?
            .to_string();
        let domain_id = DomainId::from_host(&host);
        let origin = origin_of(&parsed);
        let now = search_core::time::now_unix_s();

        self.guard(&canonical).await?;

        let mut report = DiscoveryReport {
            seed_url: canonical.clone(),
            ..Default::default()
        };
        let mut discovered: Vec<DiscoveredResource> = Vec::new();

        // (1) robots.txt -> sitemaps.
        let robots_url = format!("{origin}/robots.txt");
        self.guard(&robots_url).await?;
        let robots_text = self.robots_text(&robots_url).await.unwrap_or_default();
        let sitemap_urls: Vec<String> = sitemaps_in_robots(&robots_text)
            .into_iter()
            .map(|u| resolve_against(&origin, &u))
            .filter(|u| u.starts_with("http"))
            .take(MAX_SITEMAPS)
            .collect();

        // The seed HTML is fetched once: used for feed discovery and (when no
        // sitemap advertises it) as a page in its own right.
        let seed_body = self.get_body(&canonical).await?;

        for sm_url in &sitemap_urls {
            if self.guard(sm_url).await.is_err() {
                continue;
            }
            let Some(fo) = self.get_body(sm_url).await? else {
                continue;
            };
            let child_urls = sitemap_index_urls(fo.body.as_deref().unwrap_or(&[])).unwrap_or_default();
            let is_index = !child_urls.is_empty();
            if is_index {
                self.persist_sitemap(sm_url, &domain_id, true, 0, &fo, now)?;
                report.sitemaps += 1;
                for child in child_urls.into_iter().take(MAX_CHILD_SITEMAPS) {
                    let child = resolve_against(&origin, &child);
                    if !child.starts_with("http") || self.guard(&child).await.is_err() {
                        continue;
                    }
                    let Some(cfo) = self.get_body(&child).await? else {
                        continue;
                    };
                    let cr = parse_sitemap(cfo.body.as_deref().unwrap_or(&[]), &child).unwrap_or_default();
                    self.persist_sitemap(&child, &domain_id, false, cr.len() as i64, &cfo, now)?;
                    report.sitemaps += 1;
                    discovered.extend(cr);
                }
            } else {
                let resources = parse_sitemap(fo.body.as_deref().unwrap_or(&[]), sm_url).unwrap_or_default();
                self.persist_sitemap(sm_url, &domain_id, false, resources.len() as i64, &fo, now)?;
                report.sitemaps += 1;
                discovered.extend(resources);
            }
        }

        // (2) feeds from the seed HTML.
        if let Some(fo) = &seed_body {
            if let Some(body) = &fo.body {
                let html = String::from_utf8_lossy(body);
                for feed_url in feeds_in_html(&html, &canonical).into_iter().take(MAX_FEEDS) {
                    if self.guard(&feed_url).await.is_err() {
                        continue;
                    }
                    let Some(ffo) = self.get_body(&feed_url).await? else {
                        continue;
                    };
                    let entries = parse_feed(ffo.body.as_deref().unwrap_or(&[]), &feed_url).unwrap_or_default();
                    self.persist_feed(&feed_url, &domain_id, entries.len() as i64, &ffo, now)?;
                    report.feeds += 1;
                    discovered.extend(entries);
                }
            }
        }

        // (3) push discovered resources into the frontier (bounded).
        let mut pushed = 0usize;
        for r in discovered.into_iter().take(MAX_DISCOVERED_RESOURCES) {
            if r.kind == ResourceKind::Sitemap {
                continue;
            }
            let resource_id = match self.resource_id_for(&r.url) {
                Ok(id) => id,
                Err(_) => continue,
            };
            let priority = match r.kind {
                ResourceKind::FeedEntry => Priority::Hot,
                _ => Priority::Discovery,
            };
            let entry = FrontierEntry {
                resource_id,
                url: r.url.clone(),
                domain_id: domain_id.as_str().to_string(),
                priority,
                earliest_fetch_at: 0,
                expected_value: None,
                discovered_from: Some(r.source_url.clone()),
            };
            if self.frontier.push(entry).is_ok() {
                pushed += 1;
            }
        }
        report.resources = pushed;

        Ok(report)
    }

    // ----------------------------------------------------------------- crawl_one

    /// Full single-URL ingest: lease -> robots -> conditional fetch -> route ->
    /// normalize -> ContentId -> DocumentVersion -> journal stages -> Tantivy
    /// index -> publish manifest. Refreshes the per-domain `SourceProfile` and
    /// the resource freshness columns along the way.
    pub async fn crawl_one(&self, url: &str) -> AppResult<IngestOutcome> {
        let canonical = canonicalize(url)
            .map_err(|e| AppError::Invalid(format!("invalid url {url:?}: {e}")))?;
        let parsed = Url::parse(&canonical)
            .map_err(|e| AppError::Invalid(format!("invalid canonical url {canonical:?}: {e}")))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| AppError::Invalid(format!("url {canonical:?} has no host")))?
            .to_string();
        let domain_id = DomainId::from_host(&host);
        let resource_id = ResourceId::from_canonical_url(&canonical);
        let origin = origin_of(&parsed);
        let robots_url = format!("{origin}/robots.txt");

        // (2) SSRF guard BEFORE any network I/O.
        self.guard(&canonical).await?;
        self.guard(&robots_url).await?;

        // (3) lease the resource.
        let now = search_core::time::now_unix_s();
        self.frontier.push(FrontierEntry::new(
            resource_id.as_str(),
            canonical.clone(),
            domain_id.as_str(),
            Priority::Normal,
            0,
        ))?;
        let lease = self
            .frontier
            .lease(&self.owner, now, 1)?
            .into_iter()
            .find(|l| l.resource_id == resource_id.as_str());

        // (4) robots before the resource fetch.
        let policy = self.load_robots(&robots_url, &domain_id).await?;
        if robots::evaluate(&policy, parsed.path()) == RobotsVerdict::Disallowed {
            if let Some(l) = &lease {
                let _ = self.frontier.fail(l, now, false);
            }
            return Ok(IngestOutcome::Denied {
                url: canonical,
                resource_id: resource_id.into_string(),
                reason: format!("robots.txt disallows {}", parsed.path()),
            });
        }

        // (5) conditional fetch (a 304 is Unchanged: no re-parse, no re-index).
        let outcome = match self.do_fetch_conditional(&canonical).await {
            Ok(o) => o,
            Err(e) => {
                let _ = self.observe_source(&host, false, false);
                if let Some(l) = &lease {
                    let _ = self.frontier.fail(l, now, e.is_retryable());
                }
                return Err(e);
            }
        };

        if outcome.status == 304 {
            self.observe_source(&host, true, false)?;
            self.persist_freshness(&resource_id, None, None, now, None)?;
            if let Some(l) = &lease {
                let _ = self.frontier.complete(l, now);
            }
            let latest = self.latest_version(&resource_id)?;
            let (version_no, content_id) = match latest {
                Some((v, c)) => (v, c.unwrap_or_default()),
                None => (1, String::new()),
            };
            let dv = DocVerId::from_parts(&resource_id, version_no).into_string();
            let rid = resource_id.into_string();
            return Ok(IngestOutcome::Unchanged {
                url: canonical,
                resource_id: rid,
                docver_id: dv,
                version_no,
                content_id,
                generation: self.indexer.visible_generation(),
            });
        }

        if !(200..300).contains(&outcome.status) {
            self.observe_source(&host, false, false)?;
            if let Some(l) = &lease {
                let _ = self.frontier.fail(l, now, false);
            }
            return Err(classify_status(outcome.status, &canonical));
        }

        let body = outcome.body.clone().unwrap_or_default();
        let sniff = &body[..body.len().min(4096)];
        let routing = classify(outcome.content_type.as_deref(), &canonical, sniff);

        // (6) route. Feeds and non-document routes never create a DocumentVersion.
        if routing.route == Route::Feed {
            let discovered = self.ingest_feed_body(&canonical, &body, &domain_id, now)?;
            self.observe_source(&host, true, false)?;
            if let Some(l) = &lease {
                let _ = self.frontier.complete(l, now);
            }
            return Ok(IngestOutcome::Routed {
                url: canonical,
                resource_id: resource_id.into_string(),
                route: "feed".to_string(),
                discovered,
            });
        }
        if !matches!(routing.route, Route::Html | Route::Pdf) {
            self.observe_source(&host, true, false)?;
            if let Some(l) = &lease {
                let _ = self.frontier.complete(l, now);
            }
            return Ok(IngestOutcome::Routed {
                url: canonical,
                resource_id: resource_id.into_string(),
                route: format!("{:?}", routing.route),
                discovered: 0,
            });
        }

        // (7) ContentId from the exact bytes + idempotency check.
        let content_id = ContentId::from_bytes(&body);
        let existing = self.latest_version(&resource_id)?;
        if let Some((vno, Some(exist_cid))) = &existing {
            if exist_cid == content_id.as_str() {
                let op_key = OpKey::from_parts(&resource_id, 0, *vno);
                self.record_stage(&op_key, resource_id.as_str(), IngestStage::Visible)?;
                self.observe_source(&host, true, false)?;
                self.persist_freshness(&resource_id, None, None, now, None)?;
                if let Some(l) = &lease {
                    let _ = self.frontier.complete(l, now);
                }
                let dv = DocVerId::from_parts(&resource_id, *vno).into_string();
                let rid = resource_id.into_string();
                return Ok(IngestOutcome::Unchanged {
                    url: canonical,
                    resource_id: rid,
                    docver_id: dv,
                    version_no: *vno,
                    content_id: content_id.into_string(),
                    generation: self.indexer.visible_generation(),
                });
            }
        }
        let version_no = existing.as_ref().map(|(v, _)| v + 1).unwrap_or(1);
        let docver_id = DocVerId::from_parts(&resource_id, version_no);
        let op_key = OpKey::from_parts(&resource_id, 0, version_no);

        // Build the normalized doc according to the route.
        let (doc, kind, parser_version) = match routing.route {
            Route::Pdf => {
                let outcome_pdf = search_pdf::extract(&body, PdfLimits::default())
                    .map_err(|e| AppError::Invalid(format!("pdf extract failed: {e}")))?;
                if outcome_pdf.text.trim().is_empty() {
                    // Scanned/image-only PDF: no text layer in Stage 1. Record
                    // the failure honestly rather than indexing empty text.
                    self.observe_source(&host, true, true)?;
                    if let Some(l) = &lease {
                        let _ = self.frontier.complete(l, now);
                    }
                    return Ok(IngestOutcome::Routed {
                        url: canonical,
                        resource_id: resource_id.into_string(),
                        route: "pdf-no-text".to_string(),
                        discovered: 0,
                    });
                }
                (pdf_normalized_doc(&canonical, outcome_pdf.text), "pdf", PARSER_VERSION_PDF)
            }
            _ => (
                search_parser::normalize(&canonical, &body, outcome.content_type.as_deref())?,
                "html",
                PARSER_VERSION,
            ),
        };

        // FETCHED + NORMALIZED
        self.record_stage(&op_key, resource_id.as_str(), IngestStage::Fetched)?;
        self.record_stage(&op_key, resource_id.as_str(), IngestStage::Normalized)?;

        // BLOB_WRITTEN
        self.write_content_files(&content_id, &body, doc.text.as_bytes())?;
        self.record_stage(&op_key, resource_id.as_str(), IngestStage::BlobWritten)?;

        // METADATA_COMMITTED (resource kind, source id, parser version)
        self.commit_metadata(
            &canonical,
            &host,
            &domain_id,
            &resource_id,
            &docver_id,
            version_no,
            &content_id,
            &outcome,
            &doc,
            kind,
            &host,
            parser_version,
            now,
        )?;
        self.record_stage(&op_key, resource_id.as_str(), IngestStage::MetadataCommitted)?;

        // Stage-1 freshness columns + per-domain source profile.
        let feed_watermark = if self.is_feed_sourced(resource_id.as_str()) {
            Some(doc.published_at.unwrap_or(now))
        } else {
            None
        };
        self.persist_freshness(
            &resource_id,
            doc.published_at,
            doc.modified_at,
            now,
            feed_watermark,
        )?;
        self.observe_source(&host, true, true)?;

        // LEXICAL_INDEXED (Tantivy overlay)
        let generation = self.indexer.visible_generation() + 1;
        let index_doc = build_index_doc(
            &resource_id,
            &content_id,
            version_no,
            &canonical,
            &doc,
            &host,
            generation,
            now,
        );
        self.indexer.index_overlay(index_doc)?;
        self.record_stage(&op_key, resource_id.as_str(), IngestStage::LexicalIndexed)?;

        // GRAPH_INDEXED — recorded no-op: no graph projection wired yet.
        self.record_stage(&op_key, resource_id.as_str(), IngestStage::GraphIndexed)?;

        // Publish manifest + watermark, then VISIBLE.
        let manifest = self.indexer.publish_manifest()?;
        {
            let mut w = self.meta.lock().unwrap();
            Watermark::publish(&w, manifest.generation)?;
            w.apply_batch(&[Mutation::UpsertManifest {
                manifest_id: manifest.manifest_id.clone(),
                generation: manifest.generation as i64,
                doc_count: manifest.doc_count as i64,
                base_segments: manifest.base_segments as i64,
                created_at: now,
            }])?;
        }
        self.record_stage(&op_key, resource_id.as_str(), IngestStage::Visible)?;
        if let Some(l) = &lease {
            let _ = self.frontier.complete(l, now);
        }

        Ok(IngestOutcome::Ingested {
            url: canonical,
            resource_id: resource_id.into_string(),
            docver_id: docver_id.into_string(),
            version_no,
            content_id: content_id.into_string(),
            generation: manifest.generation,
        })
    }

    // -- internals ---------------------------------------------------------

    /// SSRF guard. The loopback exemption is strictly test-only and restricted
    /// to genuine loopback hosts; every other URL is fully validated.
    async fn guard(&self, url: &str) -> AppResult<()> {
        if self.allow_loopback && is_loopback_url(url) {
            return Ok(());
        }
        ssrf::validate_url_resolved(url).await.map(|_| ())
    }

    /// Fetch `url` unconditionally, honouring the loopback exemption.
    async fn do_fetch(&self, url: &str) -> AppResult<FetchOutcome> {
        if self.allow_loopback && is_loopback_url(url) {
            return self.fetch_local_cond(url, None, None).await;
        }
        self.fetcher.fetch(url, None, None).await
    }

    /// Conditional fetch: send stored `If-None-Match` / `If-Modified-Since`
    /// validators when known; store fresh validators on 2xx/304. A `304` is
    /// returned as status `304` with no body.
    async fn do_fetch_conditional(&self, url: &str) -> AppResult<FetchOutcome> {
        let key = canonicalize(url).unwrap_or_else(|_| url.to_string());
        if self.allow_loopback && is_loopback_url(url) {
            let prior = self.validators.get(&key);
            let (etag, lm) = prior
                .as_ref()
                .map(|v| (v.etag.clone(), v.last_modified.clone()))
                .unwrap_or((None, None));
            let o = self
                .fetch_local_cond(url, etag.as_deref(), lm.as_deref())
                .await?;
            store_validators(&self.validators, &key, &o);
            return Ok(o);
        }
        let fetched = self.fetcher.fetch_conditional(&self.validators, url).await?;
        Ok(match fetched {
            Fetched::Unchanged {
                final_url,
                validators,
            } => FetchOutcome {
                final_url,
                status: 304,
                etag: validators.etag,
                last_modified: validators.last_modified,
                body: None,
                content_type: None,
            },
            Fetched::Modified {
                final_url,
                status,
                content_type,
                body,
                validators,
            } => FetchOutcome {
                final_url,
                status,
                etag: validators.etag,
                last_modified: validators.last_modified,
                body: Some(body),
                content_type,
            },
            Fetched::Other {
                final_url,
                status,
                content_type,
            } => FetchOutcome {
                final_url,
                status,
                etag: None,
                last_modified: None,
                body: None,
                content_type,
            },
        })
    }

    /// Direct fetch used only for loopback (test) targets: same body-cap and
    /// status handling as the fetcher, minus the SSRF guard and redirect walk.
    /// Honours optional conditional validators.
    async fn fetch_local_cond(
        &self,
        url: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> AppResult<FetchOutcome> {
        let mut req = self.http.get(url);
        if let Some(tag) = etag {
            req = req.header(reqwest::header::IF_NONE_MATCH, tag);
        }
        if let Some(lm) = last_modified {
            req = req.header(reqwest::header::IF_MODIFIED_SINCE, lm);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| AppError::Network(e.to_string()))?;
        let status = resp.status();
        let etag = header_string(&resp, reqwest::header::ETAG);
        let last_modified = header_string(&resp, reqwest::header::LAST_MODIFIED);
        let content_type = header_string(&resp, reqwest::header::CONTENT_TYPE);
        let code = status.as_u16();
        if code == 304 || !status.is_success() {
            return Ok(FetchOutcome {
                final_url: url.to_string(),
                status: code,
                etag,
                last_modified,
                body: None,
                content_type,
            });
        }
        let body = search_fetcher::read_body_capped(resp, &self.cfg.limits).await?;
        Ok(FetchOutcome {
            final_url: url.to_string(),
            status: code,
            etag,
            last_modified,
            body: Some(body),
            content_type,
        })
    }

    /// Fetch + parse `robots.txt` with RFC 9309 failure semantics.
    async fn load_robots(&self, robots_url: &str, domain_id: &DomainId) -> AppResult<RobotsPolicy> {
        let fetched_at = search_core::time::now_unix_s();
        match self.do_fetch(robots_url).await {
            Ok(o) => match o.status {
                200..=299 => {
                    let body = o.body.unwrap_or_default();
                    let text = String::from_utf8_lossy(&body).to_string();
                    Ok(robots::parse_capped(
                        &text,
                        &self.cfg.user_agent,
                        fetched_at,
                        self.cfg.limits.max_robots_bytes,
                    ))
                }
                // RFC 9309: 4xx (other than 429) -> unavailable -> allow all.
                400..=499 => Ok(RobotsPolicy::unavailable(domain_id.as_str(), fetched_at)),
                // 5xx -> unreachable -> assume complete disallow.
                _ => Ok(RobotsPolicy::unreachable(domain_id.as_str(), fetched_at)),
            },
            // Fail closed on an SSRF block; a transient network error is treated
            // as unreachable (complete disallow), per RFC 9309.
            Err(AppError::SsrfBlocked(_)) => {
                Err(AppError::SsrfBlocked("robots.txt SSRF-blocked".into()))
            }
            Err(_) => Ok(RobotsPolicy::unreachable(domain_id.as_str(), fetched_at)),
        }
    }

    /// The raw `robots.txt` body for discovery (`Sitemap:` lines live here).
    async fn robots_text(&self, robots_url: &str) -> AppResult<String> {
        let o = self.do_fetch(robots_url).await?;
        if (200..300).contains(&o.status) {
            Ok(String::from_utf8_lossy(&o.body.unwrap_or_default()).to_string())
        } else {
            Ok(String::new())
        }
    }

    /// Fetch a body for discovery; `None` for a non-2xx (or 304) status.
    async fn get_body(&self, url: &str) -> AppResult<Option<FetchOutcome>> {
        let o = self.do_fetch_conditional(url).await?;
        if (200..300).contains(&o.status) && o.body.is_some() {
            Ok(Some(o))
        } else {
            Ok(None)
        }
    }

    /// Persist a sitemap row through the single-writer `Mutation` API.
    fn persist_sitemap(
        &self,
        url: &str,
        domain_id: &DomainId,
        is_index: bool,
        url_count: i64,
        fo: &FetchOutcome,
        now: i64,
    ) -> AppResult<()> {
        let mut w = self.meta.lock().unwrap();
        w.apply_batch(&[
            Mutation::UpsertDomain {
                domain_id: domain_id.as_str().to_string(),
                host: domain_id.as_str().to_string(),
            },
            Mutation::UpsertSitemap {
                sitemap_id: url.to_string(),
                url: url.to_string(),
                source_url: None,
                domain_id: Some(domain_id.as_str().to_string()),
                is_index,
                status: "OK".to_string(),
                url_count,
                etag: fo.etag.clone(),
                last_modified: fo.last_modified.clone(),
                fetched_at: Some(now),
            },
        ])
    }

    /// Persist a feed row through the single-writer `Mutation` API.
    fn persist_feed(
        &self,
        url: &str,
        domain_id: &DomainId,
        entry_count: i64,
        fo: &FetchOutcome,
        now: i64,
    ) -> AppResult<()> {
        let mut w = self.meta.lock().unwrap();
        w.apply_batch(&[
            Mutation::UpsertDomain {
                domain_id: domain_id.as_str().to_string(),
                host: domain_id.as_str().to_string(),
            },
            Mutation::UpsertFeed {
                feed_id: url.to_string(),
                url: url.to_string(),
                domain_id: Some(domain_id.as_str().to_string()),
                title: None,
                feed_type: "rss".to_string(),
                status: "OK".to_string(),
                entry_count,
                etag: fo.etag.clone(),
                last_modified: fo.last_modified.clone(),
                watermark: None,
                fetched_at: Some(now),
            },
        ])
    }

    /// Parse a feed body and push its entries into the frontier. Returns the
    /// number of entries learned. Used by both `discover` and the `crawl_one`
    /// Feed route.
    fn ingest_feed_body(
        &self,
        feed_url: &str,
        body: &[u8],
        domain_id: &DomainId,
        now: i64,
    ) -> AppResult<usize> {
        let entries = parse_feed(body, feed_url).unwrap_or_default();
        // Refresh the feed row with the observed entry count.
        {
            let mut w = self.meta.lock().unwrap();
            w.apply_batch(&[
                Mutation::UpsertDomain {
                    domain_id: domain_id.as_str().to_string(),
                    host: domain_id.as_str().to_string(),
                },
                Mutation::UpsertFeed {
                    feed_id: feed_url.to_string(),
                    url: feed_url.to_string(),
                    domain_id: Some(domain_id.as_str().to_string()),
                    title: None,
                    feed_type: "rss".to_string(),
                    status: "OK".to_string(),
                    entry_count: entries.len() as i64,
                    etag: None,
                    last_modified: None,
                    watermark: None,
                    fetched_at: Some(now),
                },
            ])?;
        }
        let mut pushed = 0usize;
        for r in entries.iter().take(MAX_DISCOVERED_RESOURCES) {
            let Ok(resource_id) = self.resource_id_for(&r.url) else {
                continue;
            };
            let entry = FrontierEntry {
                resource_id,
                url: r.url.clone(),
                domain_id: domain_id.as_str().to_string(),
                priority: Priority::Hot,
                earliest_fetch_at: 0,
                expected_value: None,
                discovered_from: Some(feed_url.to_string()),
            };
            if self.frontier.push(entry).is_ok() {
                pushed += 1;
            }
        }
        Ok(pushed)
    }

    /// Refresh the per-domain `SourceProfile` from one fetch observation.
    fn observe_source(&self, source: &str, ok: bool, changed: bool) -> AppResult<()> {
        let mut profile = self
            .load_source_profile(source)?
            .unwrap_or_else(|| SourceProfile::from_domain(source));
        profile.observe_fetch(ok, changed);
        let mut w = self.meta.lock().unwrap();
        w.apply_batch(&[Mutation::UpsertSourceProfile {
            source_id: profile.source_id.clone(),
            domain: profile.domain.clone(),
            topics: serde_json::to_string(&profile.topics).unwrap_or_else(|_| "[]".to_string()),
            entities: serde_json::to_string(&profile.entities).unwrap_or_else(|_| "[]".to_string()),
            url_patterns: serde_json::to_string(&profile.url_patterns)
                .unwrap_or_else(|_| "[]".to_string()),
            authority: profile.authority as f64,
            change_rate: profile.change_rate as f64,
            js_probability: profile.js_probability as f64,
            fetch_count: profile.fetch_count as i64,
            error_count: profile.error_count as i64,
        }])
    }

    /// Load a persisted `SourceProfile` (plain columns -> domain type).
    fn load_source_profile(&self, source_id: &str) -> AppResult<Option<SourceProfile>> {
        let w = self.meta.lock().unwrap();
        let row = w
            .conn()
            .query_row(
                "SELECT domain, topics, entities, url_patterns, authority, change_rate,
                        js_probability, fetch_count, error_count
                   FROM source_profile WHERE source_id = ?1",
                params![source_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, f64>(4)?,
                        r.get::<_, f64>(5)?,
                        r.get::<_, f64>(6)?,
                        r.get::<_, i64>(7)?,
                        r.get::<_, i64>(8)?,
                    ))
                },
            )
            .optional()
            .map_err(search_metadata::db_err)?;
        Ok(row.map(
            |(
                domain,
                topics,
                entities,
                url_patterns,
                authority,
                change_rate,
                js_probability,
                fetch_count,
                error_count,
            )| SourceProfile {
                source_id: source_id.to_string(),
                domain,
                topics: json_string_array(&topics),
                entities: json_string_array(&entities),
                url_patterns: json_string_array(&url_patterns),
                authority: authority as f32,
                change_rate: change_rate as f32,
                js_probability: js_probability as f32,
                fetch_count: fetch_count as u64,
                error_count: error_count as u64,
            },
        ))
    }

    /// Write the Stage-1 freshness columns on the resource row.
    fn persist_freshness(
        &self,
        resource_id: &ResourceId,
        published_at: Option<i64>,
        modified_at: Option<i64>,
        now: i64,
        feed_watermark: Option<i64>,
    ) -> AppResult<()> {
        let mut w = self.meta.lock().unwrap();
        w.apply_batch(&[Mutation::UpdateResourceFreshness {
            resource_id: resource_id.as_str().to_string(),
            published_at,
            modified_at,
            fetched_at: Some(now),
            verified_at: Some(now),
            feed_watermark,
        }])
    }

    /// Whether this resource was discovered from a known feed (drives
    /// `feed_watermark`).
    fn is_feed_sourced(&self, resource_id: &str) -> bool {
        let w = self.meta.lock().unwrap();
        let discovered_from: Option<String> = w
            .conn()
            .query_row(
                "SELECT discovered_from FROM frontier_entry WHERE resource_id = ?1",
                params![resource_id],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();
        let Some(from) = discovered_from else {
            return false;
        };
        let hit: Option<i64> = w
            .conn()
            .query_row("SELECT 1 FROM feed WHERE url = ?1", params![from], |r| r.get(0))
            .optional()
            .ok()
            .flatten();
        hit.is_some()
    }

    /// Idempotent journal stage recording.
    fn record_stage(&self, op_key: &OpKey, resource_id: &str, stage: IngestStage) -> AppResult<()> {
        let mut w = self.meta.lock().unwrap();
        IngestCoordinator::record(&mut w, op_key, resource_id, stage)
    }

    /// Latest `(version_no, content_id)` for a resource, if any.
    fn latest_version(&self, resource_id: &ResourceId) -> AppResult<Option<(u64, Option<String>)>> {
        let w = self.meta.lock().unwrap();
        w.conn()
            .query_row(
                "SELECT version_no, content_id FROM document_version
                 WHERE resource_id = ?1 ORDER BY version_no DESC LIMIT 1",
                params![resource_id.as_str()],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, Option<String>>(1)?)),
            )
            .optional()
            .map_err(search_metadata::db_err)
    }

    /// Write the raw blob and the normalized text under the data dir.
    fn write_content_files(
        &self,
        content_id: &ContentId,
        body: &[u8],
        text: &[u8],
    ) -> AppResult<()> {
        let hex = content_id.as_str().trim_start_matches("cnt_");
        let blob = self.cfg.blob_path(hex);
        if let Some(p) = blob.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&blob, body)?;

        let text_path = self.cfg.text_path(hex);
        if let Some(p) = text_path.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&text_path, text)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_metadata(
        &self,
        canonical: &str,
        host: &str,
        domain_id: &DomainId,
        resource_id: &ResourceId,
        docver_id: &DocVerId,
        version_no: u64,
        content_id: &ContentId,
        outcome: &FetchOutcome,
        doc: &NormalizedDoc,
        kind: &str,
        source_id: &str,
        parser_version: &str,
        now: i64,
    ) -> AppResult<()> {
        let hex = content_id.as_str().trim_start_matches("cnt_").to_string();
        let text_path = self.cfg.text_path(&hex).to_string_lossy().to_string();

        let mut w = self.meta.lock().unwrap();
        w.apply_batch(&[
            Mutation::UpsertDomain {
                domain_id: domain_id.as_str().to_string(),
                host: host.to_string(),
            },
            Mutation::UpsertResource {
                resource_id: resource_id.as_str().to_string(),
                url: canonical.to_string(),
                canonical_url: canonical.to_string(),
                domain_id: domain_id.as_str().to_string(),
                source_id: Some(source_id.to_string()),
                kind: kind.to_string(),
            },
            Mutation::RecordFetch {
                resource_id: resource_id.as_str().to_string(),
                http_status: outcome.status as i64,
                etag: outcome.etag.clone(),
                last_modified: outcome.last_modified.clone(),
                content_hash: Some(hex.clone()),
                fetched_at: now,
            },
        ])?;

        // Content must exist before its `document_version` FK resolves, so the
        // content upsert and the version insert share one atomic batch (applied
        // in order inside a single transaction).
        w.apply_batch(&[
            Mutation::UpsertContent {
                content_id: content_id.as_str().to_string(),
                exact_hash: hex,
                text_path: Some(text_path),
                byte_len: doc.text_len as i64,
            },
            Mutation::UpsertDocumentVersion {
                docver_id: docver_id.as_str().to_string(),
                resource_id: resource_id.as_str().to_string(),
                version_no: version_no as i64,
                content_id: Some(content_id.as_str().to_string()),
                title: doc.title.clone(),
                body_hash: None,
                parser_version: parser_version.to_string(),
                published_at: doc.published_at,
                modified_at: doc.modified_at,
                fetched_at: now,
            },
        ])?;
        Ok(())
    }
}

/// A normalized projection for an extracted PDF (indexed as `doc_type = pdf`).
fn pdf_normalized_doc(url: &str, text: String) -> NormalizedDoc {
    let title = Url::parse(url)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|s| s.last().map(|x| x.to_string()))
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| url.to_string());
    NormalizedDoc {
        url: url.to_string(),
        title: Some(title),
        text_len: text.len(),
        text,
        headings: Vec::new(),
        entities: Vec::new(),
        language: None,
        description: None,
        published_at: None,
        modified_at: None,
        outlinks: Vec::new(),
        doc_type: "pdf".to_string(),
    }
}

/// Build the Tantivy overlay document for a normalized page/PDF.
fn build_index_doc(
    resource_id: &ResourceId,
    content_id: &ContentId,
    version_no: u64,
    canonical: &str,
    doc: &NormalizedDoc,
    source_id: &str,
    generation: u64,
    now: i64,
) -> IndexDoc {
    IndexDoc {
        // doc_id is the delete term: one live doc per resource, versioned.
        doc_id: resource_id.as_str().to_string(),
        version: version_no,
        resource_id: resource_id.as_str().to_string(),
        work_id: None,
        content_id: Some(content_id.as_str().to_string()),
        url: canonical.to_string(),
        title: doc.title.clone().unwrap_or_default(),
        headings: doc.headings.join(" \n "),
        body: doc.text.clone(),
        entities: doc.entities.clone(),
        source_id: Some(source_id.to_string()),
        doc_type: doc.doc_type.clone(),
        language: doc.language.clone().unwrap_or_else(|| "en".to_string()),
        published_at: doc.published_at,
        modified_at: doc.modified_at,
        fetched_at: now,
        visible_generation: generation,
        tombstone: false,
    }
}

fn header_string(resp: &reqwest::Response, name: reqwest::header::HeaderName) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

fn origin_of(u: &Url) -> String {
    match (u.host_str(), u.port()) {
        (Some(host), Some(port)) => format!("{}://{}:{}", u.scheme(), host, port),
        (Some(host), None) => format!("{}://{}", u.scheme(), host),
        (None, _) => String::new(),
    }
}

/// Resolve a possibly-relative URL against the origin. Absolute URLs pass
/// through unchanged.
fn resolve_against(origin: &str, url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        return url.to_string();
    }
    match Url::parse(origin).and_then(|b| b.join(url)) {
        Ok(u) => u.to_string(),
        Err(_) => url.to_string(),
    }
}

/// Feed URLs advertised in an HTML document: explicit
/// `<link rel="alternate" type="...rss/atom...">` tags plus feed-ish hrefs
/// (`/feed`, `/rss`, `.rss`, `.atom`, `feed.xml`, ...). Deduped, in document
/// order, resolved to absolute URLs.
fn feeds_in_html(html: &str, base_url: &str) -> Vec<String> {
    use regex::Regex;

    let tag_re = Regex::new(r"(?is)<link\b[^>]*>").expect("static regex");
    let href_re = Regex::new(r#"(?is)href\s*=\s*["']([^"']+)["']"#).expect("static regex");
    let type_re = Regex::new(r#"(?is)type\s*=\s*["']([^"']+)["']"#).expect("static regex");

    let mut raw: Vec<String> = Vec::new();
    for tag in tag_re.find_iter(html).map(|m| m.as_str()) {
        let href = href_re.captures(tag).map(|c| c[1].to_string());
        let typ = type_re
            .captures(tag)
            .map(|c| c[1].to_ascii_lowercase())
            .unwrap_or_default();
        if let Some(h) = href {
            if typ.contains("rss") || typ.contains("atom") {
                raw.push(h);
            }
        }
    }
    // Feed-ish hrefs anywhere in the document (anchors included).
    for cap in href_re.captures_iter(html) {
        let href = cap[1].to_ascii_lowercase();
        let feedish = href.contains("/feed")
            || href.contains("/rss")
            || href.contains("feed.xml")
            || href.contains("atom.xml")
            || href.contains("rss.xml")
            || href.ends_with(".rss")
            || href.ends_with(".atom");
        if feedish {
            raw.push(cap[1].to_string());
        }
    }

    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for href in raw {
        let abs = resolve_against(base_url, href.trim());
        if abs.starts_with("http") && seen.insert(abs.clone()) {
            out.push(abs);
        }
    }
    out
}

fn is_loopback_url(raw: &str) -> bool {
    let Ok(u) = Url::parse(raw) else {
        return false;
    };
    match u.host_str() {
        Some("localhost") => true,
        Some(h) => {
            let bare = h.trim_start_matches('[').trim_end_matches(']');
            bare.parse::<IpAddr>()
                .map(|ip| ip.is_loopback())
                .unwrap_or(false)
        }
        None => false,
    }
}

/// Map an HTTP status onto the shared error taxonomy (no panics).
fn classify_status(status: u16, url: &str) -> AppError {
    match status {
        404 | 410 => AppError::NotFound(format!("HTTP {status} for {url}")),
        429 => AppError::LimitExceeded(format!("HTTP 429 (rate limited) for {url}")),
        400..=499 => AppError::Network(format!("HTTP {status} (client error) for {url}")),
        _ => AppError::Network(format!("HTTP {status} (server error) for {url}")),
    }
}

/// Store fresh validators after a loopback fetch: refresh the timestamp on a
/// 304, replace on a 2xx.
fn store_validators(store: &dyn ValidatorStore, key: &str, o: &FetchOutcome) {
    match o.status {
        304 => {
            let mut v = store.get(key).unwrap_or_default();
            v.fetched_at = search_core::time::now_unix_s();
            store.put(key, v);
        }
        200..=299 => {
            store.put(
                key,
                Validators::new(o.etag.clone(), o.last_modified.clone()),
            );
        }
        _ => {}
    }
}

/// Parse a JSON-encoded string array (metadata stores arrays as TEXT JSON).
fn json_string_array(s: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(s).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Source-first search
// ---------------------------------------------------------------------------

/// A [`Searcher`] that runs the source-first lane from `search_retrieval` and
/// passes its candidates through, tagging them with a `source_first` signal so
/// `/search` results carry source-first provenance.
///
/// It holds only the shared indexer and the metadata path (opening a read-only
/// connection per query), so it stays `Send + Sync`.
pub struct SourceFirstSearcher {
    inner: IndexerSearcher,
    db_path: PathBuf,
}

impl SourceFirstSearcher {
    pub fn new(indexer: Arc<Indexer>, db_path: PathBuf) -> Self {
        Self {
            inner: IndexerSearcher::new(indexer),
            db_path,
        }
    }

    /// Source ids whose profile matches the query (domain/topic/entity appears
    /// in the query text). When nothing matches — e.g. profiles with no topics
    /// yet — it falls back to **all** known sources, bounded by
    /// [`MAX_SOURCE_FIRST_SOURCES`], so the lane is still exercised
    /// deterministically. A DB read failure degrades to an empty list
    /// (global-only).
    fn matching_sources(&self, query: &str) -> Vec<String> {
        let Ok(reader) = MetadataReader::from_path(&self.db_path) else {
            return Vec::new();
        };
        let conn = reader.conn();
        let mut stmt = match conn.prepare("SELECT source_id, domain, topics, entities FROM source_profile")
        {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        }) {
            Ok(rows) => rows,
            Err(_) => return Vec::new(),
        };

        let q = query.to_ascii_lowercase();
        let mut all = Vec::new();
        let mut matched = Vec::new();
        for (sid, domain, topics, entities) in rows.flatten() {
            all.push(sid.clone());
            let mut needles = vec![domain];
            needles.extend(json_string_array(&topics));
            needles.extend(json_string_array(&entities));
            if needles
                .iter()
                .any(|n| !n.is_empty() && q.contains(&n.to_ascii_lowercase()))
            {
                matched.push(sid);
            }
        }
        let chosen = if matched.is_empty() { all } else { matched };
        chosen.into_iter().take(MAX_SOURCE_FIRST_SOURCES).collect()
    }
}

impl Searcher for SourceFirstSearcher {
    fn search(
        &self,
        q: &str,
        limit: usize,
        filters: Option<&search_core::SearchFilters>,
    ) -> AppResult<Vec<IndexHit>> {
        // Pull a generous global window so source-scoped hits are present for
        // signal tagging (and so `execute` still has enough to rank).
        let base_limit = limit.saturating_mul(4).max(limit);
        let mut hits = self.inner.search(q, base_limit, filters)?;

        let source_ids = self.matching_sources(q);
        if !source_ids.is_empty() {
            let adapter = PipelineLaneSearcher {
                inner: &self.inner,
                filters: filters.cloned(),
            };
            let lane = SourceFirst::new(source_ids);
            if let Ok(cands) = lane.retrieve(&adapter, q, limit) {
                let mut index_of: HashMap<String, usize> = HashMap::new();
                for (i, h) in hits.iter().enumerate() {
                    index_of.insert(h.doc_id.clone(), i);
                }
                for cand in cands {
                    if cand.lane != Lane::SourceFirst {
                        continue;
                    }
                    let signal = Signal {
                        name: "source_first".to_string(),
                        value: cand.score,
                        note: format!(
                            "source-scoped match ({})",
                            cand.source_id.clone().unwrap_or_else(|| "?".to_string())
                        ),
                    };
                    match index_of.get(&cand.doc_id).copied() {
                        Some(i) => {
                            let h = &mut hits[i];
                            h.signals.push(signal);
                            if !h.lane.contains("source_first") {
                                h.lane = format!("{}+source_first", h.lane);
                            }
                        }
                        // The candidate did not match the query globally, so it
                        // is not a query hit. Source scoping only *re-ranks*
                        // genuine query matches toward matching sources; it must
                        // never inject a document that merely belongs to a
                        // matching source. Drop it.
                        None => {}
                    }
                }
            }
        }

        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.doc_id.cmp(&b.doc_id))
        });
        Ok(hits)
    }

    fn manifest(&self) -> AppResult<search_core::IndexManifest> {
        self.inner.manifest()
    }

    fn degradations(&self) -> Vec<String> {
        self.inner.degradations()
    }
}

/// Adapter driving the retrieval [`LaneSearcher`] over the real index.
struct PipelineLaneSearcher<'a> {
    inner: &'a IndexerSearcher,
    filters: Option<search_core::SearchFilters>,
}

impl LaneSearcher for PipelineLaneSearcher<'_> {
    fn search(
        &self,
        query: &str,
        limit: usize,
        source_id: Option<&str>,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        let mut filters = self.filters.clone().unwrap_or_default();
        if let Some(s) = source_id {
            filters.source_id = Some(s.to_string());
        }
        let has_any = filters.doc_type.is_some()
            || filters.source_id.is_some()
            || filters.language.is_some()
            || filters.after.is_some()
            || filters.before.is_some();
        let filters_ref = if has_any { Some(&filters) } else { None };
        let hits = self
            .inner
            .search(query, limit, filters_ref)
            .map_err(RetrievalError::index)?;
        Ok(hits
            .into_iter()
            .map(|h| Candidate {
                doc_id: h.doc_id,
                score: h.score,
                lane: Lane::Bm25,
                source_id: h.source_id,
                title: Some(h.title),
                url: Some(h.url),
            })
            .collect())
    }
}

/// Minimal hit for a source-lane candidate that the global window missed
/// (reserved for a future lane that returns candidates the global window
/// excluded; not currently used since source scoping only re-ranks).
#[allow(dead_code)]
fn synth_hit(cand: Candidate, signal: Signal) -> IndexHit {
    let doc_id = cand.doc_id.clone();
    IndexHit {
        resource_id: doc_id.clone(),
        doc_id,
        version: 0,
        work_id: None,
        content_id: None,
        url: cand.url.unwrap_or_default(),
        title: cand.title.unwrap_or_default(),
        snippet: String::new(),
        source_id: cand.source_id,
        doc_type: None,
        language: None,
        published_at: None,
        modified_at: None,
        fetched_at: 0,
        lane: "source_first".to_string(),
        provenance: Provenance::Overlay,
        score: cand.score,
        signals: vec![signal],
    }
}

#[cfg(test)]
mod tests {
    /// The single-writer contract: every content/manifest write must go through
    /// the `Mutation` API, never a raw SQL statement smuggled onto
    /// `MetadataWriter::conn()`. This compiles the driver's own source in and
    /// asserts the forbidden raw SQL is absent.
    #[test]
    fn writes_content_and_manifest_only_via_mutation_api() {
        let src = include_str!("pipeline.rs");
        // Built from parts so this test's own literals are not self-matching.
        let content_insert = concat!("INSERT INTO ", "content");
        let manifest_insert = concat!("INSERT INTO ", "manifest");
        assert!(
            !src.contains(content_insert),
            "content must be written via Mutation::UpsertContent, not raw SQL"
        );
        assert!(
            !src.contains(manifest_insert),
            "the manifest row must be published via Mutation::UpsertManifest, not raw SQL"
        );
        assert!(
            src.contains("Mutation::UpsertContent"),
            "the content path uses the single-writer API"
        );
        assert!(
            src.contains("Mutation::UpsertManifest"),
            "the manifest path uses the single-writer API"
        );
    }

    #[test]
    fn feeds_in_html_finds_alternate_and_feedish_links() {
        let html = r#"<html><head>
            <link rel="alternate" type="application/rss+xml" href="/feed.xml">
            <link rel="stylesheet" href="/style.css">
            <a href="/blog/rss.xml">rss</a>
        </head></html>"#;
        let feeds = super::feeds_in_html(html, "https://example.com/");
        assert!(feeds.contains(&"https://example.com/feed.xml".to_string()));
        assert!(feeds.contains(&"https://example.com/blog/rss.xml".to_string()));
        assert!(!feeds.iter().any(|f| f.contains("style.css")));
    }

    #[test]
    fn resolve_against_handles_relative_and_absolute() {
        assert_eq!(
            super::resolve_against("https://e.com", "/sitemap.xml"),
            "https://e.com/sitemap.xml"
        );
        assert_eq!(
            super::resolve_against("https://e.com", "https://other.com/s.xml"),
            "https://other.com/s.xml"
        );
    }
}
