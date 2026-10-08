//! The Stage-0 pipeline: one reusable [`Pipeline`] that owns the frontier,
//! fetcher, metadata writer, indexer and API-facing searcher, and drives a
//! single URL through the frozen ingest lifecycle.
//!
//! ## Lifecycle
//!
//! For `crawl_one(url)`:
//!
//! 1. canonicalize the URL (stable `ResourceId`),
//! 2. SSRF-validate the target and its `robots.txt`,
//! 3. push + lease the resource in the frontier,
//! 4. fetch `robots.txt` and evaluate it **before** fetching the resource,
//! 5. fetch the resource (redirects/limits/loopback-exemption handled by the
//!    fetcher),
//! 6. normalize (`ContentId` from the exact bytes),
//! 7. write the raw blob and the normalized text under `data_dir`,
//! 8. commit metadata (domain/resource/fetch observation/content/DocumentVersion),
//! 9. index into the Tantivy overlay, publish the manifest + watermark,
//! 10. record every journal stage and mark the resource done.
//!
//! Stages are journalled through [`IngestCoordinator`], which is idempotent per
//! `(op_key, stage)`: re-running the full walk writes nothing new.
//!
//! ## Idempotency (the Stage-0 no-duplicate requirement)
//!
//! `crawl_one` derives `ContentId` from the fetched bytes and compares it to the
//! latest `DocumentVersion` for the resource. If unchanged, **no** second
//! `DocumentVersion` is created: the op is already on record, so the pipeline
//! skips to `VISIBLE` and returns [`IngestOutcome::Unchanged`].

use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use rusqlite::{params, OptionalExtension};
use url::Url;

use search_api::query::Searcher;
use search_api::IndexerSearcher;
use search_core::config::Config;
use search_core::ids::{ContentId, DocVerId, DomainId, OpKey, ResourceId};
use search_core::{AppError, AppResult, SearchRequest, SearchResponse};
use search_fetcher::canon::canonicalize;
use search_fetcher::http::{FetchOutcome, Fetcher};
use search_fetcher::robots::{self, RobotsPolicy, RobotsVerdict};
use search_fetcher::ssrf;
use search_frontier::queue::{FrontierEntry, Priority};
use search_frontier::Frontier;
use search_indexer::{IndexDoc, Indexer, IndexerConfig};
use search_ingest::{IngestCoordinator, IngestStage, Watermark};
use search_metadata::{MetadataWriter, Mutation};
use search_parser::NormalizedDoc;

/// Bumped when the Stage-0 HTML normalization contract changes.
const PARSER_VERSION: &str = "stage0-html-v1";

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
}

/// The runnable Stage-0 pipeline.
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
            allow_loopback,
        })
    }

    /// The searcher the HTTP API should be wired to (real Tantivy indexer).
    pub fn searcher(&self) -> Arc<dyn Searcher + Send + Sync> {
        Arc::new(IndexerSearcher::new(self.indexer.clone()))
    }

    /// Deterministic resource id for a URL (canonicalized first), matching the
    /// id `crawl_one` uses.
    pub fn resource_id_for(&self, url: &str) -> AppResult<String> {
        let canonical = canonicalize(url)
            .map_err(|e| AppError::Invalid(format!("invalid url {url:?}: {e}")))?;
        Ok(ResourceId::from_canonical_url(&canonical).into_string())
    }

    /// Run a `/search` query against the live index.
    pub async fn search(&self, q: &str, limit: usize) -> AppResult<SearchResponse> {
        let searcher = IndexerSearcher::new(self.indexer.clone());
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

    /// Full single-URL ingest: lease -> robots -> fetch -> normalize -> ContentId
    /// -> DocumentVersion -> journal stages -> Tantivy index -> publish manifest.
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

        // (5) fetch.
        let outcome = match self.do_fetch(&canonical).await {
            Ok(o) => o,
            Err(e) => {
                if let Some(l) = &lease {
                    let _ = self.frontier.fail(l, now, e.is_retryable());
                }
                return Err(e);
            }
        };

        if outcome.status == 304 {
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
            if let Some(l) = &lease {
                let _ = self.frontier.fail(l, now, false);
            }
            return Err(classify_status(outcome.status, &canonical));
        }

        // (6) ContentId from the exact bytes + idempotency check.
        let body = outcome.body.clone().unwrap_or_default();
        let content_id = ContentId::from_bytes(&body);
        let existing = self.latest_version(&resource_id)?;
        if let Some((vno, Some(exist_cid))) = &existing {
            if exist_cid == content_id.as_str() {
                // Unchanged: no new DocumentVersion. Skip idempotently to VISIBLE.
                let op_key = OpKey::from_parts(&resource_id, 0, *vno);
                self.record_stage(&op_key, resource_id.as_str(), IngestStage::Visible)?;
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

        // FETCHED + NORMALIZED
        self.record_stage(&op_key, resource_id.as_str(), IngestStage::Fetched)?;
        let doc = search_parser::normalize(&canonical, &body, outcome.content_type.as_deref())?;
        self.record_stage(&op_key, resource_id.as_str(), IngestStage::Normalized)?;

        // BLOB_WRITTEN
        self.write_content_files(&content_id, &body, doc.text.as_bytes())?;
        self.record_stage(&op_key, resource_id.as_str(), IngestStage::BlobWritten)?;

        // METADATA_COMMITTED
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
            now,
        )?;
        self.record_stage(&op_key, resource_id.as_str(), IngestStage::MetadataCommitted)?;

        // LEXICAL_INDEXED (Tantivy overlay)
        let generation = self.indexer.visible_generation() + 1;
        let index_doc = build_index_doc(
            &resource_id,
            &content_id,
            version_no,
            &canonical,
            &doc,
            generation,
            now,
        );
        self.indexer.index_overlay(index_doc)?;
        self.record_stage(&op_key, resource_id.as_str(), IngestStage::LexicalIndexed)?;

        // GRAPH_INDEXED — recorded no-op: Stage 0 has no graph projection.
        self.record_stage(&op_key, resource_id.as_str(), IngestStage::GraphIndexed)?;

        // Publish manifest + watermark, then VISIBLE.
        let manifest = self.indexer.publish_manifest()?;
        {
            let w = self.meta.lock().unwrap();
            Watermark::publish(&w, manifest.generation)?;
            let _ = w.conn().execute(
                "INSERT INTO manifest(manifest_id, generation, published, doc_count, base_segments, created_at, published_at)
                 VALUES (?1, ?2, 1, ?3, ?4, ?5, ?5) ON CONFLICT(manifest_id) DO NOTHING",
                params![
                    manifest.manifest_id,
                    manifest.generation as i64,
                    manifest.doc_count as i64,
                    manifest.base_segments as i64,
                    now
                ],
            );
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

    /// Fetch `url`, honouring the loopback exemption.
    async fn do_fetch(&self, url: &str) -> AppResult<FetchOutcome> {
        if self.allow_loopback && is_loopback_url(url) {
            return self.fetch_local(url).await;
        }
        self.fetcher.fetch(url, None, None).await
    }

    /// Direct fetch used only for loopback (test) targets: same body-cap and
    /// status handling as the fetcher, minus the SSRF guard and redirect walk.
    async fn fetch_local(&self, url: &str) -> AppResult<FetchOutcome> {
        let resp = self
            .http
            .get(url)
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
                source_id: None,
                kind: "html".to_string(),
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

        // GAP: `Mutation` has no `UpsertContent` variant, but `document_version`
        // has an FK to `content(content_id)`. The driver inserts the content row
        // directly on the single writer connection.
        w.conn()
            .execute(
                "INSERT INTO content(content_id, exact_hash, text_path, byte_len, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(content_id) DO NOTHING",
                params![
                    content_id.as_str(),
                    hex,
                    text_path,
                    doc.text_len as i64,
                    now
                ],
            )
            .map_err(search_metadata::db_err)?;

        w.apply_batch(&[Mutation::UpsertDocumentVersion {
            docver_id: docver_id.as_str().to_string(),
            resource_id: resource_id.as_str().to_string(),
            version_no: version_no as i64,
            content_id: Some(content_id.as_str().to_string()),
            title: doc.title.clone(),
            body_hash: None,
            parser_version: PARSER_VERSION.to_string(),
            published_at: doc.published_at,
            modified_at: doc.modified_at,
            fetched_at: now,
        }])?;
        Ok(())
    }
}

/// Build the Tantivy overlay document for a normalized page.
fn build_index_doc(
    resource_id: &ResourceId,
    content_id: &ContentId,
    version_no: u64,
    canonical: &str,
    doc: &NormalizedDoc,
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
        source_id: None,
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
