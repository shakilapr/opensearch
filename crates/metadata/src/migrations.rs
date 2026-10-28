//! Schema DDL and migration runner (spec §4). OWNER: metadata agent.
//!
//! Migrations are ordered, append-only DDL files under `migrations/`. The
//! runner applies every version not yet recorded in the `schema_migration`
//! ledger, each inside its own transaction, so:
//!
//! * re-running `apply` is idempotent (applied versions are skipped), and
//! * future migrations that are *not* naturally idempotent (e.g. `ALTER TABLE
//!   ADD COLUMN`, which has no `IF NOT EXISTS` form in SQLite) are still
//!   applied at most once.
//!
//! Append new migrations to [`MIGRATIONS`] with the next integer version; never
//! edit a released version.

/// Highest schema version known to this build.
pub const SCHEMA_VERSION: u64 = 6;

/// Ordered `(version, DDL)` migrations. Kept as string constants so migration
/// `0001` is byte-exact.
pub const MIGRATIONS: &[(u64, &str)] = &[
    (1, include_str!("../migrations/0001_init.sql")),
    (2, include_str!("../migrations/0002_watermark.sql")),
    (3, include_str!("../migrations/0003_stage1.sql")),
    (4, include_str!("../migrations/0004_storage_tier.sql")),
    (5, include_str!("../migrations/0005_semantic_index.sql")),
    (6, include_str!("../migrations/0006_identity_layer.sql")),
    (7, include_str!("../migrations/0007_frontier_dispatch_index.sql")),
];

/// Migration ledger: records the version of every migration already applied.
const LEDGER_DDL: &str = "\
CREATE TABLE IF NOT EXISTS schema_migration (
  version    INTEGER PRIMARY KEY,
  applied_at INTEGER NOT NULL
);";

/// Apply all pending migrations to a connection. Idempotent: versions already
/// recorded in `schema_migration` are skipped, and each applied migration
/// commits atomically together with its ledger row.
pub fn apply(conn: &rusqlite::Connection) -> search_core::AppResult<()> {
    conn.execute_batch(LEDGER_DDL).map_err(crate::db_err)?;

    for (version, ddl) in MIGRATIONS {
        let applied: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM schema_migration WHERE version = ?1)",
                rusqlite::params![version],
                |r| r.get(0),
            )
            .map_err(crate::db_err)?;
        if applied {
            continue;
        }

        // One transaction per migration: the DDL and its ledger row land
        // together, so a crash cannot leave a migration half-applied.
        let tx = conn.unchecked_transaction().map_err(crate::db_err)?;
        tx.execute_batch(ddl).map_err(crate::db_err)?;
        tx.execute(
            "INSERT INTO schema_migration(version, applied_at) VALUES (?1, ?2)",
            rusqlite::params![version, search_core::time::now_unix_s()],
        )
        .map_err(crate::db_err)?;
        tx.commit().map_err(crate::db_err)?;
    }

    Ok(())
}
