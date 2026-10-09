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
use serde::Serialize;
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
use search_frontier::{Frontier, Lease};
use search_indexer::{IndexDoc, Indexer, IndexerConfig};
use search_ingest::{IngestCoordinator, IngestStage, Watermark};
use search_metadata::{MetadataReader, MetadataWriter, Mutation};
use search_parser::NormalizedDoc;
use search_pdf::PdfLimits;
use search_retrieval::{Candidate, Lane, LaneSearcher, RetrievalError, SourceFirst};
use search_router::{classify, Route};
use search_scheduler::{
    change_probability, classify_source, is_time_sensitive, Activation, ActivationScheduler,
    BudgetLedger, SchedulerConfig, SourceDue,
};
use search_sources::SourceProfile;
use search_trace::PolicyTrace;

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

/// Default Stage-3 refresh budget (units == live refresh fetches) charged before
/// dispatch. Generous enough for a long crawl session; overridable per pipeline.
pub const DEFAULT_REFRESH_BUDGET: u64 = 10_000;
/// P(changed) at which the learned change model schedules a source for refresh
/// (the inverse of [`change_probability`] gives the due horizon).
const DUE_PROB: f64 = 0.5;
/// Cap on the candidate URLs offered to a single query-triggered refresh.
const MAX_REFRESH_CANDIDATES: usize = 64;

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

/// Summary of one refresh pass (one [`Pipeline::run_refresh_tick`] or
/// [`Pipeline::query_refresh`]).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct RefreshReport {
    /// Activations the scheduler admitted (charged + given a fan-out slot).
    pub admitted: usize,
    /// Admitted activations skipped because the dispatch budget refused them.
    pub skipped: usize,
    /// Activations whose fetch completed (a 304 still counts here).
    pub fetched: usize,
    /// Completed fetches that produced no new document (304 or identical bytes).
    pub unchanged: usize,
    /// Completed fetches that produced a new, searchable document version.
    pub new_documents: usize,
    /// Early aborts whose budget unit was refunded.
    pub refunded: usize,
    /// URLs actually attempted, in dispatch order.
    pub activations: Vec<String>,
    /// `(url, error)` for aborted dispatches.
    pub errors: Vec<(String, String)>,
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
    /// Validators for *discovery* fetches only (robots/sitemaps/feeds/the seed
    /// page). Kept separate so a discovery fetch can never make the resource
    /// fetch report `304`/`Unchanged` for a version that was never ingested.
    discovery_validators: MemoryValidators,
    /// Test-only: exempt loopback targets from the SSRF guard so a hermetic
    /// local server can be crawled. Production (`open`) leaves this `false`.
    allow_loopback: bool,
    /// Stage-3 freshness scheduler (background due work + P0 query refresh).
    scheduler: ActivationScheduler,
    /// Stage-3 dispatch budget. Charged *before* a live refresh fetch is
    /// attempted and refunded on an early abort. It mirrors the scheduler's
    /// internal charge (which `ActivationScheduler` does not expose), so the
    /// crawl loop can implement refund-on-abort itself.
    dispatch_budget: BudgetLedger,
    /// Optional real `search_trace::PolicyTrace` (STAGE8). `None` (the default)
    /// keeps the hermetic no-op emitter; a JSONL path configured through
    /// `SEARCH_TRACE_PATH` (or [`Pipeline::set_trace_jsonl`]) makes every
    /// `deep_search` append canonical trace events.
    pub(crate) trace: Option<Arc<PolicyTrace>>,
}

/// Inputs to [`Pipeline::commit_document`]: the post-fetch half of
/// [`Pipeline::crawl_one`], factored out so a document can be indexed without a
/// live fetch (see [`Pipeline::import_record`]).
struct CommitDoc<'a> {
    canonical: &'a str,
    host: &'a str,
    domain_id: &'a DomainId,
    resource_id: &'a ResourceId,
    body: &'a [u8],
    doc: &'a search_parser::NormalizedDoc,
    kind: &'a str,
    parser_version: &'a str,
    outcome: &'a FetchOutcome,
    now: i64,
    lease: Option<&'a Lease>,
    /// Whether to record a live-host observation (`self.observe_source`). `true`
    /// for a crawl; `false` for an imported record, which was never fetched from
    /// a live host.
    observe: bool,
}

/// A record that arrived out-of-band (a dataset pack / an import) rather than by
/// fetching a live URL.
#[derive(Debug, Clone)]
pub struct ImportRecord {
    pub url: String,
    pub title: Option<String>,
    pub text: String,
    pub source: String,
    pub published_at: Option<i64>,
    pub extras: std::collections::BTreeMap<String, String>,
}

impl Pipeline {
    /// Open the pipeline in production mode: the SSRF guard blocks loopback.
    pub fn open(cfg: &Config) -> AppResult<Self> {
        Self::open_with(cfg, false)
    }

    /// Open the pipeline, optionally allowing loopback targets (tests only).
    /// Uses [`SchedulerConfig::default`] and [`DEFAULT_REFRESH_BUDGET`].
    pub fn open_with(cfg: &Config, allow_loopback: bool) -> AppResult<Self> {
        Self::open_with_scheduler(
            cfg,
            allow_loopback,
            SchedulerConfig::default(),
            DEFAULT_REFRESH_BUDGET,
        )
    }

    /// Open the pipeline with an explicit Stage-3 [`SchedulerConfig`] and
    /// dispatch-budget capacity. The default build stays hermetic and offline:
    /// the scheduler performs no work until `run_refresh_tick`/`query_refresh`
    /// is called.
    pub fn open_with_scheduler(
        cfg: &Config,
        allow_loopback: bool,
        scheduler_cfg: SchedulerConfig,
        budget_capacity: u64,
    ) -> AppResult<Self> {
        Self::open_inner(cfg, allow_loopback, scheduler_cfg, budget_capacity, false)
    }

    /// Open **read-only**: no Tantivy writer, so no lock on the index directory.
    ///
    /// This is what lets the server run beside a crawler (or any other writer) against one data
    /// root. Queries are unaffected; refresh and ingest cannot run, and say so.
    pub fn open_read_only(cfg: &Config, allow_loopback: bool) -> AppResult<Self> {
        Self::open_inner(
            cfg,
            allow_loopback,
            SchedulerConfig::default(),
            DEFAULT_REFRESH_BUDGET,
            true,
        )
    }

    fn open_inner(
        cfg: &Config,
        allow_loopback: bool,
        scheduler_cfg: SchedulerConfig,
        budget_capacity: u64,
        read_only: bool,
    ) -> AppResult<Self> {
        cfg.assert_not_synced().map_err(AppError::Config)?;
        cfg.ensure_dirs()?;
        cfg.limits.validate().map_err(AppError::Config)?;

        // Metadata single writer (runs migrations on open).
        let writer = MetadataWriter::from_path(&cfg.meta_db())?;
        let frontier = Frontier::new(cfg.meta_db());
        let fetcher = Fetcher::new(cfg.limits.clone(), cfg.user_agent.clone());
        let index_cfg = if read_only {
            IndexerConfig::read_only(cfg.data_dir.join("tantivy"))
        } else {
            IndexerConfig::new(cfg.data_dir.join("tantivy"))
        };
        let indexer = Arc::new(Indexer::open(index_cfg)?);
        let http = reqwest::Client::builder()
            .user_agent(cfg.user_agent.clone())
            // Redirects are not auto-followed; the loopback path is direct.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| AppError::Internal(format!("failed to build http client: {e}")))?;

        // STAGE8 trace sink: only when explicitly configured. A missing or
        // unopenable path degrades to the no-op emitter (with a warning) rather
        // than failing startup.
        let trace = match std::env::var("SEARCH_TRACE_PATH") {
            Ok(path) if !path.trim().is_empty() => {
                match search_trace::JsonlTraceSink::new(path.clone()) {
                    Ok(sink) => Some(Arc::new(PolicyTrace::new(Box::new(sink)))),
                    Err(e) => {
                        tracing::warn!(path = %path, error = %e, "trace sink unavailable; continuing without traces");
                        None
                    }
                }
            }
            _ => None,
        };

        Ok(Self {
            cfg: cfg.clone(),
            meta: Mutex::new(writer),
            frontier,
            fetcher,
            http,
            indexer,
            owner: format!("agent0-{}", std::process::id()),
            validators: MemoryValidators::new(),
            discovery_validators: MemoryValidators::new(),
            allow_loopback,
            scheduler: ActivationScheduler::new(scheduler_cfg, BudgetLedger::new(budget_capacity)),
            dispatch_budget: BudgetLedger::new(budget_capacity),
            trace,
        })
    }

    /// Configure a real `search_trace::PolicyTrace` JSONL sink: every subsequent
    /// `deep_search` appends its phase events to `path` (created, appending if
    /// present). Additive; the default build keeps the no-op emitter. Returns the
    /// I/O error if the file cannot be opened.
    pub fn set_trace_jsonl(&mut self, path: impl Into<PathBuf>) -> AppResult<()> {
        let sink = search_trace::JsonlTraceSink::new(path.into())
            .map_err(|e| AppError::Internal(format!("failed to open trace sink: {e}")))?;
        self.trace = Some(Arc::new(PolicyTrace::new(Box::new(sink))));
        Ok(())
    }

    /// Whether this pipeline was opened without an index writer (no lock, no refresh).
    pub fn is_read_only(&self) -> bool {
        self.indexer.is_read_only()
    }

    /// The searcher the HTTP API should be wired to (real Tantivy indexer behind
    /// the source-first lane).
    pub fn searcher(&self) -> Arc<dyn Searcher + Send + Sync> {
        Arc::new(SourceFirstSearcher::new(
            self.indexer.clone(),
            self.cfg.meta_db(),
        ))
    }

    /// Every live document in the index, for export into a portable pack.
    ///
    /// Thin accessor over [`Indexer::all_docs`]: the Tantivy schema STOREs
    /// `url`/`title`/`body`/`source_id`/`published_at`, so no blob-store read is
    /// needed. `doc_id`-sorted and tombstone-free, so an export is reproducible.
    pub fn all_docs(&self) -> AppResult<Vec<IndexDoc>> {
        self.indexer.all_docs()
    }

    /// Deterministic resource id for a URL (canonicalized first), matching the
    /// id `crawl_one` uses.
    pub fn resource_id_for(&self, url: &str) -> AppResult<String> {
        let canonical = canonicalize(url)
            .map_err(|e| AppError::Invalid(format!("invalid url {url:?}: {e}")))?;
        Ok(ResourceId::from_canonical_url(&canonical).into_string())
    }

    /// Run `f` against the single-writer metadata connection while holding the
    /// lock. Used by read-side queries (e.g. `read_doc`) that must not race the
    /// writer.
    pub(crate) fn with_meta_conn<T>(
        &self,
        f: impl FnOnce(&rusqlite::Connection) -> AppResult<T>,
    ) -> AppResult<T> {
        let w = self.meta.lock().unwrap();
        f(w.conn())
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
        let resp = search_api::execute(&searcher, &req)?;
        tracing::info!(
            target: "agent0::search",
            query_len = q.len(),
            limit,
            hits = resp.results.len(),
            timing_ms = resp.timing_ms,
            "search served"
        );
        Ok(resp)
    }

    // --------------------------------------------------------------- Stage 3

    /// The Stage-3 freshness scheduler (background due work + P0 query refresh).
    pub fn scheduler(&self) -> &ActivationScheduler {
        &self.scheduler
    }

    /// Units charged on the dispatch ledger so far (mirrors scheduler admission).
    pub fn budget_spent(&self) -> u64 {
        self.dispatch_budget.spent()
    }

    /// Units still available on the dispatch ledger.
    pub fn budget_remaining(&self) -> u64 {
        self.dispatch_budget.remaining()
    }

    /// Build background `SourceDue` candidates from what the pipeline knows:
    /// every indexed resource, classified HOT/WARM/COLD from its source
    /// profile's observed change rate, with a `next_allowed_at` derived from the
    /// learned change model ([`change_probability`]) and bounded by the class
    /// horizon so HOT sources come due before WARM before COLD.
    pub fn due_sources(&self, now: i64) -> AppResult<Vec<SourceDue>> {
        let cfg = self.scheduler.config().clone();
        let rows: Vec<(String, f64, f64, i64)> = {
            let w = self.meta.lock().unwrap();
            let mut stmt = w
                .conn()
                .prepare(
                    "SELECT r.canonical_url,
                            COALESCE(sp.change_rate, 0.0),
                            COALESCE(sp.authority, 0.5),
                            COALESCE(r.verified_at, r.fetched_at, r.last_fetch, 0)
                       FROM resource r
                       LEFT JOIN source_profile sp ON sp.source_id = r.source_id",
                )
                .map_err(search_metadata::db_err)?;
            let mapped = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, f64>(1)?,
                        r.get::<_, f64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                })
                .map_err(search_metadata::db_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(search_metadata::db_err)?;
            mapped
        };

        let mut out = Vec::with_capacity(rows.len());
        for (url, rate, authority, last_fetch) in rows {
            let rate = rate as f32;
            let class = classify_source(rate, authority as f32);
            let horizon = cfg.max_age_secs(class);
            let age = now.saturating_sub(last_fetch).max(0) as f64;
            // The learned change model: P(changed) at this age. A source whose
            // model gives no change signal waits a full class horizon; otherwise
            // it is due once the model's P(changed) reaches DUE_PROB, which is
            // the inverse of `change_probability`.
            let p = change_probability(rate as f64, age);
            let model_secs = if p > 0.0 {
                secs_until_prob(rate as f64, DUE_PROB)
                    .map(|s| s.round() as i64)
                    .unwrap_or(horizon)
            } else {
                horizon
            };
            let secs = model_secs.clamp(1, horizon);
            out.push(SourceDue {
                origin: origin_of_url(&url),
                class,
                change_rate: rate,
                last_fetch,
                next_allowed_at: last_fetch.saturating_add(secs),
                url,
            });
        }
        Ok(out)
    }

    /// Run one background refresh tick: the scheduler decides the due work
    /// (HOT before WARM before COLD, bounded per-origin and total fan-out) and
    /// each returned activation is dispatched through the conditional-refresh
    /// path (`crawl_one`, where a 304 is `Unchanged`). Returns a [`RefreshReport`].
    pub async fn run_refresh_tick(
        &self,
        sources: &[SourceDue],
        now: i64,
    ) -> AppResult<RefreshReport> {
        if self.is_read_only() {
            return Ok(RefreshReport::default());
        }
        let activations = self.scheduler.due(sources, now);
        let mut report = RefreshReport {
            admitted: activations.len(),
            ..Default::default()
        };
        for act in &activations {
            self.dispatch_activation(act, &mut report).await;
        }
        Ok(report)
    }

    /// Query-triggered P0 refresh. For a **time-sensitive** query, spawn up to
    /// `query_refresh_limit` P0 activations over candidates the pipeline knows
    /// (frontier URLs not yet indexed) and dispatch them. A query that is not
    /// time-sensitive performs no refresh work and issues zero fetches.
    pub async fn query_refresh(&self, q: &str, now: i64) -> AppResult<RefreshReport> {
        let mut report = RefreshReport::default();
        // A read-only pipeline cannot write the index; report that as a no-op rather than
        // failing the search that asked for freshness.
        if self.is_read_only() {
            return Ok(report);
        }
        if !is_time_sensitive(q, now) {
            return Ok(report);
        }
        let candidates = self.refresh_candidates()?;
        let activations = self.scheduler.on_query(q, &candidates, now);
        report.admitted = activations.len();
        for act in &activations {
            self.dispatch_activation(act, &mut report).await;
        }
        Ok(report)
    }

    /// Search with a live-refresh preamble: when the query is time-sensitive a
    /// P0 refresh runs first, so any newly fetched document is scored against
    /// this query and merged into the results from the **live overlay** before
    /// any global reindex. Base segments stay immutable; the overlay shadows
    /// their versions and is gated by the published watermark.
    pub async fn search_with_refresh(
        &self,
        q: &str,
        limit: usize,
        now: i64,
    ) -> AppResult<SearchResponse> {
        let _ = self.query_refresh(q, now).await?;
        self.search(q, limit).await
    }

    /// Candidate `(url, origin)` pairs for a P0 refresh: frontier URLs the
    /// pipeline knows but has not yet indexed (no `document_version` row).
    fn refresh_candidates(&self) -> AppResult<Vec<(String, String)>> {
        let urls: Vec<String> = {
            let w = self.meta.lock().unwrap();
            let mut stmt = w
                .conn()
                .prepare(
                    "SELECT f.url FROM frontier_entry f
                      WHERE NOT EXISTS (
                            SELECT 1 FROM document_version dv
                             WHERE dv.resource_id = f.resource_id)
                      GROUP BY f.url
                      ORDER BY MIN(f.priority) ASC, MIN(f.earliest_fetch_at) ASC, f.url ASC
                      LIMIT ?1",
                )
                .map_err(search_metadata::db_err)?;
            let mapped = stmt
                .query_map(params![MAX_REFRESH_CANDIDATES as i64], |r| r.get::<_, String>(0))
                .map_err(search_metadata::db_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(search_metadata::db_err)?;
            mapped
        };
        Ok(urls
            .into_iter()
            .map(|u| {
                let origin = origin_of_url(&u);
                (u, origin)
            })
            .filter(|(_, o)| !o.is_empty())
            .collect())
    }

    /// Dispatch one scheduler activation through the crawl loop. The scheduler
    /// already charged its ledger and reserved a fan-out slot before returning
    /// the activation; here the pipeline charges its own dispatch ledger
    /// **before** attempting the fetch, runs the conditional refresh, and
    /// releases the slot. An early abort (guard / robots / network failure)
    /// refunds the charge.
    async fn dispatch_activation(&self, act: &Activation, report: &mut RefreshReport) {
        // Charge before dispatch: a refused charge means no fetch is attempted.
        if !self.dispatch_budget.try_charge(1) {
            report.skipped += 1;
            tracing::warn!(target: "agent0::refresh", url = %redact(&act.url), "refresh skipped: dispatch budget exhausted");
            self.scheduler.release(&act.url, &act.origin);
            return;
        }
        report.activations.push(act.url.clone());
        tracing::info!(target: "agent0::refresh", url = %redact(&act.url), priority = ?act.priority, "dispatching refresh activation");
        match self.crawl_one(&act.url).await {
            Ok(outcome) => {
                report.fetched += 1;
                if outcome.is_new_document() {
                    report.new_documents += 1;
                }
                if outcome.is_unchanged() {
                    report.unchanged += 1;
                }
            }
            Err(e) => {
                // Early abort: return the budget unit and free the slot.
                self.dispatch_budget.refund(1);
                report.refunded += 1;
                report.errors.push((act.url.clone(), e.to_string()));
            }
        }
        self.scheduler.release(&act.url, &act.origin);
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

        tracing::debug!(target: "agent0::crawl", url = %redact(&canonical), "crawl_one start");

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
            tracing::info!(target: "agent0::crawl", url = %redact(&canonical), "robots.txt denied crawl");
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
            tracing::info!(target: "agent0::crawl", url = %redact(&canonical), "not modified (304): reusing previous version");
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
            tracing::debug!(target: "agent0::crawl", url = %redact(&canonical), route = "feed", discovered, "routed response to feed");
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

        // (7) build the normalized doc according to the route, then hand the
        // fetched bytes + parsed document to the network-free commit half.
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

        // A page that normalizes to no text is not a document: an empty body (a bot wall, a
        // JS-only shell, a truncated transfer) must never enter the index. This mirrors both the
        // PDF branch above and the import-time invariant `empty record text cannot be indexed`,
        // and it keeps a 10K-document corpus from filling with zero-content rows.
        if doc.text.trim().is_empty() {
            self.observe_source(&host, true, true)?;
            if let Some(l) = &lease {
                let _ = self.frontier.complete(l, now);
            }
            return Ok(IngestOutcome::Routed {
                url: canonical,
                resource_id: resource_id.into_string(),
                route: format!("{kind}-no-text"),
                discovered: 0,
            });
        }

        self.commit_document(CommitDoc {
            canonical: &canonical,
            host: &host,
            domain_id: &domain_id,
            resource_id: &resource_id,
            body: &body,
            doc: &doc,
            kind,
            parser_version,
            outcome: &outcome,
            now,
            lease: lease.as_ref(),
            observe: true,
        })
    }

    /// The post-fetch half of [`Pipeline::crawl_one`]: given fetched bytes, a
    /// body-derived `ContentId` and a parsed document, run stages
    /// CONTENT->VERSION->BLOB_WRITTEN->METADATA_COMMITTED->freshness->
    /// LEXICAL_INDEXED->GRAPH_INDEXED->publish->VISIBLE and return the outcome.
    /// Performs no network I/O.
    fn commit_document(&self, c: CommitDoc<'_>) -> AppResult<IngestOutcome> {
        // ContentId from the exact bytes + idempotency check.
        let content_id = ContentId::from_bytes(c.body);
        let existing = self.latest_version(c.resource_id)?;
        if let Some((vno, Some(exist_cid))) = &existing {
            if exist_cid == content_id.as_str() {
                let op_key = OpKey::from_parts(c.resource_id, 0, *vno);
                self.record_stage(&op_key, c.resource_id.as_str(), IngestStage::Visible)?;
                if c.observe {
                    self.observe_source(c.host, true, false)?;
                }
                self.persist_freshness(c.resource_id, None, None, c.now, None)?;
                if let Some(l) = c.lease {
                    let _ = self.frontier.complete(l, c.now);
                }
                let dv = DocVerId::from_parts(c.resource_id, *vno).into_string();
                let rid = c.resource_id.clone().into_string();
                tracing::info!(target: "agent0::crawl", url = %redact(c.canonical), "content unchanged: no new document version");
                return Ok(IngestOutcome::Unchanged {
                    url: c.canonical.to_string(),
                    resource_id: rid,
                    docver_id: dv,
                    version_no: *vno,
                    content_id: content_id.into_string(),
                    generation: self.indexer.visible_generation(),
                });
            }
        }
        let version_no = existing.as_ref().map(|(v, _)| v + 1).unwrap_or(1);
        let docver_id = DocVerId::from_parts(c.resource_id, version_no);
        let op_key = OpKey::from_parts(c.resource_id, 0, version_no);

        // FETCHED + NORMALIZED
        self.record_stage(&op_key, c.resource_id.as_str(), IngestStage::Fetched)?;
        self.record_stage(&op_key, c.resource_id.as_str(), IngestStage::Normalized)?;

        // BLOB_WRITTEN
        self.write_content_files(&content_id, c.body, c.doc.text.as_bytes())?;
        self.record_stage(&op_key, c.resource_id.as_str(), IngestStage::BlobWritten)?;

        // METADATA_COMMITTED (resource kind, source id, parser version)
        self.commit_metadata(
            c.canonical,
            c.host,
            c.domain_id,
            c.resource_id,
            &docver_id,
            version_no,
            &content_id,
            c.outcome,
            c.doc,
            c.kind,
            c.host,
            c.parser_version,
            c.now,
        )?;
        self.record_stage(&op_key, c.resource_id.as_str(), IngestStage::MetadataCommitted)?;

        // Stage-1 freshness columns + per-domain source profile. `persist_freshness`
        // stays unconditional: an imported record still needs the resource row's
        // fetched/verified columns populated so it behaves like any other resource.
        let feed_watermark = if self.is_feed_sourced(c.resource_id.as_str()) {
            Some(c.doc.published_at.unwrap_or(c.now))
        } else {
            None
        };
        self.persist_freshness(
            c.resource_id,
            c.doc.published_at,
            c.doc.modified_at,
            c.now,
            feed_watermark,
        )?;
        if c.observe {
            self.observe_source(c.host, true, true)?;
        }

        // LEXICAL_INDEXED (Tantivy overlay)
        let generation = self.indexer.visible_generation() + 1;
        let index_doc = build_index_doc(
            c.resource_id,
            &content_id,
            version_no,
            c.canonical,
            c.doc,
            c.host,
            generation,
            c.now,
        );
        self.indexer.index_overlay(index_doc)?;
        tracing::debug!(
            target: "agent0::index",
            doc_id = c.resource_id.as_str(),
            version = version_no,
            generation,
            "indexed overlay document"
        );
        self.record_stage(&op_key, c.resource_id.as_str(), IngestStage::LexicalIndexed)?;

        // GRAPH_INDEXED — recorded no-op: no graph projection wired yet.
        self.record_stage(&op_key, c.resource_id.as_str(), IngestStage::GraphIndexed)?;

        // Publish manifest + watermark, then VISIBLE.
        let manifest = self.indexer.publish_manifest()?;
        tracing::info!(
            target: "agent0::index",
            generation = manifest.generation,
            doc_count = manifest.doc_count,
            "published index manifest"
        );
        {
            let mut w = self.meta.lock().unwrap();
            Watermark::publish(&w, manifest.generation)?;
            w.apply_batch(&[Mutation::UpsertManifest {
                manifest_id: manifest.manifest_id.clone(),
                generation: manifest.generation as i64,
                doc_count: manifest.doc_count as i64,
                base_segments: manifest.base_segments as i64,
                created_at: c.now,
            }])?;
        }
        self.record_stage(&op_key, c.resource_id.as_str(), IngestStage::Visible)?;
        if let Some(l) = c.lease {
            let _ = self.frontier.complete(l, c.now);
        }

        tracing::info!(
            target: "agent0::crawl",
            url = %redact(c.canonical),
            version = version_no,
            kind = c.kind,
            "ingested new document version"
        );
        Ok(IngestOutcome::Ingested {
            url: c.canonical.to_string(),
            resource_id: c.resource_id.clone().into_string(),
            docver_id: docver_id.into_string(),
            version_no,
            content_id: content_id.into_string(),
            generation: manifest.generation,
        })
    }

    /// Index an already-fetched record with NO network I/O: no SSRF guard, no
    /// robots fetch, no frontier lease. Idempotent by content: importing the
    /// same text twice yields [`IngestOutcome::Unchanged`] the second time.
    pub fn import_record(&self, rec: &ImportRecord) -> AppResult<IngestOutcome> {
        if rec.text.trim().is_empty() {
            return Err(AppError::Invalid(
                "empty record text cannot be indexed".to_string(),
            ));
        }
        let canonical = canonicalize(&rec.url)
            .map_err(|e| AppError::Invalid(format!("invalid url {:?}: {e}", rec.url)))?;
        let parsed = Url::parse(&canonical)
            .map_err(|e| AppError::Invalid(format!("invalid canonical url {canonical:?}: {e}")))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| AppError::Invalid(format!("url {canonical:?} has no host")))?
            .to_string();
        let domain_id = DomainId::from_host(&host);
        let resource_id = ResourceId::from_canonical_url(&canonical);

        let body = rec.text.as_bytes();
        let doc = NormalizedDoc {
            url: canonical.clone(),
            title: rec.title.clone(),
            text: rec.text.clone(),
            headings: Vec::new(),
            entities: Vec::new(),
            language: None,
            description: None,
            published_at: rec.published_at,
            modified_at: None,
            outlinks: Vec::new(),
            doc_type: "record".to_string(),
            text_len: rec.text.len(),
        };
        let outcome = FetchOutcome {
            final_url: canonical.clone(),
            status: 200,
            etag: None,
            last_modified: None,
            body: Some(body.to_vec()),
            content_type: Some("text/plain; charset=utf-8".to_string()),
        };
        let now = search_core::time::now_unix_s();
        self.commit_document(CommitDoc {
            canonical: &canonical,
            host: &host,
            domain_id: &domain_id,
            resource_id: &resource_id,
            body,
            doc: &doc,
            kind: "record",
            parser_version: "import-v1",
            outcome: &outcome,
            now,
            lease: None,
            observe: false,
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
    ///
    /// Uses the **resource** validator store, so it is the only path that may
    /// decide "unchanged" for a resource version.
    async fn do_fetch_conditional(&self, url: &str) -> AppResult<FetchOutcome> {
        self.do_fetch_conditional_with(&self.validators, url).await
    }

    /// As [`Pipeline::do_fetch_conditional`] but against an explicit store.
    ///
    /// Discovery fetches (robots/sitemaps/feeds/**the seed page itself**) must
    /// use their own store: storing a seed's `ETag` in the resource store makes
    /// the very next `crawl_one` see `304` and report the seed as `Unchanged`,
    /// so the seed is never ingested (silently absent from the corpus).
    async fn do_fetch_conditional_with(
        &self,
        store: &MemoryValidators,
        url: &str,
    ) -> AppResult<FetchOutcome> {
        let key = canonicalize(url).unwrap_or_else(|_| url.to_string());
        if self.allow_loopback && is_loopback_url(url) {
            let prior = store.get(&key);
            let (etag, lm) = prior
                .as_ref()
                .map(|v| (v.etag.clone(), v.last_modified.clone()))
                .unwrap_or((None, None));
            let o = self
                .fetch_local_cond(url, etag.as_deref(), lm.as_deref())
                .await?;
            store_validators(store, &key, &o);
            return Ok(o);
        }
        let fetched = self.fetcher.fetch_conditional(store, url).await?;
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
    ///
    /// Uses the discovery validator store (never the resource store) so a
    /// discovery fetch cannot suppress the resource fetch of the same URL.
    async fn get_body(&self, url: &str) -> AppResult<Option<FetchOutcome>> {
        let o = self
            .do_fetch_conditional_with(&self.discovery_validators, url)
            .await?;
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

/// Origin (scheme+host+port) of a raw URL string, or empty when it cannot be parsed.
fn origin_of_url(raw: &str) -> String {
    match Url::parse(raw) {
        Ok(u) => origin_of(&u),
        Err(_) => String::new(),
    }
}

/// Log-safe form of a URL: any embedded userinfo (`user:pass@`) is stripped so
/// credentials can never reach the log. An unparseable URL collapses to a fixed
/// placeholder rather than being echoed verbatim.
fn redact(raw: &str) -> String {
    match Url::parse(raw) {
        Ok(mut u) => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.to_string()
        }
        Err(_) => "<unparseable-url>".to_string(),
    }
}

/// Seconds until the change model says `P(changed)` has reached `target` — the
/// inverse of [`change_probability`]'s `p = 1 - exp(-rate·age)`: `age = -ln(1-target)/rate`.
fn secs_until_prob(rate: f64, target: f64) -> Option<f64> {
    if rate <= 0.0 || !(0.0..1.0).contains(&target) {
        return None;
    }
    Some(-(1.0 - target).ln() / rate)
}

/// Stage-3 [`SchedulerConfig`] defaults with optional environment overrides
/// (`SEARCH_TOTAL_LIVE_FANOUT`, `SEARCH_PER_ORIGIN_FANOUT`,
/// `SEARCH_QUERY_REFRESH_LIMIT`), so the refresh tick can be tuned without a
/// rebuild. The default build stays hermetic and offline — nothing runs until a
/// refresh command is invoked.
pub fn scheduler_config_from_env() -> SchedulerConfig {
    let mut c = SchedulerConfig::default();
    if let Some(n) = env_usize("SEARCH_TOTAL_LIVE_FANOUT") {
        c.total_live_fanout = n;
    }
    if let Some(n) = env_usize("SEARCH_PER_ORIGIN_FANOUT") {
        c.per_origin_fanout = n;
    }
    if let Some(n) = env_usize("SEARCH_QUERY_REFRESH_LIMIT") {
        c.query_refresh_limit = n;
    }
    c
}

fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
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
    /// Delegate to the index-backed inner searcher: without this the trait's
    /// `Ok(None)` default makes every stored-body read (answers, §16 coverage)
    /// see no text at all.
    fn doc_text(&self, doc_id: &str) -> AppResult<Option<String>> {
        self.inner.doc_text(doc_id)
    }

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
