//! Single-writer subsystem (spec §4). OWNER: metadata agent.
//!
//! All mutations flow through one bounded channel into this writer, which
//! batches transactions. SQLite permits only one WAL writer at a time.

use search_core::{AppError, AppResult};
use std::path::Path;

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
    /// Append one journal event (source of truth).
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

    /// Apply a batch of mutations inside one transaction.
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
    pub fn from_path(path: &Path) -> AppResult<Self> {
        Ok(Self {
            conn: open(path)?,
        })
    }
    pub fn conn(&self) -> &rusqlite::Connection {
        &self.conn
    }
}

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
    }
    Ok(())
}
