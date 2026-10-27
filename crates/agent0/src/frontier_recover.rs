//! `agent0 frontier recover` — reclaim zombie leases left by a killed crawler.
//!
//! The frontier library has had crash recovery since Stage 0
//! ([`Frontier::release_expired`]): a row left in `FETCHING` with an expired
//! `lease_until` is returned to `READY`, its stale epoch still fencing the dead
//! worker's writes. What was missing is a *caller*. Measured on 2026-10-24, the
//! four fleet shards held **328,712 rows stuck in `FETCHING` with expired
//! leases** — the signature of crawlers killed mid-run — and nothing in the
//! binary ever reclaimed them, so a resumed crawl would have re-leased from a
//! queue that had silently lost a third of a million rows.
//!
//! This module is the caller. It is deliberately read-mostly: it opens each data
//! root's metadata database through the same writer the crawler uses, calls
//! `release_expired`, and prints how many rows each root reclaimed. It never
//! fetches anything and never changes a row that is not already a zombie.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use search_core::config::Config;
use search_frontier::Frontier;

/// Reclaim expired leases in one data root. Returns the number of rows returned
/// to `READY`.
///
/// `data_dir` is a search data root (`…/meta/meta.sqlite` is opened beneath it),
/// which is the same convention every other verb uses.
pub fn recover_dir(data_dir: &Path, now: i64) -> Result<usize, String> {
    let meta = data_dir.join("meta").join("meta.sqlite");
    if !meta.is_file() {
        return Err(format!("no metadata database at {}", meta.display()));
    }
    let frontier = Frontier::new(meta);
    frontier
        .release_expired(now)
        .map_err(|e| format!("{}: {e}", data_dir.display()))
}

/// `agent0 frontier recover [--dir PATH]... [--now EPOCH]`
///
/// With no `--dir`, the current `SEARCH_DATA_DIR` (or the configured default) is
/// used, matching every other verb.
pub fn run(args: &[String]) -> ExitCode {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut now: Option<i64> = None;
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => match args.get(i + 1) {
                Some(v) => {
                    dirs.push(PathBuf::from(v));
                    i += 2;
                }
                None => return usage("--dir needs a path"),
            },
            "--now" => match args.get(i + 1).and_then(|v| v.parse::<i64>().ok()) {
                Some(v) => {
                    now = Some(v);
                    i += 2;
                }
                None => return usage("--now needs a unix timestamp"),
            },
            other => return usage(&format!("unknown argument {other}")),
        }
    }
    if dirs.is_empty() {
        dirs.push(Config::from_env().data_dir);
    }
    let now = now.unwrap_or_else(search_core::time::now_unix_s);

    let mut total = 0usize;
    let mut failed = false;
    for dir in &dirs {
        match recover_dir(dir, now) {
            Ok(n) => {
                total += n;
                println!("{}: reclaimed {n} expired lease(s)", dir.display());
            }
            Err(e) => {
                failed = true;
                eprintln!("error: {e}");
            }
        }
    }
    println!("total reclaimed: {total}");
    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn usage(why: &str) -> ExitCode {
    eprintln!("error: {why}");
    eprintln!("usage: agent0 frontier recover [--dir PATH]... [--now EPOCH]");
    ExitCode::from(2)
}
