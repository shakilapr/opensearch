//! Single-writer subsystem (spec §4). OWNER: metadata agent.
//!
//! All mutations flow through one bounded channel into this writer, which
//! batches transactions. SQLite permits only one WAL writer at a time, so N
//! crawler tasks must never contend directly on the database.
//!
//! Two ways to drive the writer:
//!
//! * [`MetadataWriter`] — a synchronous, caller-owned connection (used by
//!   tests and simple callers). `apply_batch` commits a batch atomically.
//! * [`WriterHandle`] — a clonable, async handle backed by a dedicated
//!   `std::thread` (rusqlite `Connection` is `!Sync`). The thread drains a
//!   bounded `tokio::sync::mpsc` receiver and commits in batches, flushing when
//!   the batch reaches `batch_size` or `flush_interval` elapses since the first
//!   queued mutation. `shutdown()` flushes the tail before the thread exits.

use search_core::{AppError, AppResult};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Default max mutations per committed transaction.
pub const DEFAULT_BATCH_SIZE: usize = 256;
/// Default max time a mutation waits before its batch is flushed.
pub const DEFAULT_FLUSH_INTERVAL_MS: u64 = 50;
/// Default bounded channel capacity (backpressure: a full channel blocks senders).
pub const DEFAULT_CHANNEL_CAPACITY: usize = 1024;

/// A mutation request from a crawler/ingest task. The writer applies these in
/// batches inside a single transaction.
#[derive(Debug, Clone)]
pub enum Mutation {
    /// Upsert a domain row.
    UpsertDomain { domain_id: String, host: String },
    /// Register a resource (idempotent on resource_id).
    UpsertResource {
        resource_id: String,
        url: String,
        canonical_url: String,
        domain_id: String,
        source_id: Option<String>,
        kind: String,
    },
    /// Append a fetch observation.
    RecordFetch {
        resource_id: String,
        http_status: i64,
        etag: Option<String>,
        last_modified: Option<String>,
        content_hash: Option<String>,
        fetched_at: i64,
    },
    /// Append one journal event (source of truth). Append-only: never updated,
    /// never deleted.
    Journal {
        op_key: String,
        resource_id: String,
        stage: String,
        detail: Option<String>,
        at: i64,
    },
    /// Advanced high-water projection for an op.
    UpsertIngestOp {
        op_key: String,
        resource_id: String,
        docver_id: Option<String>,
        state: String,
        at: i64,
    },
    /// Insert an immutable content row (idempotent on `content_id`). Content is
    /// content-addressed, so a re-ingest of the same bytes is a no-op — the
    /// stored `exact_hash`/`text_path`/`byte_len` of the first write win.
    UpsertContent {
        content_id: String,
        exact_hash: String,
        text_path: Option<String>,
        byte_len: i64,
    },
    /// Insert an immutable document version (idempotent on `docver_id`). A
    /// re-ingest of the same `(resource_id, version_no)` is a no-op.
    UpsertDocumentVersion {
        docver_id: String,
        resource_id: String,
        version_no: i64,
        content_id: Option<String>,
        title: Option<String>,
        body_hash: Option<String>,
        parser_version: String,
        published_at: Option<i64>,
        modified_at: Option<i64>,
        fetched_at: i64,
    },
    /// Publish a manifest row (idempotent on `manifest_id`). A re-publish of
    /// the same generation is a no-op, so the read-side projection is stable.
    UpsertManifest {
        manifest_id: String,
        generation: i64,
        doc_count: i64,
        base_segments: i64,
        created_at: i64,
    },
    /// Update the Stage-1 freshness columns of an existing `resource` row
    /// (migration 0003). Each field is set only when `Some`, so callers can
    /// touch one freshness signal without clobbering the others.
    UpdateResourceFreshness {
        resource_id: String,
        published_at: Option<i64>,
        modified_at: Option<i64>,
        fetched_at: Option<i64>,
        verified_at: Option<i64>,
        feed_watermark: Option<i64>,
    },
    /// Upsert a sitemap row (idempotent on `sitemap_id`).
    UpsertSitemap {
        sitemap_id: String,
        url: String,
        source_url: Option<String>,
        domain_id: Option<String>,
        is_index: bool,
        status: String,
        url_count: i64,
        etag: Option<String>,
        last_modified: Option<String>,
        fetched_at: Option<i64>,
    },
    /// Upsert a feed row (idempotent on `feed_id`).
    UpsertFeed {
        feed_id: String,
        url: String,
        domain_id: Option<String>,
        title: Option<String>,
        feed_type: String,
        status: String,
        entry_count: i64,
        etag: Option<String>,
        last_modified: Option<String>,
        watermark: Option<String>,
        fetched_at: Option<i64>,
    },
    /// Upsert a source profile (idempotent on `source_id`). `topics`,
    /// `entities` and `url_patterns` are JSON-encoded TEXT arrays.
    UpsertSourceProfile {
        source_id: String,
        domain: String,
        topics: String,
        entities: String,
        url_patterns: String,
        authority: f64,
        change_rate: f64,
        js_probability: f64,
        fetch_count: i64,
        error_count: i64,
    },
    /// Upsert an entity node (idempotent on `entity_id`).
    UpsertEntity {
        entity_id: String,
        name: String,
        kind: String,
    },
    /// Insert an entity alias (idempotent on `(entity_id, alias)`). Requires the
    /// referenced entity to exist (FK).
    UpsertEntityAlias { entity_id: String, alias: String },
    /// Upsert a typed, weighted graph edge (idempotent on
    /// `(from_id, to_id, kind)`); the weight is updated in place. Requires both
    /// endpoint entities to exist (FK).
    UpsertGraphEdge {
        from_id: String,
        to_id: String,
        kind: String,
        weight: f64,
    },
    /// Upsert a scholarly work (idempotent on `work_id`).
    UpsertWork {
        work_id: String,
        id_kind: String,
        identifier: String,
        title: Option<String>,
        published_at: Option<i64>,
        modified_at: Option<i64>,
        resource_id: Option<String>,
    },
}

/// Opens (or creates) the metadata database with the required pragmas and
/// applies migrations. Sync; call from a dedicated writer task.
pub fn open(path: &Path) -> AppResult<rusqlite::Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = rusqlite::Connection::open(path).map_err(crate::db_err)?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA busy_timeout=5000;
         PRAGMA wal_autocheckpoint=1000;
         PRAGMA foreign_keys=ON;",
    )
    .map_err(crate::db_err)?;
    crate::migrations::apply(&conn)?;
    Ok(conn)
}

/// Open a read-only connection with a busy timeout. Readers never block the
/// WAL writer and must never write.
fn open_read_only(path: &Path) -> AppResult<rusqlite::Connection> {
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(crate::db_err)?;
    conn.busy_timeout(Duration::from_millis(5000))
        .map_err(crate::db_err)?;
    Ok(conn)
}

/// The single writer. Owns a connection and consumes [`Mutation`]s.
pub struct MetadataWriter {
    conn: rusqlite::Connection,
}

/// Read-only handle for the API/search path. Readers do not block the writer
/// in WAL mode, but must not write.
pub struct MetadataReader {
    conn: rusqlite::Connection,
}

impl MetadataWriter {
    pub fn new(conn: rusqlite::Connection) -> Self {
        Self { conn }
    }

    pub fn from_path(path: &Path) -> AppResult<Self> {
        Ok(Self::new(open(path)?))
    }

    /// Apply a batch of mutations inside one transaction. All-or-nothing: any
    /// failing mutation rolls the whole batch back.
    pub fn apply_batch(&mut self, batch: &[Mutation]) -> AppResult<()> {
        let tx = self.conn.transaction().map_err(crate::db_err)?;
        for m in batch {
            apply_one(&tx, m)?;
        }
        tx.commit().map_err(crate::db_err)?;
        Ok(())
    }

    pub fn conn(&self) -> &rusqlite::Connection {
        &self.conn
    }
}

impl MetadataReader {
    /// Open the metadata database read-only (SQLITE_OPEN_READ_ONLY) with a
    /// busy timeout. The database must already exist (a writer creates it).
    pub fn from_path(path: &Path) -> AppResult<Self> {
        Ok(Self {
            conn: open_read_only(path)?,
        })
    }

    pub fn conn(&self) -> &rusqlite::Connection {
        &self.conn
    }

    /// The current visible manifest: the highest published generation. Search
    /// only exposes document versions covered by a published manifest.
    pub fn visible_manifest(&self) -> AppResult<Option<crate::schema::ManifestRow>> {
        use rusqlite::OptionalExtension;
        let row = self
            .conn
            .query_row(
                "SELECT manifest_id, generation, published, doc_count, base_segments,
                        created_at, published_at
                 FROM manifest WHERE published = 1
                 ORDER BY generation DESC LIMIT 1",
                [],
                map_manifest_row,
            )
            .optional()
            .map_err(crate::db_err)?;
        Ok(row)
    }

    /// Fetch one immutable document version by `docver_id`.
    pub fn doc_by_docver(
        &self,
        docver_id: &str,
    ) -> AppResult<Option<crate::schema::DocVersionRow>> {
        use rusqlite::OptionalExtension;
        let row = self
            .conn
            .query_row(
                "SELECT docver_id, resource_id, version_no, content_id, title, body_hash,
                        parser_version, published_at, modified_at, fetched_at, created_at
                 FROM document_version WHERE docver_id = ?1",
                rusqlite::params![docver_id],
                map_docver_row,
            )
            .optional()
            .map_err(crate::db_err)?;
        Ok(row)
    }

    /// Fetch one resource by `resource_id`.
    pub fn resource_by_id(
        &self,
        resource_id: &str,
    ) -> AppResult<Option<crate::schema::ResourceRow>> {
        use rusqlite::OptionalExtension;
        let row = self
            .conn
            .query_row(
                "SELECT resource_id, url, canonical_url, domain_id, source_id, kind,
                        status, first_seen, last_fetch,
                        published_at, modified_at, fetched_at, verified_at, feed_watermark
                 FROM resource WHERE resource_id = ?1",
                rusqlite::params![resource_id],
                map_resource_row,
            )
            .optional()
            .map_err(crate::db_err)?;
        Ok(row)
    }

    /// The append-only journal rows for one `op_key`, ordered by `seq`.
    pub fn journal_entries(
        &self,
        op_key: &str,
    ) -> AppResult<Vec<crate::schema::IngestEventRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT seq, op_key, resource_id, stage, detail, at
                 FROM ingest_event WHERE op_key = ?1 ORDER BY seq ASC",
            )
            .map_err(crate::db_err)?;
        let rows = stmt
            .query_map(rusqlite::params![op_key], map_ingest_event_row)
            .map_err(crate::db_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(crate::db_err)?;
        Ok(rows)
    }

    /// Total number of journal rows (used to assert append-only growth).
    pub fn journal_len(&self) -> AppResult<i64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM ingest_event", [], |r| r.get(0))
            .map_err(crate::db_err)?;
        Ok(n)
    }

    // --- Stage-1 lookups (migration 0003) ---------------------------------

    /// Fetch one sitemap row by `sitemap_id`.
    pub fn sitemap_by_id(&self, sitemap_id: &str) -> AppResult<Option<crate::schema::SitemapRow>> {
        use rusqlite::OptionalExtension;
        let row = self
            .conn
            .query_row(
                "SELECT sitemap_id, url, source_url, domain_id, is_index, status,
                        url_count, etag, last_modified, fetched_at, created_at, updated_at
                 FROM sitemap WHERE sitemap_id = ?1",
                rusqlite::params![sitemap_id],
                map_sitemap_row,
            )
            .optional()
            .map_err(crate::db_err)?;
        Ok(row)
    }

    /// Fetch one feed row by `feed_id`.
    pub fn feed_by_id(&self, feed_id: &str) -> AppResult<Option<crate::schema::FeedRow>> {
        use rusqlite::OptionalExtension;
        let row = self
            .conn
            .query_row(
                "SELECT feed_id, url, domain_id, title, feed_type, status,
                        entry_count, etag, last_modified, watermark, fetched_at,
                        created_at, updated_at
                 FROM feed WHERE feed_id = ?1",
                rusqlite::params![feed_id],
                map_feed_row,
            )
            .optional()
            .map_err(crate::db_err)?;
        Ok(row)
    }

    /// Fetch one source profile by `source_id`.
    pub fn source_profile_by_id(
        &self,
        source_id: &str,
    ) -> AppResult<Option<crate::schema::SourceProfileRow>> {
        use rusqlite::OptionalExtension;
        let row = self
            .conn
            .query_row(
                "SELECT source_id, domain, topics, entities, url_patterns, authority,
                        change_rate, js_probability, fetch_count, error_count,
                        created_at, updated_at
                 FROM source_profile WHERE source_id = ?1",
                rusqlite::params![source_id],
                map_source_profile_row,
            )
            .optional()
            .map_err(crate::db_err)?;
        Ok(row)
    }

    /// Fetch one entity node by `entity_id`.
    pub fn entity_by_id(&self, entity_id: &str) -> AppResult<Option<crate::schema::EntityRow>> {
        use rusqlite::OptionalExtension;
        let row = self
            .conn
            .query_row(
                "SELECT entity_id, name, kind, created_at, updated_at
                 FROM entity WHERE entity_id = ?1",
                rusqlite::params![entity_id],
                map_entity_row,
            )
            .optional()
            .map_err(crate::db_err)?;
        Ok(row)
    }

    /// Aliases registered for one entity, ordered by alias.
    pub fn entity_aliases(&self, entity_id: &str) -> AppResult<Vec<crate::schema::EntityAliasRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT entity_id, alias, created_at
                 FROM entity_alias WHERE entity_id = ?1 ORDER BY alias ASC",
            )
            .map_err(crate::db_err)?;
        let rows = stmt
            .query_map(rusqlite::params![entity_id], map_entity_alias_row)
            .map_err(crate::db_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(crate::db_err)?;
        Ok(rows)
    }

    /// Fetch one graph edge by its composite key.
    pub fn graph_edge(
        &self,
        from_id: &str,
        to_id: &str,
        kind: &str,
    ) -> AppResult<Option<crate::schema::GraphEdgeRow>> {
        use rusqlite::OptionalExtension;
        let row = self
            .conn
            .query_row(
                "SELECT from_id, to_id, kind, weight, created_at, updated_at
                 FROM graph_edge WHERE from_id = ?1 AND to_id = ?2 AND kind = ?3",
                rusqlite::params![from_id, to_id, kind],
                map_graph_edge_row,
            )
            .optional()
            .map_err(crate::db_err)?;
        Ok(row)
    }

    /// Fetch one work row by `work_id`.
    pub fn work_by_id(&self, work_id: &str) -> AppResult<Option<crate::schema::WorkRow>> {
        use rusqlite::OptionalExtension;
        let row = self
            .conn
            .query_row(
                "SELECT work_id, id_kind, identifier, title, published_at, modified_at,
                        resource_id, created_at, updated_at
                 FROM work WHERE work_id = ?1",
                rusqlite::params![work_id],
                map_work_row,
            )
            .optional()
            .map_err(crate::db_err)?;
        Ok(row)
    }
}

fn map_resource_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<crate::schema::ResourceRow> {
    Ok(crate::schema::ResourceRow {
        resource_id: r.get(0)?,
        url: r.get(1)?,
        canonical_url: r.get(2)?,
        domain_id: r.get(3)?,
        source_id: r.get(4)?,
        kind: r.get(5)?,
        status: r.get(6)?,
        first_seen: r.get(7)?,
        last_fetch: r.get(8)?,
        published_at: r.get(9)?,
        modified_at: r.get(10)?,
        fetched_at: r.get(11)?,
        verified_at: r.get(12)?,
        feed_watermark: r.get(13)?,
    })
}

fn map_sitemap_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<crate::schema::SitemapRow> {
    Ok(crate::schema::SitemapRow {
        sitemap_id: r.get(0)?,
        url: r.get(1)?,
        source_url: r.get(2)?,
        domain_id: r.get(3)?,
        is_index: r.get(4)?,
        status: r.get(5)?,
        url_count: r.get(6)?,
        etag: r.get(7)?,
        last_modified: r.get(8)?,
        fetched_at: r.get(9)?,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
    })
}

fn map_feed_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<crate::schema::FeedRow> {
    Ok(crate::schema::FeedRow {
        feed_id: r.get(0)?,
        url: r.get(1)?,
        domain_id: r.get(2)?,
        title: r.get(3)?,
        feed_type: r.get(4)?,
        status: r.get(5)?,
        entry_count: r.get(6)?,
        etag: r.get(7)?,
        last_modified: r.get(8)?,
        watermark: r.get(9)?,
        fetched_at: r.get(10)?,
        created_at: r.get(11)?,
        updated_at: r.get(12)?,
    })
}

fn map_source_profile_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<crate::schema::SourceProfileRow> {
    Ok(crate::schema::SourceProfileRow {
        source_id: r.get(0)?,
        domain: r.get(1)?,
        topics: r.get(2)?,
        entities: r.get(3)?,
        url_patterns: r.get(4)?,
        authority: r.get(5)?,
        change_rate: r.get(6)?,
        js_probability: r.get(7)?,
        fetch_count: r.get(8)?,
        error_count: r.get(9)?,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
    })
}

fn map_entity_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<crate::schema::EntityRow> {
    Ok(crate::schema::EntityRow {
        entity_id: r.get(0)?,
        name: r.get(1)?,
        kind: r.get(2)?,
        created_at: r.get(3)?,
        updated_at: r.get(4)?,
    })
}

fn map_entity_alias_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<crate::schema::EntityAliasRow> {
    Ok(crate::schema::EntityAliasRow {
        entity_id: r.get(0)?,
        alias: r.get(1)?,
        created_at: r.get(2)?,
    })
}

fn map_graph_edge_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<crate::schema::GraphEdgeRow> {
    Ok(crate::schema::GraphEdgeRow {
        from_id: r.get(0)?,
        to_id: r.get(1)?,
        kind: r.get(2)?,
        weight: r.get(3)?,
        created_at: r.get(4)?,
        updated_at: r.get(5)?,
    })
}

fn map_work_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<crate::schema::WorkRow> {
    Ok(crate::schema::WorkRow {
        work_id: r.get(0)?,
        id_kind: r.get(1)?,
        identifier: r.get(2)?,
        title: r.get(3)?,
        published_at: r.get(4)?,
        modified_at: r.get(5)?,
        resource_id: r.get(6)?,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
    })
}

fn map_docver_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<crate::schema::DocVersionRow> {
    Ok(crate::schema::DocVersionRow {
        docver_id: r.get(0)?,
        resource_id: r.get(1)?,
        version_no: r.get(2)?,
        content_id: r.get(3)?,
        title: r.get(4)?,
        body_hash: r.get(5)?,
        parser_version: r.get(6)?,
        published_at: r.get(7)?,
        modified_at: r.get(8)?,
        fetched_at: r.get(9)?,
        created_at: r.get(10)?,
    })
}

fn map_manifest_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<crate::schema::ManifestRow> {
    Ok(crate::schema::ManifestRow {
        manifest_id: r.get(0)?,
        generation: r.get(1)?,
        published: r.get(2)?,
        doc_count: r.get(3)?,
        base_segments: r.get(4)?,
        created_at: r.get(5)?,
        published_at: r.get(6)?,
    })
}

fn map_ingest_event_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<crate::schema::IngestEventRow> {
    Ok(crate::schema::IngestEventRow {
        seq: r.get(0)?,
        op_key: r.get(1)?,
        resource_id: r.get(2)?,
        stage: r.get(3)?,
        detail: r.get(4)?,
        at: r.get(5)?,
    })
}

// ---------------------------------------------------------------------------
// Background batched writer
// ---------------------------------------------------------------------------

/// Messages to the background writer thread.
enum Msg {
    Mutation(Mutation),
    /// Flush everything queued so far, then reply with the commit result.
    Flush(tokio::sync::oneshot::Sender<AppResult<()>>),
    /// Flush the tail (including anything still queued), reply, and exit.
    Shutdown(tokio::sync::oneshot::Sender<AppResult<()>>),
}

struct HandleInner {
    tx: tokio::sync::mpsc::Sender<Msg>,
    join: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// A clonable async handle to the background metadata writer.
///
/// `send` exerts backpressure: it awaits when the bounded channel is full.
#[derive(Clone)]
pub struct WriterHandle {
    inner: Arc<HandleInner>,
}

impl WriterHandle {
    /// Queue a mutation, awaiting capacity on the bounded channel.
    pub async fn send(&self, m: Mutation) -> AppResult<()> {
        self.inner
            .tx
            .send(Msg::Mutation(m))
            .await
            .map_err(|_| AppError::Internal("metadata writer stopped".into()))
    }

    /// Non-blocking queue; fails immediately if the channel is full or the
    /// writer has stopped.
    pub fn try_send(&self, m: Mutation) -> AppResult<()> {
        self.inner
            .tx
            .try_send(Msg::Mutation(m))
            .map_err(|e| AppError::Internal(format!("metadata writer unavailable: {e}")))
    }

    /// Wait until every mutation sent before this call has been committed.
    pub async fn flush(&self) -> AppResult<()> {
        let (otx, orx) = tokio::sync::oneshot::channel();
        self.inner
            .tx
            .send(Msg::Flush(otx))
            .await
            .map_err(|_| AppError::Internal("metadata writer stopped".into()))?;
        orx.await
            .map_err(|_| AppError::Internal("metadata writer dropped flush reply".into()))?
    }

    /// Flush the tail and stop the writer thread. Idempotent across clones.
    pub async fn shutdown(&self) -> AppResult<()> {
        let (otx, orx) = tokio::sync::oneshot::channel();
        let res = match self.inner.tx.send(Msg::Shutdown(otx)).await {
            Ok(()) => orx
                .await
                .map_err(|_| AppError::Internal("metadata writer dropped shutdown reply".into()))?,
            // Already stopped.
            Err(_) => Ok(()),
        };
        if let Some(join) = self.inner.join.lock().unwrap().take() {
            let _ = join.join();
        }
        res
    }
}

/// Spawn a background writer thread with default batching parameters.
pub fn spawn_writer(path: impl AsRef<Path>) -> AppResult<WriterHandle> {
    spawn_writer_with(
        path,
        DEFAULT_BATCH_SIZE,
        Duration::from_millis(DEFAULT_FLUSH_INTERVAL_MS),
        DEFAULT_CHANNEL_CAPACITY,
    )
}

/// Spawn a background writer thread. `batch_size` = max mutations per commit,
/// `flush_interval` = max latency for a queued mutation, `capacity` = bounded
/// channel size (backpressure).
pub fn spawn_writer_with(
    path: impl AsRef<Path>,
    batch_size: usize,
    flush_interval: Duration,
    capacity: usize,
) -> AppResult<WriterHandle> {
    let conn = open(path.as_ref())?;
    let batch_size = batch_size.max(1);
    let capacity = capacity.max(1);
    let (tx, rx) = tokio::sync::mpsc::channel::<Msg>(capacity);

    let join = std::thread::Builder::new()
        .name("metadata-writer".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("build metadata writer runtime");
            rt.block_on(writer_loop(
                MetadataWriter::new(conn),
                rx,
                batch_size,
                flush_interval,
            ));
        })
        .map_err(|e| AppError::Internal(format!("spawn metadata writer thread: {e}")))?;

    Ok(WriterHandle {
        inner: Arc::new(HandleInner {
            tx,
            join: Mutex::new(Some(join)),
        }),
    })
}

async fn writer_loop(
    mut writer: MetadataWriter,
    mut rx: tokio::sync::mpsc::Receiver<Msg>,
    batch_size: usize,
    flush_interval: Duration,
) {
    let mut batch: Vec<Mutation> = Vec::with_capacity(batch_size);
    let mut flush_at: Option<tokio::time::Instant> = None;

    loop {
        let timer = async {
            match flush_at {
                Some(t) => tokio::time::sleep_until(t).await,
                // No deadline until the first item of a batch arrives.
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            biased;
            msg = rx.recv() => {
                match msg {
                    None => {
                        // All senders dropped: commit the tail and exit.
                        let _ = do_flush(&mut writer, &mut batch);
                        break;
                    }
                    Some(Msg::Mutation(m)) => {
                        if batch.is_empty() {
                            flush_at = Some(tokio::time::Instant::now() + flush_interval);
                        }
                        batch.push(m);
                        if batch.len() >= batch_size {
                            let _ = do_flush(&mut writer, &mut batch);
                            flush_at = None;
                        }
                    }
                    Some(Msg::Flush(reply)) => {
                        let r = do_flush(&mut writer, &mut batch);
                        flush_at = None;
                        let _ = reply.send(r);
                    }
                    Some(Msg::Shutdown(reply)) => {
                        // Drain anything still queued, then commit the tail.
                        while let Ok(msg) = rx.try_recv() {
                            if let Msg::Mutation(m) = msg {
                                batch.push(m);
                            }
                        }
                        let r = do_flush(&mut writer, &mut batch);
                        let _ = reply.send(r);
                        break;
                    }
                }
            }
            _ = timer => {
                let _ = do_flush(&mut writer, &mut batch);
                flush_at = None;
            }
        }
    }
}

/// Commit `batch` atomically and clear it. Empty batches are a no-op. Failures
/// are logged and reported (the writer keeps running).
fn do_flush(writer: &mut MetadataWriter, batch: &mut Vec<Mutation>) -> AppResult<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let res = writer.apply_batch(batch);
    if let Err(e) = &res {
        tracing::error!(error = %e, n = batch.len(), "metadata writer batch commit failed");
    }
    batch.clear();
    res
}

// ---------------------------------------------------------------------------
// Mutation application
// ---------------------------------------------------------------------------

fn apply_one(tx: &rusqlite::Transaction<'_>, m: &Mutation) -> AppResult<()> {
    let now = search_core::time::now_unix_s();
    match m {
        Mutation::UpsertDomain { domain_id, host } => {
            tx.execute(
                "INSERT INTO domain(domain_id, host, created_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(domain_id) DO NOTHING",
                rusqlite::params![domain_id, host, now],
            )
            .map_err(crate::db_err)?;
        }
        Mutation::UpsertResource {
            resource_id,
            url,
            canonical_url,
            domain_id,
            source_id,
            kind,
        } => {
            tx.execute(
                "INSERT INTO resource(resource_id, url, canonical_url, domain_id, source_id, kind, status, first_seen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'NEW', ?7)
                 ON CONFLICT(resource_id) DO UPDATE SET
                    url=excluded.url, canonical_url=excluded.canonical_url,
                    source_id=COALESCE(excluded.source_id, resource.source_id)",
                rusqlite::params![resource_id, url, canonical_url, domain_id, source_id, kind, now],
            )
            .map_err(crate::db_err)?;
        }
        Mutation::RecordFetch {
            resource_id,
            http_status,
            etag,
            last_modified,
            content_hash,
            fetched_at,
        } => {
            tx.execute(
                "INSERT INTO fetch_observation(resource_id, http_status, etag, last_modified, content_hash, fetched_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![resource_id, http_status, etag, last_modified, content_hash, fetched_at],
            )
            .map_err(crate::db_err)?;
            tx.execute(
                "UPDATE resource SET last_fetch=?2 WHERE resource_id=?1",
                rusqlite::params![resource_id, fetched_at],
            )
            .map_err(crate::db_err)?;
        }
        Mutation::Journal {
            op_key,
            resource_id,
            stage,
            detail,
            at,
        } => {
            tx.execute(
                "INSERT INTO ingest_event(op_key, resource_id, stage, detail, at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![op_key, resource_id, stage, detail, at],
            )
            .map_err(crate::db_err)?;
        }
        Mutation::UpsertIngestOp {
            op_key,
            resource_id,
            docver_id,
            state,
            at,
        } => {
            tx.execute(
                "INSERT INTO ingest_op(op_key, resource_id, docver_id, state, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                 ON CONFLICT(op_key) DO UPDATE SET state=excluded.state,
                    docver_id=COALESCE(excluded.docver_id, ingest_op.docver_id), updated_at=excluded.updated_at",
                rusqlite::params![op_key, resource_id, docver_id, state, at],
            )
            .map_err(crate::db_err)?;
        }
        Mutation::UpsertContent {
            content_id,
            exact_hash,
            text_path,
            byte_len,
        } => {
            // Content is content-addressed and immutable: a duplicate
            // content_id is a no-op (the first write wins).
            tx.execute(
                "INSERT INTO content(content_id, exact_hash, text_path, byte_len, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(content_id) DO NOTHING",
                rusqlite::params![content_id, exact_hash, text_path, byte_len, now],
            )
            .map_err(crate::db_err)?;
        }
        Mutation::UpsertDocumentVersion {
            docver_id,
            resource_id,
            version_no,
            content_id,
            title,
            body_hash,
            parser_version,
            published_at,
            modified_at,
            fetched_at,
        } => {
            // Immutable versions: a duplicate (resource_id, version_no) or
            // docver_id is a no-op, never an update.
            tx.execute(
                "INSERT INTO document_version(docver_id, resource_id, version_no, content_id,
                        title, body_hash, parser_version, published_at, modified_at,
                        fetched_at, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(docver_id) DO NOTHING",
                rusqlite::params![
                    docver_id,
                    resource_id,
                    version_no,
                    content_id,
                    title,
                    body_hash,
                    parser_version,
                    published_at,
                    modified_at,
                    fetched_at,
                    now
                ],
            )
            .map_err(crate::db_err)?;
        }
        Mutation::UpsertManifest {
            manifest_id,
            generation,
            doc_count,
            base_segments,
            created_at,
        } => {
            // Published manifest projection: a duplicate manifest_id is a
            // no-op, so re-publishing a generation is idempotent.
            tx.execute(
                "INSERT INTO manifest(manifest_id, generation, published, doc_count,
                        base_segments, created_at, published_at)
                 VALUES (?1, ?2, 1, ?3, ?4, ?5, ?5)
                 ON CONFLICT(manifest_id) DO NOTHING",
                rusqlite::params![manifest_id, generation, doc_count, base_segments, created_at],
            )
            .map_err(crate::db_err)?;
        }
        Mutation::UpdateResourceFreshness {
            resource_id,
            published_at,
            modified_at,
            fetched_at,
            verified_at,
            feed_watermark,
        } => {
            // Set only the provided signals (COALESCE keeps the rest), so a
            // caller can refresh one field without clearing the others.
            tx.execute(
                "UPDATE resource SET
                    published_at   = COALESCE(?2, published_at),
                    modified_at    = COALESCE(?3, modified_at),
                    fetched_at     = COALESCE(?4, fetched_at),
                    verified_at    = COALESCE(?5, verified_at),
                    feed_watermark = COALESCE(?6, feed_watermark)
                 WHERE resource_id = ?1",
                rusqlite::params![
                    resource_id,
                    published_at,
                    modified_at,
                    fetched_at,
                    verified_at,
                    feed_watermark
                ],
            )
            .map_err(crate::db_err)?;
        }
        Mutation::UpsertSitemap {
            sitemap_id,
            url,
            source_url,
            domain_id,
            is_index,
            status,
            url_count,
            etag,
            last_modified,
            fetched_at,
        } => {
            tx.execute(
                "INSERT INTO sitemap(sitemap_id, url, source_url, domain_id, is_index,
                        status, url_count, etag, last_modified, fetched_at,
                        created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)
                 ON CONFLICT(sitemap_id) DO UPDATE SET
                    url=excluded.url, source_url=excluded.source_url,
                    domain_id=excluded.domain_id, is_index=excluded.is_index,
                    status=excluded.status, url_count=excluded.url_count,
                    etag=excluded.etag, last_modified=excluded.last_modified,
                    fetched_at=excluded.fetched_at, updated_at=excluded.updated_at",
                rusqlite::params![
                    sitemap_id,
                    url,
                    source_url,
                    domain_id,
                    is_index,
                    status,
                    url_count,
                    etag,
                    last_modified,
                    fetched_at,
                    now
                ],
            )
            .map_err(crate::db_err)?;
        }
        Mutation::UpsertFeed {
            feed_id,
            url,
            domain_id,
            title,
            feed_type,
            status,
            entry_count,
            etag,
            last_modified,
            watermark,
            fetched_at,
        } => {
            tx.execute(
                "INSERT INTO feed(feed_id, url, domain_id, title, feed_type, status,
                        entry_count, etag, last_modified, watermark, fetched_at,
                        created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)
                 ON CONFLICT(feed_id) DO UPDATE SET
                    url=excluded.url, domain_id=excluded.domain_id,
                    title=excluded.title, feed_type=excluded.feed_type,
                    status=excluded.status, entry_count=excluded.entry_count,
                    etag=excluded.etag, last_modified=excluded.last_modified,
                    watermark=excluded.watermark, fetched_at=excluded.fetched_at,
                    updated_at=excluded.updated_at",
                rusqlite::params![
                    feed_id,
                    url,
                    domain_id,
                    title,
                    feed_type,
                    status,
                    entry_count,
                    etag,
                    last_modified,
                    watermark,
                    fetched_at,
                    now
                ],
            )
            .map_err(crate::db_err)?;
        }
        Mutation::UpsertSourceProfile {
            source_id,
            domain,
            topics,
            entities,
            url_patterns,
            authority,
            change_rate,
            js_probability,
            fetch_count,
            error_count,
        } => {
            tx.execute(
                "INSERT INTO source_profile(source_id, domain, topics, entities,
                        url_patterns, authority, change_rate, js_probability,
                        fetch_count, error_count, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)
                 ON CONFLICT(source_id) DO UPDATE SET
                    domain=excluded.domain, topics=excluded.topics,
                    entities=excluded.entities, url_patterns=excluded.url_patterns,
                    authority=excluded.authority, change_rate=excluded.change_rate,
                    js_probability=excluded.js_probability,
                    fetch_count=excluded.fetch_count, error_count=excluded.error_count,
                    updated_at=excluded.updated_at",
                rusqlite::params![
                    source_id,
                    domain,
                    topics,
                    entities,
                    url_patterns,
                    authority,
                    change_rate,
                    js_probability,
                    fetch_count,
                    error_count,
                    now
                ],
            )
            .map_err(crate::db_err)?;
        }
        Mutation::UpsertEntity {
            entity_id,
            name,
            kind,
        } => {
            tx.execute(
                "INSERT INTO entity(entity_id, name, kind, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)
                 ON CONFLICT(entity_id) DO UPDATE SET
                    name=excluded.name, kind=excluded.kind, updated_at=excluded.updated_at",
                rusqlite::params![entity_id, name, kind, now],
            )
            .map_err(crate::db_err)?;
        }
        Mutation::UpsertEntityAlias { entity_id, alias } => {
            // Alias rows are immutable identity facts: a duplicate is a no-op.
            tx.execute(
                "INSERT INTO entity_alias(entity_id, alias, created_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(entity_id, alias) DO NOTHING",
                rusqlite::params![entity_id, alias, now],
            )
            .map_err(crate::db_err)?;
        }
        Mutation::UpsertGraphEdge {
            from_id,
            to_id,
            kind,
            weight,
        } => {
            tx.execute(
                "INSERT INTO graph_edge(from_id, to_id, kind, weight, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                 ON CONFLICT(from_id, to_id, kind) DO UPDATE SET
                    weight=excluded.weight, updated_at=excluded.updated_at",
                rusqlite::params![from_id, to_id, kind, weight, now],
            )
            .map_err(crate::db_err)?;
        }
        Mutation::UpsertWork {
            work_id,
            id_kind,
            identifier,
            title,
            published_at,
            modified_at,
            resource_id,
        } => {
            tx.execute(
                "INSERT INTO work(work_id, id_kind, identifier, title, published_at,
                        modified_at, resource_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
                 ON CONFLICT(work_id) DO UPDATE SET
                    id_kind=excluded.id_kind, identifier=excluded.identifier,
                    title=excluded.title, published_at=excluded.published_at,
                    modified_at=excluded.modified_at,
                    resource_id=COALESCE(excluded.resource_id, work.resource_id),
                    updated_at=excluded.updated_at",
                rusqlite::params![
                    work_id,
                    id_kind,
                    identifier,
                    title,
                    published_at,
                    modified_at,
                    resource_id,
                    now
                ],
            )
            .map_err(crate::db_err)?;
        }
    }
    Ok(())
}
