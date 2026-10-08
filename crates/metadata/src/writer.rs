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
                        status, first_seen, last_fetch
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
    }
    Ok(())
}
