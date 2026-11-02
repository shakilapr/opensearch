//! Stage-5 acceptance tests for metadata migration 0004 (`docs/specs/STAGE5_SPEC.md`).
//!
//! Covers the `resource.storage_tier` column + index, the `Mutation::UpdateStorageTier`
//! round-trip through the single-writer API, the `resource_tier` reader, and that the migration
//! runner applies 0001..0004 in order on a fresh database.

use search_metadata::{MetadataReader, MetadataWriter, Mutation};
use tempfile::tempdir;

fn resource(rid: &str) -> Mutation {
    Mutation::UpsertResource {
        resource_id: rid.into(),
        url: format!("https://d1.example/{rid}"),
        canonical_url: format!("https://d1.example/{rid}"),
        domain_id: "d1".into(),
        source_id: None,
        kind: "html".into(),
    }
}

/// The runner applies 0001..0006 in order on a fresh DB; SCHEMA_VERSION is 6.
#[test]
fn migration_runner_applies_0001_through_0004_in_order() {
    assert_eq!(search_metadata::migrations::SCHEMA_VERSION, 9);

    let dir = tempdir().unwrap();
    let path = dir.path().join("meta.db");
    let conn = search_metadata::writer::open(&path).unwrap();
    // Re-run to prove idempotency (non-idempotent ALTER stays applied once).
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
    let expected: Vec<i64> = search_metadata::migrations::MIGRATIONS
        .iter()
        .map(|(v, _)| *v as i64)
        .collect();
    assert_eq!(versions, expected, "ledger records each version once, in order");
}

/// `resource.storage_tier` exists with the expected `TEXT NOT NULL DEFAULT 'hot'` shape, and the
/// 0004 index is present.
#[test]
fn storage_tier_column_and_index_exist() {
    let dir = tempdir().unwrap();
    let conn = search_metadata::writer::open(&dir.path().join("meta.db")).unwrap();

    // PRAGMA table_info columns: (cid, name, type, notnull, dflt_value, pk).
    let (ty, notnull, default): (String, i64, Option<String>) = conn
        .query_row(
            "SELECT type, \"notnull\", dflt_value FROM pragma_table_info('resource')
             WHERE name = 'storage_tier'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(ty, "TEXT");
    assert_eq!(notnull, 1, "storage_tier is NOT NULL");
    assert_eq!(default.as_deref(), Some("'hot'"), "default is 'hot'");

    let idx: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type='index' AND name='idx_resource_storage_tier'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(idx, 1, "the 0004 index is created");
}

/// A freshly registered resource defaults to `hot`; `UpdateStorageTier` changes it and the
/// `resource_tier` reader reflects the new value.
#[test]
fn update_storage_tier_round_trips() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("meta.db");
    let mut w = MetadataWriter::from_path(&path).unwrap();

    w.apply_batch(&[
        Mutation::UpsertDomain {
            domain_id: "d1".into(),
            host: "d1.example".into(),
        },
        resource("r1"),
    ])
    .unwrap();

    let reader = MetadataReader::from_path(&path).unwrap();
    assert_eq!(
        reader.resource_tier("r1").unwrap().as_deref(),
        Some("hot"),
        "new resources default to hot"
    );

    // Demote to cold, then promote to warm.
    w.apply_batch(&[Mutation::UpdateStorageTier {
        resource_id: "r1".into(),
        tier: "cold".into(),
    }])
    .unwrap();
    let reader = MetadataReader::from_path(&path).unwrap();
    assert_eq!(reader.resource_tier("r1").unwrap().as_deref(), Some("cold"));

    w.apply_batch(&[Mutation::UpdateStorageTier {
        resource_id: "r1".into(),
        tier: "warm".into(),
    }])
    .unwrap();
    let reader = MetadataReader::from_path(&path).unwrap();
    assert_eq!(reader.resource_tier("r1").unwrap().as_deref(), Some("warm"));

    // Unknown resource -> None (not an error).
    assert_eq!(reader.resource_tier("ghost").unwrap(), None);
}
