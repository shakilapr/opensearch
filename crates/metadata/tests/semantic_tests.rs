//! Acceptance tests for metadata migration 0005 — the semantic index
//! (`docs/SEMANTIC_INDEX_ARCHITECTURE.md`).
//!
//! What is pinned here is the part that is expensive to get wrong later:
//! the *shape* of the associations. A concept can point at documents without a
//! full scan, a document can hold a direct claim and an inferred one without the
//! two being merged into one number, every inferred fact names its model, and
//! the schema refuses the states that would corrupt a rebuild (weight outside
//! 0..1, a concept related to itself, an unknown relation kind).

use rusqlite::Connection;
use tempfile::tempdir;

/// Open a fresh database with 0001..0005 applied.
fn fresh() -> (tempfile::TempDir, Connection) {
    let dir = tempdir().unwrap();
    let conn = search_metadata::writer::open(&dir.path().join("meta.db")).unwrap();
    (dir, conn)
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        == 1
}

fn now() -> i64 {
    search_core::time::now_unix_s()
}

/// Seed a resource the way the writer does, so foreign keys are satisfiable.
fn resource(conn: &Connection, rid: &str) {
    conn.execute(
        "INSERT INTO domain(domain_id, host, created_at) VALUES ('d1','d1.example',?1)
         ON CONFLICT(domain_id) DO NOTHING",
        [now()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO resource(resource_id, url, canonical_url, domain_id, kind, status, first_seen)
         VALUES (?1, ?2, ?2, 'd1', 'html', 'NEW', ?3)",
        rusqlite::params![rid, format!("https://d1.example/{rid}"), now()],
    )
    .unwrap();
}

fn concept(conn: &Connection, id: &str, name: &str) {
    conn.execute(
        "INSERT INTO concept(concept_id, name, kind, status, created_at, updated_at)
         VALUES (?1, ?2, 'topic', 'active', ?3, ?3)",
        rusqlite::params![id, name, now()],
    )
    .unwrap();
}

fn assoc(conn: &Connection, rid: &str, cid: &str, weight: f64, kind: &str, model: Option<&str>) {
    conn.execute(
        "INSERT INTO doc_concept(resource_id, concept_id, weight, relation_kind, model_version,
                                 created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        rusqlite::params![rid, cid, weight, kind, model, now()],
    )
    .unwrap();
}

/// All eight tables of the semantic layer exist.
#[test]
fn semantic_tables_exist() {
    let (_dir, conn) = fresh();
    for t in [
        "concept",
        "concept_alias",
        "concept_relation",
        "doc_concept",
        "passage",
        "passage_concept",
        "web_link",
        "model_registry",
    ] {
        assert!(table_exists(&conn, t), "missing table {t}");
    }
}

/// The direction that matters: concept -> documents, strongest first, without
/// scanning the document table.
#[test]
fn concept_to_documents_is_ordered_by_weight() {
    let (_dir, conn) = fresh();
    resource(&conn, "r1");
    resource(&conn, "r2");
    resource(&conn, "r3");
    concept(&conn, "C207", "Romantic attachment");
    assoc(&conn, "r1", "C207", 0.91, "direct", Some("m1"));
    assoc(&conn, "r2", "C207", 0.42, "direct", Some("m1"));
    assoc(&conn, "r3", "C207", 0.66, "direct", Some("m1"));

    let mut stmt = conn
        .prepare(
            "SELECT resource_id, weight FROM doc_concept
              WHERE concept_id = ?1 ORDER BY weight DESC",
        )
        .unwrap();
    let rows: Vec<(String, f64)> = stmt
        .query_map(["C207"], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        vec!["r1", "r3", "r2"],
        "concept lookup returns documents strongest-first"
    );
    assert!((rows[0].1 - 0.91).abs() < 1e-9);
}

/// A direct claim and an inferred claim for the same (document, concept) are two
/// rows, not one averaged number — that is what stops a model reinforcing its
/// own inference on the next pass.
#[test]
fn direct_and_inferred_coexist_and_stay_separate() {
    let (_dir, conn) = fresh();
    resource(&conn, "r1");
    concept(&conn, "C93", "Hormones");
    assoc(&conn, "r1", "C93", 0.94, "direct", Some("m1"));
    assoc(&conn, "r1", "C93", 0.65, "inferred", Some("m2"));

    let mut stmt = conn
        .prepare(
            "SELECT relation_kind, weight, model_version FROM doc_concept
              WHERE resource_id='r1' AND concept_id='C93'
              ORDER BY (relation_kind='direct') DESC",
        )
        .unwrap();
    let rows: Vec<(String, f64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows.len(), 2, "both claims are retained");
    assert_eq!(rows[0].0, "direct", "preference order puts direct first");
    assert!((rows[0].1 - 0.94).abs() < 1e-9, "direct score is not blended");
    assert!((rows[1].1 - 0.65).abs() < 1e-9, "inferred score is kept as-is");
}

/// The schema refuses weights outside 0..1 and unknown relation kinds.
#[test]
fn schema_rejects_invalid_associations() {
    let (_dir, conn) = fresh();
    resource(&conn, "r1");
    concept(&conn, "C1", "Chemistry");

    let bad_weight = conn.execute(
        "INSERT INTO doc_concept(resource_id, concept_id, weight, relation_kind, created_at, updated_at)
         VALUES ('r1','C1', 1.4, 'direct', 1, 1)",
        [],
    );
    assert!(bad_weight.is_err(), "weight > 1 must be rejected");

    let bad_kind = conn.execute(
        "INSERT INTO doc_concept(resource_id, concept_id, weight, relation_kind, created_at, updated_at)
         VALUES ('r1','C1', 0.5, 'guess', 1, 1)",
        [],
    );
    assert!(bad_kind.is_err(), "unknown relation_kind must be rejected");
}

/// A concept cannot be related to itself, and aliases are unique per concept.
#[test]
fn relation_and_alias_constraints_hold() {
    let (_dir, conn) = fresh();
    concept(&conn, "C1", "Chemistry");
    concept(&conn, "C2", "Love");

    let self_edge = conn.execute(
        "INSERT INTO concept_relation(src_concept_id, dst_concept_id, relation, created_at)
         VALUES ('C1','C1','related', 1)",
        [],
    );
    assert!(self_edge.is_err(), "a self relation must be rejected");

    conn.execute(
        "INSERT INTO concept_relation(src_concept_id, dst_concept_id, relation, created_at)
         VALUES ('C2','C1','related', 1)",
        [],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO concept_alias(concept_id, alias, lang, created_at) VALUES ('C1','chem','en',1)",
        [],
    )
    .unwrap();
    let dup = conn.execute(
        "INSERT INTO concept_alias(concept_id, alias, lang, created_at) VALUES ('C1','chem','en',1)",
        [],
    );
    assert!(dup.is_err(), "an alias is stored once per concept");
}

/// Deleting a resource removes its associations (no orphan weights), and the
/// links derived from it.
#[test]
fn deleting_a_resource_cascades_its_associations_and_links() {
    let (_dir, conn) = fresh();
    resource(&conn, "r1");
    concept(&conn, "C207", "Romantic attachment");
    assoc(&conn, "r1", "C207", 0.8, "direct", Some("m1"));
    conn.execute(
        "INSERT INTO web_link(src_resource_id, dst_url, anchor, internal, created_at)
         VALUES ('r1','https://d1.example/x','x',1,1)",
        [],
    )
    .unwrap();

    conn.execute("DELETE FROM resource WHERE resource_id='r1'", []).unwrap();

    let assocs: i64 = conn
        .query_row("SELECT COUNT(*) FROM doc_concept", [], |r| r.get(0))
        .unwrap();
    let links: i64 = conn
        .query_row("SELECT COUNT(*) FROM web_link", [], |r| r.get(0))
        .unwrap();
    assert_eq!(assocs, 0, "associations cascade with the resource");
    assert_eq!(links, 0, "links cascade with the resource");
}

/// Every inferred fact can name its model, and the registry keeps versions
/// apart so a rollback does not require re-running the model.
#[test]
fn model_versions_are_addressable() {
    let (_dir, conn) = fresh();
    for (id, version, status) in [("concept-clf", "v1", "retired"), ("concept-clf", "v2", "active")] {
        conn.execute(
            "INSERT INTO model_registry(model_id, version, kind, status, created_at)
             VALUES (?1, ?2, 'multilabel', ?3, 1)",
            rusqlite::params![id, version, status],
        )
        .unwrap();
    }
    let active: String = conn
        .query_row(
            "SELECT version FROM model_registry WHERE model_id='concept-clf' AND status='active'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(active, "v2");
}
