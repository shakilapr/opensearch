//! Apply pending schema migrations to a data root's metadata database.
//!
//! `MetadataWriter::from_path` runs the migration ledger on open, but nothing in the operator CLI
//! exposed that: migrations were only applied as a side effect of a crawl starting. A fleet whose
//! shards were crawled by an older binary therefore lags the schema until the next crawl — measured
//! 2026-10-29, shards 1–3 sat at migration 7 while shard 0 was at 8, so `doc_concept_dim` did not
//! exist on three quarters of the corpus.
//!
//! Usage: `agent0 migrate [--dir PATH]...`

use std::path::PathBuf;
use std::process::ExitCode;

use search_core::config::Config;

pub fn run(args: &[String]) -> ExitCode {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut i = 2;
    while i < args.len() {
        if args[i] == "--dir" {
            match args.get(i + 1) {
                Some(v) => {
                    dirs.push(PathBuf::from(v));
                    i += 2;
                }
                None => {
                    eprintln!("error: --dir needs a path");
                    return ExitCode::from(2);
                }
            }
        } else {
            break;
        }
    }
    if dirs.is_empty() {
        dirs.push(Config::from_env().data_dir);
    }

    let mut failed = false;
    for dir in &dirs {
        let db = dir.join("meta").join("meta.sqlite");
        if !db.is_file() {
            eprintln!("{}: no metadata database at {}", dir.display(), db.display());
            failed = true;
            continue;
        }
        // Record the version before, so the report distinguishes "applied" from "already current".
        let before = max_version(&db);
        // Opening a writer is what applies the ledger; it is dropped immediately.
        match search_metadata::MetadataWriter::from_path(&db) {
            Ok(_) => {}
            Err(e) => {
                eprintln!("{}: {e}", dir.display());
                failed = true;
                continue;
            }
        }
        let after = max_version(&db);
        match (before, after) {
            (Some(b), Some(a)) if a > b => {
                println!("{}: migrated {b} -> {a}", dir.display())
            }
            (_, Some(a)) => println!("{}: already at {a}", dir.display()),
            _ => println!("{}: no ledger readable", dir.display()),
        }
    }
    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn max_version(db: &std::path::Path) -> Option<i64> {
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;
    conn.query_row("SELECT MAX(version) FROM schema_migration", [], |r| r.get(0))
        .ok()
}
