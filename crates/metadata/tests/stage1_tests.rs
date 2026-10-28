//! Stage-1 acceptance tests for the metadata additions (spec:
//! `docs/STAGE1_SPEC.md` §"Metadata additions (Stage 1)", migration 0003).
//!
//! Covers the new tables/repositories, the `resource` freshness columns, the
//! new `Mutation` variants (all writes through the single-writer API), and the
//! FK behaviour of the new relations.

use search_metadata::{MetadataReader, MetadataWriter, Mutation};
use tempfile::tempdir;

fn open_writer(dir: &tempfile::TempDir) -> MetadataWriter {
    MetadataWriter::from_path(&dir.path().join("meta.db")).unwrap()
}

fn column_names(conn: &rusqlite::Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    stmt.query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn count(conn: &rusqlite::Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

/// Migration 0006 present: applying all migrations twice is idempotent and the
/// ledger is exactly [1, 2, 3, 4, 5, 6]; SCHEMA_VERSION is 6.
#[test]
fn migrations_idempotent_and_ledger_is_1_2_3() {
    assert_eq!(search_metadata::migrations::SCHEMA_VERSION, 6);
    let dir = tempdir().unwrap();
    let path = dir.path().join("meta.db");
    let conn = search_metadata::writer::open(&path).unwrap();
    // `open` applied them; run the runner twice more to prove idempotency.
    search_metadata::migrations::apply(&conn).unwrap();
    search_metadata::migrations::apply(&conn).unwrap();

    let versions: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT version FROM schema_migration ORDER BY version")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert_eq!(versions, vec![1, 2, 3, 4, 5, 6, 7], "ledger has each version once");
}

/// Every Stage-1 table exists with its expected columns.
#[test]
fn new_tables_exist_with_expected_columns() {
    let dir = tempdir().unwrap();
    let conn = search_metadata::writer::open(&dir.path().join("meta.db")).unwrap();

    let expected: &[(&str, &[&str])] = &[
        (
            "sitemap",
            &[
                "sitemap_id", "url", "source_url", "domain_id", "is_index", "status",
                "url_count", "etag", "last_modified", "fetched_at", "created_at", "updated_at",
            ],
        ),
        (
            "feed",
            &[
                "feed_id", "url", "domain_id", "title", "feed_type", "status", "entry_count",
                "etag", "last_modified", "watermark", "fetched_at", "created_at", "updated_at",
            ],
        ),
        (
            "source_profile",
            &[
                "source_id", "domain", "topics", "entities", "url_patterns", "authority",
                "change_rate", "js_probability", "fetch_count", "error_count", "created_at",
                "updated_at",
            ],
        ),
        ("entity", &["entity_id", "name", "kind", "created_at", "updated_at"]),
        ("entity_alias", &["entity_id", "alias", "created_at"]),
        (
            "graph_edge",
            &["from_id", "to_id", "kind", "weight", "created_at", "updated_at"],
        ),
        (
            "work",
            &[
                "work_id", "id_kind", "identifier", "title", "published_at", "modified_at",
                "resource_id", "created_at", "updated_at",
            ],
        ),
    ];

    for (table, cols) in expected {
        let have = column_names(&conn, table);
        for c in *cols {
            assert!(
                have.iter().any(|x| x == c),
                "table `{table}` missing column `{c}` (have {have:?})"
            );
        }
    }
}

/// The five freshness columns were added to the existing `resource` table, and
/// the pre-existing columns are preserved (not duplicated).
#[test]
fn resource_freshness_columns_exist() {
    let dir = tempdir().unwrap();
    let conn = search_metadata::writer::open(&dir.path().join("meta.db")).unwrap();
    let cols = column_names(&conn, "resource");

    for c in [
        "published_at",
        "modified_at",
        "fetched_at",
        "verified_at",
        "feed_watermark",
    ] {
        assert_eq!(
            cols.iter().filter(|x| x.as_str() == c).count(),
            1,
            "resource must have exactly one `{c}` column"
        );
    }
    // Stage-0 columns are still present and distinct.
    for c in ["first_seen", "last_fetch"] {
        assert!(cols.iter().any(|x| x == c), "resource kept `{c}`");
    }
}

/// `UpdateResourceFreshness` writes the freshness columns; a second call only
/// touches provided fields (COALESCE keeps the rest).
#[test]
fn resource_freshness_mutation_updates_and_preserves() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("meta.db");
    let mut w = open_writer(&dir);
    w.apply_batch(&[
        Mutation::UpsertDomain {
            domain_id: "d1".into(),
            host: "d1.example".into(),
        },
        Mutation::UpsertResource {
            resource_id: "r1".into(),
            url: "https://d1.example/r1".into(),
            canonical_url: "https://d1.example/r1".into(),
            domain_id: "d1".into(),
            source_id: None,
            kind: "html".into(),
        },
        Mutation::UpdateResourceFreshness {
            resource_id: "r1".into(),
            published_at: Some(100),
            modified_at: Some(200),
            fetched_at: Some(300),
            verified_at: Some(400),
            feed_watermark: Some(500),
        },
        // Only `verified_at` provided: the others must be preserved.
        Mutation::UpdateResourceFreshness {
            resource_id: "r1".into(),
            published_at: None,
            modified_at: None,
            fetched_at: None,
            verified_at: Some(999),
            feed_watermark: None,
        },
    ])
    .unwrap();

    let r = MetadataReader::from_path(&path).unwrap();
    let res = r.resource_by_id("r1").unwrap().unwrap();
    assert_eq!(res.published_at, Some(100));
    assert_eq!(res.modified_at, Some(200));
    assert_eq!(res.fetched_at, Some(300));
    assert_eq!(res.verified_at, Some(999), "second call updated verified_at");
    assert_eq!(res.feed_watermark, Some(500), "unspecified column preserved");
}

/// `UpsertSitemap` is idempotent (twice → one row) and updates mutable fields.
#[test]
fn sitemap_upsert_idempotent_and_updates() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("meta.db");
    let mut w = open_writer(&dir);
    let base = |url_count: i64, status: &str| Mutation::UpsertSitemap {
        sitemap_id: "s1".into(),
        url: "https://d1.example/sitemap.xml".into(),
        source_url: Some("https://d1.example/robots.txt".into()),
        domain_id: None,
        is_index: false,
        status: status.into(),
        url_count,
        etag: Some("W/\"a\"".into()),
        last_modified: None,
        fetched_at: Some(10),
    };
    w.apply_batch(&[base(3, "NEW"), base(3, "NEW"), base(7, "OK")])
        .unwrap();

    let r = MetadataReader::from_path(&path).unwrap();
    let s = r.sitemap_by_id("s1").unwrap().unwrap();
    assert_eq!(s.url_count, 7, "update path applied");
    assert_eq!(s.status, "OK");
    assert_eq!(count(r.conn(), "sitemap"), 1, "upsert is idempotent");
}

/// `UpsertFeed` is idempotent and updates mutable fields (watermark/count).
#[test]
fn feed_upsert_idempotent_and_updates() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("meta.db");
    let mut w = open_writer(&dir);
    let base = |watermark: Option<&str>, entry_count: i64| Mutation::UpsertFeed {
        feed_id: "f1".into(),
        url: "https://d1.example/feed.xml".into(),
        domain_id: None,
        title: Some("Blog".into()),
        feed_type: "atom".into(),
        status: "OK".into(),
        entry_count,
        etag: None,
        last_modified: None,
        watermark: watermark.map(Into::into),
        fetched_at: Some(1),
    };
    w.apply_batch(&[base(Some("e1"), 5), base(Some("e1"), 5), base(Some("e9"), 9)])
        .unwrap();

    let r = MetadataReader::from_path(&path).unwrap();
    let f = r.feed_by_id("f1").unwrap().unwrap();
    assert_eq!(f.watermark.as_deref(), Some("e9"), "watermark updated");
    assert_eq!(f.entry_count, 9);
    assert_eq!(f.feed_type, "atom");
    assert_eq!(count(r.conn(), "feed"), 1, "upsert is idempotent");
}

/// `UpsertSourceProfile` is idempotent and updates the running statistics.
#[test]
fn source_profile_upsert_idempotent_and_updates() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("meta.db");
    let mut w = open_writer(&dir);
    let base = |authority: f64, fetch_count: i64| Mutation::UpsertSourceProfile {
        source_id: "sp1".into(),
        domain: "d1.example".into(),
        topics: "[\"llm\"]".into(),
        entities: "[]".into(),
        url_patterns: "[\"/blog/*\"]".into(),
        authority,
        change_rate: 0.1,
        js_probability: 0.2,
        fetch_count,
        error_count: 1,
    };
    w.apply_batch(&[base(0.5, 10), base(0.5, 10), base(0.9, 42)])
        .unwrap();

    let r = MetadataReader::from_path(&path).unwrap();
    let sp = r.source_profile_by_id("sp1").unwrap().unwrap();
    assert_eq!(sp.authority, 0.9, "authority updated");
    assert_eq!(sp.fetch_count, 42);
    assert_eq!(sp.topics, "[\"llm\"]");
    assert_eq!(count(r.conn(), "source_profile"), 1, "upsert is idempotent");
}

/// `UpsertEntity` updates in place; `UpsertEntityAlias` is idempotent on the
/// composite key and requires the entity (FK).
#[test]
fn entity_and_alias_upsert_idempotent() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("meta.db");
    let mut w = open_writer(&dir);
    w.apply_batch(&[
        Mutation::UpsertEntity {
            entity_id: "e1".into(),
            name: "Large Language Model".into(),
            kind: "concept".into(),
        },
        Mutation::UpsertEntityAlias {
            entity_id: "e1".into(),
            alias: "LLM".into(),
        },
        // Duplicate alias: no-op.
        Mutation::UpsertEntityAlias {
            entity_id: "e1".into(),
            alias: "LLM".into(),
        },
        Mutation::UpsertEntityAlias {
            entity_id: "e1".into(),
            alias: "large-language-model".into(),
        },
        // Update path: rename/rekind the entity.
        Mutation::UpsertEntity {
            entity_id: "e1".into(),
            name: "LLM".into(),
            kind: "concept".into(),
        },
    ])
    .unwrap();

    let r = MetadataReader::from_path(&path).unwrap();
    let e = r.entity_by_id("e1").unwrap().unwrap();
    assert_eq!(e.name, "LLM", "entity update path applied");
    let aliases: Vec<String> = r
        .entity_aliases("e1")
        .unwrap()
        .into_iter()
        .map(|a| a.alias)
        .collect();
    assert_eq!(aliases, vec!["LLM", "large-language-model"]);
    assert_eq!(count(r.conn(), "entity"), 1);
    assert_eq!(count(r.conn(), "entity_alias"), 2, "duplicate alias no-op");

    // FK: an alias for an unknown entity must fail.
    let err = w.apply_batch(&[Mutation::UpsertEntityAlias {
        entity_id: "ghost".into(),
        alias: "x".into(),
    }]);
    assert!(err.is_err(), "alias without entity violates FK");
}

/// `UpsertGraphEdge` is idempotent on the composite key and updates the weight;
/// both endpoint entities must exist (FK).
#[test]
fn graph_edge_upsert_updates_weight_and_enforces_fk() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("meta.db");
    let mut w = open_writer(&dir);
    let base = |weight: f64| Mutation::UpsertGraphEdge {
        from_id: "a".into(),
        to_id: "b".into(),
        kind: "related_to".into(),
        weight,
    };
    w.apply_batch(&[
        Mutation::UpsertEntity {
            entity_id: "a".into(),
            name: "A".into(),
            kind: "concept".into(),
        },
        Mutation::UpsertEntity {
            entity_id: "b".into(),
            name: "B".into(),
            kind: "concept".into(),
        },
        base(0.5),
        base(0.5),
        base(0.8),
    ])
    .unwrap();

    let r = MetadataReader::from_path(&path).unwrap();
    let e = r.graph_edge("a", "b", "related_to").unwrap().unwrap();
    assert_eq!(e.weight, 0.8, "edge weight update path applied");
    assert_eq!(count(r.conn(), "graph_edge"), 1, "upsert is idempotent");

    // FK: an edge to an unknown entity must fail.
    let err = w.apply_batch(&[Mutation::UpsertGraphEdge {
        from_id: "a".into(),
        to_id: "ghost".into(),
        kind: "related_to".into(),
        weight: 1.0,
    }]);
    assert!(err.is_err(), "edge to unknown entity violates FK");
}

/// `UpsertWork` is idempotent on `work_id` and updates mutable fields; the
/// `resource_id` FK is enforced.
#[test]
fn work_upsert_idempotent_and_enforces_fk() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("meta.db");
    let mut w = open_writer(&dir);
    let base = |title: &str| Mutation::UpsertWork {
        work_id: "doi:10.1234/x".into(),
        id_kind: "doi".into(),
        identifier: "10.1234/x".into(),
        title: Some(title.into()),
        published_at: Some(100),
        modified_at: None,
        resource_id: None,
    };
    w.apply_batch(&[base("First"), base("First"), base("Revised")])
        .unwrap();

    let r = MetadataReader::from_path(&path).unwrap();
    let wk = r.work_by_id("doi:10.1234/x").unwrap().unwrap();
    assert_eq!(wk.title.as_deref(), Some("Revised"), "update path applied");
    assert_eq!(wk.id_kind, "doi");
    assert_eq!(count(r.conn(), "work"), 1, "upsert is idempotent");

    // FK: a work pointing at an unknown resource must fail.
    let err = w.apply_batch(&[Mutation::UpsertWork {
        work_id: "arxiv:2301.00001".into(),
        id_kind: "arxiv".into(),
        identifier: "2301.00001".into(),
        title: None,
        published_at: None,
        modified_at: None,
        resource_id: Some("no_such_resource".into()),
    }]);
    assert!(err.is_err(), "work with unknown resource_id violates FK");
}

/// A full Stage-1 write batch round-trips through the single-writer API and is
/// readable via the reader lookups.
#[test]
fn stage1_batch_round_trips_through_writer() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("meta.db");
    let mut w = open_writer(&dir);
    w.apply_batch(&[
        Mutation::UpsertDomain {
            domain_id: "d1".into(),
            host: "d1.example".into(),
        },
        Mutation::UpsertResource {
            resource_id: "r1".into(),
            url: "https://d1.example/doc".into(),
            canonical_url: "https://d1.example/doc".into(),
            domain_id: "d1".into(),
            source_id: None,
            kind: "pdf".into(),
        },
        Mutation::UpsertSitemap {
            sitemap_id: "s1".into(),
            url: "https://d1.example/sitemap.xml".into(),
            source_url: None,
            domain_id: Some("d1".into()),
            is_index: true,
            status: "OK".into(),
            url_count: 2,
            etag: None,
            last_modified: None,
            fetched_at: Some(5),
        },
        Mutation::UpsertWork {
            work_id: "pmid:12345".into(),
            id_kind: "pmid".into(),
            identifier: "12345".into(),
            title: Some("A Paper".into()),
            published_at: Some(50),
            modified_at: Some(60),
            resource_id: Some("r1".into()),
        },
    ])
    .unwrap();

    let r = MetadataReader::from_path(&path).unwrap();
    let sm = r.sitemap_by_id("s1").unwrap().unwrap();
    assert_eq!(sm.is_index, 1);
    assert_eq!(sm.domain_id.as_deref(), Some("d1"));
    let wk = r.work_by_id("pmid:12345").unwrap().unwrap();
    assert_eq!(wk.resource_id.as_deref(), Some("r1"));
    assert_eq!(wk.modified_at, Some(60));
}
