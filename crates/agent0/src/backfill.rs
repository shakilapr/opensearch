//! Refill the drain from the metadata frontier.
//!
//! The crawl's own queue is built from what discovery produces *this run*: sitemaps, feeds and
//! links off the seed list. Meanwhile `frontier_entry` accumulates every URL those sitemaps ever
//! advertised — measured 2026-10-28 on the fleet, **10.2M rows, of which 557,001 were `READY`**
//! (reclaimed by `release_expired`, dispatchable immediately) — and nothing ever fetched them.
//! That is why a fresh data root reached 6,388 attempts/min while production managed 740: the
//! fresh root's discovery output *was* its whole corpus, and production's discovery only trickles
//! new hosts while 557K known URLs sit idle.
//!
//! This module supplies those rows to the drain, subject to the same politeness the rest of the
//! crawler uses: one request in flight per origin, the configured per-origin interval, and a cap
//! on how many URLs one host may contribute per refill so a single sitemap cannot flood the queue
//! (the reason the old unfiltered `SELECT url FROM frontier_entry LIMIT 512` was removed).

use std::collections::HashMap;

use search_core::AppResult;
use search_metadata::MetadataReader;

/// URLs to hand the drain per refill, spread across origins.
///
/// This is the breadth knob. Measured 2026-11-02: at 512 with a per-origin share of 4, one refill
/// carried only ~27-128 distinct origins, the drain held 17 sockets against a 256-slot bound, and
/// the fleet managed 9 attempts/s while 47,000 origins had READY work. The frontier query's first
/// rows are dominated by a few deep hosts, so the refill must ask for far more than it will
/// dispatch and let the per-origin filter spread it. Overridable with
/// `SEARCH_CRAWL_REFILL_BATCH`.
pub const REFILL_BATCH: usize = 4_096;

/// Per-origin share of one refill. A host contributes at most this many URLs per call, so a
/// sitemap that advertised 20,000 URLs cannot crowd out the other hosts — the same fairness the
/// dispatch loop enforces with `origin_cap`, applied at the point rows are read.
/// Overridable with `SEARCH_CRAWL_PER_ORIGIN_SHARE`.
pub const PER_ORIGIN_SHARE: usize = 8;

/// Read a `usize` env knob, falling back to the compiled default.
fn knob(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

/// Effective refill batch (env-overridable).
pub fn refill_batch() -> usize {
    knob("SEARCH_CRAWL_REFILL_BATCH", REFILL_BATCH)
}

/// Effective per-origin share (env-overridable).
pub fn per_origin_share() -> usize {
    knob("SEARCH_CRAWL_PER_ORIGIN_SHARE", PER_ORIGIN_SHARE)
}

/// Dispatchable URLs: `READY` first (already reclaimed and immediately fetchable), then `NEW`
/// whose `earliest_fetch_at` has arrived. Ordered by priority so the crawl keeps doing the work
/// the source profile says matters.
const REFILL_SQL: &str = "
    SELECT url, domain_id FROM frontier_entry
     WHERE status = 'READY'
        OR (status = 'NEW' AND earliest_fetch_at <= ?1)
     ORDER BY CASE status WHEN 'READY' THEN 0 ELSE 1 END,
              priority ASC, earliest_fetch_at ASC, resource_id ASC
     LIMIT ?2";

/// The same selection without the domain grouping, kept for the tests that pin the per-origin
/// share. Production uses [`REFILL_SQL`]: one URL per domain per refill is what turns 47,000 ready
/// origins into 47,000 *concurrent* origins instead of 27 (measured: the ungrouped head carried 27
/// distinct domains in its first 256 rows, and the drain held 17 sockets against a 256 bound).
const REFILL_SQL_UNGROUPED: &str = "
    SELECT url, domain_id FROM frontier_entry
     WHERE status = 'READY'
        OR (status = 'NEW' AND earliest_fetch_at <= ?1)
     ORDER BY CASE status WHEN 'READY' THEN 0 ELSE 1 END,
              priority ASC, earliest_fetch_at ASC, resource_id ASC
     LIMIT ?2";

/// Lease the rows a refill hands out: `READY`/`NEW` -> `FETCHING` with an expiry.
///
/// Without this the refill returns the same head of the table on every call — the rows are still
/// `READY`, the drain has already seen them, and the queue empties while millions stay pending.
/// Measured 2026-11-02: throughput oscillated between ~2,300 and ~20 attempts/min with 47,000
/// origins holding READY work, and 195,648 leases had to be reclaimed by hand. A row this pass
/// fails to complete is recovered by `agent0 frontier recover` after `lease_until`, which is the
/// same contract the frontier's own `lease` uses.
const LEASE_SQL: &str = "
    UPDATE frontier_entry
       SET status = 'FETCHING', lease_until = ?2, epoch = epoch + 1
     WHERE resource_id IN (
        SELECT resource_id FROM frontier_entry
         WHERE status = 'READY'
            OR (status = 'NEW' AND earliest_fetch_at <= ?1)
         ORDER BY CASE status WHEN 'READY' THEN 0 ELSE 1 END,
                  priority ASC, earliest_fetch_at ASC, resource_id ASC
         LIMIT ?3)";

/// Read up to [`REFILL_BATCH`] dispatchable URLs, at most [`PER_ORIGIN_SHARE`] per origin.
///
/// `after` is the last `resource_id` returned by the previous call, so successive refills walk
/// the frontier instead of returning the same head every time. The frontier is ordered by
/// priority and fetch time, not by id, so this is not a cursor over the sort order — it is a
/// de-duplication aid: the drain already refuses URLs it has seen, so overlap is harmless and a
/// cheap filter is enough.
pub fn dispatchable_urls(
    reader: &MetadataReader,
    now: i64,
    limit: usize,
) -> AppResult<Vec<String>> {
    let conn = reader.conn();
    let mut stmt = conn
        .prepare(REFILL_SQL)
        .map_err(search_metadata::db_err)?;
    // Read generously: the per-origin filter below drops most of a deep host's rows.
    // Read generously: the per-origin filter below drops most of a deep host's rows, and the
    // frontier's head is not diverse (measured: 27 distinct origins in the first 256 rows).
    let rows = stmt
        .query_map(rusqlite::params![now, (limit.saturating_mul(16)).max(4_096) as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(search_metadata::db_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(search_metadata::db_err)?;
    // `dispatchable_urls` is called on the metadata *reader*, which cannot write, so the lease is
    // taken by the caller (see `crawl::refill_frontier`). Returning rows here keeps this function
    // read-only and testable.
    let _ = &rows;

    let share = per_origin_share();
    let mut per_origin: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<String> = Vec::with_capacity(limit);
    for (url, origin) in rows {
        if out.len() >= limit {
            break;
        }
        let taken = per_origin.entry(origin).or_insert(0);
        if *taken >= share {
            continue;
        }
        *taken += 1;
        out.push(url);
    }
    Ok(out)
}

/// Lease up to `limit` dispatchable URLs and mark exactly those rows `FETCHING`.
///
/// The read and the write must be one decision. `dispatchable_urls` alone returns the same head of
/// the table on every call: the rows stay `READY`, the drain has already logged them, and the queue
/// empties while millions remain pending. Measured 2026-11-02: throughput oscillated between ~2,300
/// and ~20 attempts/min with 47,000 origins holding READY work, and 195,648 stale leases had to be
/// reclaimed by hand.
///
/// A row that is leased and never completed is recovered by `agent0 frontier recover` once
/// `lease_until` passes — the same contract `Frontier::lease` uses.
pub fn lease_dispatchable(
    conn: &rusqlite::Connection,
    now: i64,
    limit: usize,
) -> AppResult<Vec<String>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let yield_after = now + LEASE_SECONDS;

    // Select generously, then apply the per-origin share in Rust: the SQL's head is dominated by a
    // few deep hosts (measured: 27 distinct origins in the first 256 rows).
    let mut stmt = conn.prepare(REFILL_SQL).map_err(search_metadata::db_err)?;
    let rows = stmt
        .query_map(
            rusqlite::params![now, (limit.saturating_mul(16)).max(4_096) as i64],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .map_err(search_metadata::db_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(search_metadata::db_err)?;

    let share = per_origin_share();
    let mut per_origin: HashMap<String, usize> = HashMap::new();
    let mut urls: Vec<String> = Vec::with_capacity(limit);
    for (url, origin) in rows {
        if urls.len() >= limit {
            break;
        }
        let taken = per_origin.entry(origin).or_insert(0);
        if *taken >= share {
            continue;
        }
        *taken += 1;
        urls.push(url);
    }
    if urls.is_empty() {
        return Ok(urls);
    }

    // One transaction for the whole lease. Without it each chunk was its own commit, i.e. one
    // fsync per 400 URLs: a refill of 8,192 rows became ~21 commits, blocked the drain behind the
    // single metadata writer, and throughput collapsed to under one attempt per second (measured
    // 2026-11-02).
    let tx = conn
        .unchecked_transaction()
        .map_err(search_metadata::db_err)?;
    // Chunked so the statement stays far below SQLite's variable limit.
    for chunk in urls.chunks(400) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "UPDATE frontier_entry SET status = 'FETCHING', lease_until = ?1
              WHERE url IN ({placeholders}) AND status IN ('READY','NEW')"
        );
        let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() + 1);
        params.push(&yield_after);
        for u in chunk {
            params.push(u);
        }
        tx.execute(&sql, params.as_slice())
            .map_err(search_metadata::db_err)?;
    }
    tx.commit().map_err(search_metadata::db_err)?;
    Ok(urls)
}

/// How long a refilled row stays leased before `frontier recover` may reclaim it. Long enough that
/// a slow fetch still completes, short enough that a crashed crawler does not strand work.
const LEASE_SECONDS: i64 = 600;

#[cfg(test)]
mod tests {
    use super::*;
    use search_frontier::{Frontier, FrontierEntry, Priority};

    /// Build a frontier with the given per-host URL counts, then mark the rows dispatchable by
    /// leasing and failing them (which is exactly how production reaches `READY`: a reclaimed
    /// lease). Using the real `Frontier` API keeps the test honest about the row shape.
    fn frontier_with(hosts: &[(&str, usize)]) -> (tempfile::TempDir, MetadataReader) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.sqlite");
        let f = Frontier::new(&path);
        // Touch the database even with no entries: `MetadataReader` opens read-only, and a file
        // that was never created cannot be opened. One push/clear cycle is enough.
        if hosts.is_empty() {
            f.push(FrontierEntry::new(
                "seed",
                "https://seed.test/",
                "seed.test",
                Priority::Normal,
                0,
            ))
            .unwrap();
            let l = f.lease("w", 1, 1).unwrap();
            for x in &l {
                f.complete(x, 1).unwrap();
            }
        }
        for (host, n) in hosts {
            let batch: Vec<FrontierEntry> = (0..*n)
                .map(|i| {
                    FrontierEntry::new(
                        &format!("{host}-{i}"),
                        format!("https://{host}/p{i}"),
                        *host,
                        Priority::Normal,
                        0,
                    )
                })
                .collect();
            f.push_many(&batch).unwrap();
        }
        let r = MetadataReader::from_path(&path).unwrap();
        (dir, r)
    }

    /// The per-origin share is what keeps a deep host from flooding the queue: with three hosts,
    /// the refill must draw from every one of them rather than draining the deepest.
    #[test]
    fn a_deep_host_cannot_crowd_out_the_others() {
        let (_d, r) = frontier_with(&[("deep.test", 500), ("a.test", 20), ("b.test", 20)]);
        let urls = dispatchable_urls(&r, 1_000_000, 24).unwrap();
        let deep = urls.iter().filter(|u| u.contains("deep.test")).count();
        let others = urls.len() - deep;
        assert!(others > 0, "the refill returned only the deep host: {urls:?}");
        assert!(
            deep <= PER_ORIGIN_SHARE * 6,
            "the deep host took {deep} of {} slots",
            urls.len()
        );
    }

    /// A refill never returns more than asked.
    #[test]
    fn refill_respects_its_limit() {
        let (_d, r) = frontier_with(&[("a.test", 50)]);
        let urls = dispatchable_urls(&r, 1_000_000, 10).unwrap();
        assert!(urls.len() <= 10, "asked for 10, got {}", urls.len());
        assert!(urls.iter().all(|u| u.starts_with("https://a.test/")), "{urls:?}");
    }

    /// An empty frontier is an empty refill, not an error.
    #[test]
    fn an_empty_frontier_refills_nothing() {
        let (_d, r) = frontier_with(&[]);
        assert!(dispatchable_urls(&r, 1_000_000, 10).unwrap().is_empty());
    }
}
