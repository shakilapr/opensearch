//! Schema DDL and migration runner (spec §4). OWNER: metadata agent.

pub const SCHEMA_VERSION: u64 = 1;

/// The authoritative DDL. Kept as one string so migration 0001 is exact.
pub const DDL_V1: &str = include_str!("../migrations/0001_init.sql");

/// Apply all migrations to a connection. Idempotent.
pub fn apply(conn: &rusqlite::Connection) -> search_core::AppResult<()> {
    conn.execute_batch(DDL_V1).map_err(crate::db_err)?;
    Ok(())
}
