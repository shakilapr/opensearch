//! Acceptance tests for metadata migration 0006 — the identity layer
//! (`docs/SEMANTIC_INDEX_ARCHITECTURE.md` §2–§3).
//!
//! The properties pinned here are the ones a later refactor would quietly break:
//! a website may span several hostnames and a hostname may host several websites;
//! a work may have several locations but only one preferred; sitemap-advertised
//! metadata is not the same column as fetched metadata; a source profile is
//! normalised by document count; and resources keep working with no identity.

use rusqlite::Connection;
use tempfile::tempdir;

fn fresh() -> (tempfile::TempDir, Connection) {
    let dir = tempdir().unwrap();
    let conn = search_metadata::writer::open(&dir.path().join("meta.db")).unwrap();
    (dir, conn)
}

fn now() -> i64 {
    search_core::time::now_unix_s()
}

fn domain(conn: &Connection, id: &str, host: &str) {
    conn.execute(
        "INSERT INTO domain(domain_id, host, created_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(domain_id) DO NOTHING",
        rusqlite::params![id, host, now()],
    )
    .unwrap();
}

fn resource(conn: &Connection, rid: &str, host: &str) -> String {
    let url = format!("https://{host}/{rid}");
    conn.execute(
        "INSERT INTO resource(resource_id, url, canonical_url, domain_id, kind, status, first_seen)
         VALUES (?1, ?2, ?2, ?3, 'html', 'NEW', ?4)",
        rusqlite::params![rid, url, host, now()],
    )
    .unwrap();
    url
}

fn website(conn: &Connection, id: &str, name: &str) {
    conn.execute(
        "INSERT INTO website(web_id, name, language, source_type, authority, created_at, updated_at)
         VALUES (?1, ?2, 'en', 'documentation', 0.8, ?3, ?3)",
        rusqlite::params![id, name, now()],
    )
    .unwrap();
}

/// The new tables exist alongside the ones from 0005.
#[test]
fn identity_tables_exist() {
    let (_dir, conn) = fresh();
    for t in ["website", "website_host", "source_section", "sitemap_entry", "work_location", "source_concept"] {
        let present: i64 = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [t],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(present, 1, "missing table {t}");
    }
    for c in ["web_id", "section_id"] {
        let cols: Vec<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(resource)").unwrap();
            stmt.query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert!(cols.iter().any(|x| x == c), "resource is missing column {c}");
    }
}

/// A website can own several hostnames, and one hostname can belong to two
/// different websites — the mapping is many-to-many by design.
#[test]
fn websites_and_hostnames_are_many_to_many() {
    let (_dir, conn) = fresh();
    website(&conn, "W100", "Example Research");
    website(&conn, "W200", "Shared Platform");
    for (w, h) in [
        ("W100", "example.org"),
        ("W100", "docs.example.org"),
        ("W200", "example.org"),
    ] {
        conn.execute(
            "INSERT INTO website_host(web_id, host, primary_host, created_at) VALUES (?1, ?2, 0, ?3)",
            rusqlite::params![w, h, now()],
        )
        .unwrap();
    }
    let hosts_of_w100: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT host FROM website_host WHERE web_id='W100' ORDER BY host")
            .unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
    };
    assert_eq!(hosts_of_w100, vec!["docs.example.org", "example.org"]);
    let owners_of_example: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM website_host WHERE host='example.org'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(owners_of_example, 2, "one hostname may host several websites");
}

/// A work can be reached at a publisher, a preprint and a repository copy, with
/// exactly one marked preferred — and no duplicate row for the same copy.
#[test]
fn a_work_has_several_locations_one_preferred() {
    let (_dir, conn) = fresh();
    domain(&conn, "pub.example", "pub.example");
    domain(&conn, "pre.example", "pre.example");
    resource(&conn, "r1", "pub.example");
    resource(&conn, "r2", "pre.example");
    conn.execute(
        "INSERT INTO work(work_id, id_kind, identifier, title, created_at, updated_at)
         VALUES ('A501','doi','10.1/x','Neuroendocrine mechanisms of pair bonding', ?1, ?1)",
        [now()],
    )
    .unwrap();

    for (rid, rel, pref) in [("r1", "publisher", 1), ("r2", "preprint", 0)] {
        conn.execute(
            "INSERT INTO work_location(work_id, resource_id, relationship, preferred, created_at)
             VALUES ('A501', ?1, ?2, ?3, ?4)",
            rusqlite::params![rid, rel, pref, now()],
        )
        .unwrap();
    }
    let preferred: String = conn
        .query_row(
            "SELECT resource_id FROM work_location WHERE work_id='A501' AND preferred=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(preferred, "r1");

    let dup = conn.execute(
        "INSERT INTO work_location(work_id, resource_id, relationship, preferred, created_at)
         VALUES ('A501','r1','copy',0,1)",
        [],
    );
    assert!(dup.is_err(), "one location is recorded once per work");

    let bad_rel = conn.execute(
        "INSERT INTO work_location(work_id, resource_id, relationship, preferred, created_at)
         VALUES ('A501','r2','mirror',0,1)",
        [],
    );
    assert!(bad_rel.is_err(), "unknown relationship kinds are rejected");
}

/// Sitemap-advertised metadata is a separate column from what a fetch observed.
#[test]
fn sitemap_entries_keep_advertised_hints_apart_from_fetched_facts() {
    let (_dir, conn) = fresh();
    domain(&conn, "d1.example", "d1.example");
    let url = resource(&conn, "r1", "d1.example");
    conn.execute(
        "INSERT INTO sitemap(sitemap_id, url, domain_id, status, created_at, updated_at)
         VALUES ('S10','https://d1.example/sitemap.xml','d1.example','OK',1,1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sitemap_entry(sitemap_id, url, resource_id, advertised_last_mod, created_at)
         VALUES ('S10', ?1, 'r1', 'Mon, 10 Aug 2026 06:18:00 GMT', ?2)",
        rusqlite::params![url, now()],
    )
    .unwrap();

    let advertised: String = conn
        .query_row("SELECT advertised_last_mod FROM sitemap_entry", [], |r| r.get(0))
        .unwrap();
    let fetched: Option<i64> = conn
        .query_row("SELECT modified_at FROM resource WHERE resource_id='r1'", [], |r| r.get(0))
        .unwrap();
    assert!(advertised.starts_with("Mon,"));
    assert_eq!(fetched, None, "the sitemap hint does not become a fetched timestamp");
}

/// Source-level concept profiles store the document count they were normalised
/// against, so a big site cannot look topical merely by having more pages.
#[test]
fn source_concept_profiles_record_their_normalisation_basis() {
    let (_dir, conn) = fresh();
    conn.execute(
        "INSERT INTO concept(concept_id, name, kind, status, created_at, updated_at)
         VALUES ('C93','Hormones','topic','active',1,1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO source_concept(source_id, concept_id, weight, doc_count, model_version, updated_at)
         VALUES ('research.example','C93', 0.42, 20, 'v1', 1)",
        [],
    )
    .unwrap();
    let (weight, docs): (f64, i64) = conn
        .query_row("SELECT weight, doc_count FROM source_concept", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert!((weight - 0.42).abs() < 1e-9);
    assert_eq!(docs, 20, "the normalisation basis is stored, not implied");

    let out_of_range = conn.execute(
        "INSERT INTO source_concept(source_id, concept_id, weight, doc_count, updated_at)
         VALUES ('other.example','C93', 1.5, 1, 1)",
        [],
    );
    assert!(out_of_range.is_err(), "weights stay in 0..1");
}

/// Existing resources keep working with no identity, and the identity can be
/// attached later without touching the resource's other columns.
#[test]
fn resources_work_without_an_identity_and_accept_one_later() {
    let (_dir, conn) = fresh();
    domain(&conn, "d1.example", "d1.example");
    resource(&conn, "r1", "d1.example");

    let none: Option<String> = conn
        .query_row("SELECT web_id FROM resource WHERE resource_id='r1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(none, None, "identity is optional for every existing row");

    website(&conn, "W100", "Example");
    conn.execute(
        "UPDATE resource SET web_id='W100', section_id='SEC1' WHERE resource_id='r1'",
        [],
    )
    .unwrap();
    let web: String = conn
        .query_row("SELECT web_id FROM resource WHERE resource_id='r1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(web, "W100");
}
