//! Stage-0 acceptance tests for the SQLite single-writer subsystem (spec §4).

use search_metadata::{spawn_writer_with, MetadataReader, MetadataWriter, Mutation};
use std::time::Duration;
use tempfile::tempdir;

fn domain(id: &str) -> Mutation {
    Mutation::UpsertDomain {
        domain_id: id.into(),
        host: format!("{id}.example"),
    }
}

fn resource(rid: &str, dom: &str) -> Mutation {
    Mutation::UpsertResource {
        resource_id: rid.into(),
        url: format!("https://{dom}/{}", rid),
        canonical_url: format!("https://{dom}/{}", rid),
        domain_id: dom.into(),
        source_id: None,
        kind: "html".into(),
    }
}

fn journal(op_key: &str, rid: &str, stage: &str, at: i64) -> Mutation {
    Mutation::Journal {
        op_key: op_key.into(),
        resource_id: rid.into(),
        stage: stage.into(),
        detail: None,
        at,
    }
}

/// (a) Migrations are idempotent when applied twice.
#[test]
fn migrations_are_idempotent() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("meta.db");

    // `open` runs the migration; running it again must not error.
    let conn = search_metadata::writer::open(&path).unwrap();
    search_metadata::migrations::apply(&conn).unwrap();
    search_metadata::migrations::apply(&conn).unwrap();

    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='document_version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "schema present after repeated migration");
}

/// (a2) Applying migrations twice is idempotent AND migration 0002 has created
/// the `published_watermark` table. Every table the Stage-0 schema needs is
/// present in `sqlite_master` after migrations alone — no lazy `CREATE TABLE`.
#[test]
fn migrations_create_watermark_and_all_expected_tables() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("meta.db");

    // `open` runs migrations; run the runner twice more to prove idempotency.
    let conn = search_metadata::writer::open(&path).unwrap();
    search_metadata::migrations::apply(&conn).unwrap();
    search_metadata::migrations::apply(&conn).unwrap();

    // Migration ledger records exactly the known versions, once each.
    let versions: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT version FROM schema_migration ORDER BY version")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert_eq!(versions, vec![1, 2], "each migration applied exactly once");

    let tables: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master
                  WHERE type='table' AND name NOT LIKE 'sqlite_%'
                  ORDER BY name",
            )
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };

    let mut expected = vec![
        "content",
        "crawl_policy",
        "document_version",
        "domain",
        "fetch_observation",
        "frontier_entry",
        "ingest_event",
        "ingest_op",
        "manifest",
        "origin_state",
        "published_watermark",
        "resource",
        "robots_cache",
        "schema_migration",
        "source",
    ];
    expected.sort_unstable();
    assert_eq!(
        tables,
        expected.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        "sqlite_master table set after migrations"
    );

    // The watermark table is really the 0002 single-row shape: the CHECK(id=1)
    // constraint must reject a second row.
    conn.execute(
        "INSERT INTO published_watermark(id, generation, published_at) VALUES (1, 0, 0)",
        [],
    )
    .unwrap();
    let dup = conn.execute(
        "INSERT INTO published_watermark(id, generation, published_at) VALUES (2, 0, 0)",
        [],
    );
    assert!(dup.is_err(), "published_watermark must hold exactly one row");
}

/// (b) apply_batch of 3 mutations commits atomically and rows are readable.
#[test]
fn apply_batch_commits_atomically_and_is_readable() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("meta.db");

    let mut w = MetadataWriter::from_path(&path).unwrap();
    w.apply_batch(&[
        domain("d1"),
        resource("r1", "d1"),
        Mutation::RecordFetch {
            resource_id: "r1".into(),
            http_status: 200,
            etag: Some("W/\"abc\"".into()),
            last_modified: None,
            content_hash: Some("hash1".into()),
            fetched_at: 1000,
        },
    ])
    .unwrap();

    let r = MetadataReader::from_path(&path).unwrap();
    let res = r.resource_by_id("r1").unwrap().expect("resource readable");
    assert_eq!(res.domain_id, "d1");
    assert_eq!(res.status, "NEW");
    assert_eq!(res.last_fetch, Some(1000));

    let obs: i64 = r
        .conn()
        .query_row("SELECT COUNT(*) FROM fetch_observation", [], |x| x.get(0))
        .unwrap();
    assert_eq!(obs, 1);
}

/// (b2) A failing mutation rolls back the whole batch (atomicity).
#[test]
fn apply_batch_rolls_back_on_failure() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("meta.db");
    let mut w = MetadataWriter::from_path(&path).unwrap();

    let err = w.apply_batch(&[
        domain("d1"),
        resource("r1", "d1"),
        // r2 references an unknown domain -> FK violation -> whole batch fails.
        resource("r2", "missing"),
    ]);
    assert!(err.is_err(), "batch with a bad row must fail");

    let r = MetadataReader::from_path(&path).unwrap();
    assert!(
        r.resource_by_id("r1").unwrap().is_none(),
        "earlier rows in a failed batch must not persist"
    );
}

/// (c) foreign_keys=ON is enforced: a resource with an unknown domain_id fails.
#[test]
fn foreign_keys_are_enforced() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("meta.db");
    let mut w = MetadataWriter::from_path(&path).unwrap();

    let fk: i64 = w
        .conn()
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fk, 1, "foreign_keys pragma must be ON");

    let err = w.apply_batch(&[resource("orphan", "no_such_domain")]);
    assert!(err.is_err(), "unknown domain_id must violate the FK constraint");
}

/// (d) Journal append is append-only and ordered by seq.
#[test]
fn journal_is_append_only_and_ordered() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("meta.db");
    let mut w = MetadataWriter::from_path(&path).unwrap();

    w.apply_batch(&[domain("d1"), resource("r1", "d1")]).unwrap();

    let stages = ["FETCHED", "NORMALIZED", "BLOB_WRITTEN", "METADATA_COMMITTED"];
    let batch: Vec<Mutation> = stages
        .iter()
        .enumerate()
        .map(|(i, s)| journal("op1", "r1", s, 100 + i as i64))
        .collect();
    w.apply_batch(&batch).unwrap();

    // Append more later; existing rows must be untouched.
    w.apply_batch(&[journal("op1", "r1", "LEXICAL_INDEXED", 200)])
        .unwrap();

    let r = MetadataReader::from_path(&path).unwrap();
    let rows = r.journal_entries("op1").unwrap();
    assert_eq!(rows.len(), 5);
    assert_eq!(r.journal_len().unwrap(), 5);
    // seq strictly increasing (ordered by seq asc).
    for pair in rows.windows(2) {
        assert!(
            pair[1].seq > pair[0].seq,
            "journal seq must be strictly increasing"
        );
    }
    let stages_back: Vec<&str> = rows.iter().map(|e| e.stage.as_str()).collect();
    assert_eq!(
        stages_back,
        vec![
            "FETCHED",
            "NORMALIZED",
            "BLOB_WRITTEN",
            "METADATA_COMMITTED",
            "LEXICAL_INDEXED"
        ]
    );
}

/// (e) A reader can query committed data while a writer holds an open
/// transaction (WAL concurrency): uncommitted rows are invisible, committed
/// rows are readable without blocking.
#[test]
fn reader_queries_while_writer_holds_transaction() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("meta.db");

    let mut w = MetadataWriter::from_path(&path).unwrap();
    w.apply_batch(&[domain("d1"), resource("r1", "d1")])
        .unwrap();

    // A second writer connection opens an uncommitted write transaction.
    let mut holder = search_metadata::writer::open(&path).unwrap();
    let tx = holder.transaction().unwrap();
    tx.execute(
        "INSERT INTO domain(domain_id, host, created_at) VALUES ('pending','pending.example',1)",
        [],
    )
    .unwrap();

    // Reader sees the committed state, not the uncommitted pending row.
    let r = MetadataReader::from_path(&path).unwrap();
    let res = r.resource_by_id("r1").unwrap();
    assert!(res.is_some(), "committed resource readable during open txn");
    let pending: i64 = r
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM domain WHERE domain_id='pending'",
            [],
            |x| x.get(0),
        )
        .unwrap();
    assert_eq!(pending, 0, "uncommitted writes are invisible to readers");

    drop(tx);
    drop(holder);
}

/// Typed helpers: document_version round-trips through the writer and reader.
#[test]
fn document_version_round_trips() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("meta.db");
    let mut w = MetadataWriter::from_path(&path).unwrap();
    // document_version.content_id has an FK to content, so the content row
    // must exist first.
    w.conn()
        .execute(
            "INSERT INTO content(content_id, exact_hash, text_path, byte_len, created_at)
             VALUES ('cnt_x', 'exh', NULL, 10, 1)",
            [],
        )
        .unwrap();
    w.apply_batch(&[
        domain("d1"),
        resource("r1", "d1"),
        Mutation::UpsertDocumentVersion {
            docver_id: "r1:1".into(),
            resource_id: "r1".into(),
            version_no: 1,
            content_id: Some("cnt_x".into()),
            title: Some("Hello".into()),
            body_hash: Some("bh".into()),
            parser_version: "p1".into(),
            published_at: None,
            modified_at: Some(42),
            fetched_at: 1000,
        },
        // Idempotent: same docver_id again is a no-op.
        Mutation::UpsertDocumentVersion {
            docver_id: "r1:1".into(),
            resource_id: "r1".into(),
            version_no: 1,
            content_id: Some("cnt_x".into()),
            title: Some("Hello".into()),
            body_hash: Some("bh".into()),
            parser_version: "p1".into(),
            published_at: None,
            modified_at: Some(42),
            fetched_at: 1000,
        },
    ])
    .unwrap();

    let r = MetadataReader::from_path(&path).unwrap();
    let dv = r.doc_by_docver("r1:1").unwrap().expect("docver readable");
    assert_eq!(dv.title.as_deref(), Some("Hello"));
    assert_eq!(dv.published_at, None);
    let count: i64 = r
        .conn()
        .query_row("SELECT COUNT(*) FROM document_version", [], |x| x.get(0))
        .unwrap();
    assert_eq!(count, 1, "duplicate docver insert must be a no-op");
}

/// visible_manifest returns only the highest published generation.
#[test]
fn visible_manifest_returns_highest_published() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("meta.db");
    let conn = search_metadata::writer::open(&path).unwrap();
    conn.execute_batch(
        "INSERT INTO manifest(manifest_id,generation,published,doc_count,base_segments,created_at,published_at)
            VALUES ('m1',1,1,10,1,100,100),
                   ('m2',2,0,20,1,200,NULL),
                   ('m3',3,1,30,1,300,300);",
    )
    .unwrap();

    let r = MetadataReader::from_path(&path).unwrap();
    let m = r.visible_manifest().unwrap().expect("a published manifest");
    assert_eq!(m.generation, 3);
    assert_eq!(m.manifest_id, "m3");
    assert_eq!(m.doc_count, 30);
}

/// The background batched writer drains the channel and a graceful shutdown
/// flushes the tail before the thread exits.
#[tokio::test]
async fn background_writer_batches_and_flushes_tail() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("meta.db");

    // Small batch so several commits occur; capacity forces backpressure.
    let handle = spawn_writer_with(&path, 8, Duration::from_millis(20), 4).unwrap();

    handle.send(domain("d1")).await.unwrap();
    for i in 0..50 {
        handle
            .send(resource(&format!("r{i}"), "d1"))
            .await
            .unwrap();
    }
    handle
        .send(journal("op1", "r0", "FETCHED", 1))
        .await
        .unwrap();

    handle.flush().await.unwrap();
    handle.shutdown().await.unwrap();

    let r = MetadataReader::from_path(&path).unwrap();
    assert!(r.resource_by_id("r49").unwrap().is_some());
    let n: i64 = r
        .conn()
        .query_row("SELECT COUNT(*) FROM resource", [], |x| x.get(0))
        .unwrap();
    assert_eq!(n, 50);
    assert_eq!(r.journal_len().unwrap(), 1);
}
