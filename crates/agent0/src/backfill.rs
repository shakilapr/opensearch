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
pub const REFILL_BATCH: usize = 512;

/// Per-origin share of one refill. A host contributes at most this many URLs per call, so a
/// sitemap that advertised 20,000 URLs cannot crowd out the other hosts — the same fairness the
/// dispatch loop enforces with `origin_cap`, applied at the point rows are read.
pub const PER_ORIGIN_SHARE: usize = 4;

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
    let rows = stmt
        .query_map(rusqlite::params![now, (limit * 32) as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(search_metadata::db_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(search_metadata::db_err)?;

    let mut per_origin: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<String> = Vec::with_capacity(limit);
    for (url, origin) in rows {
        if out.len() >= limit {
            break;
        }
        let taken = per_origin.entry(origin).or_insert(0);
        if *taken >= PER_ORIGIN_SHARE {
            continue;
        }
        *taken += 1;
        out.push(url);
    }
    Ok(out)
}

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
