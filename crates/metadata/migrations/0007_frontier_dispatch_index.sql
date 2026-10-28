-- 0007: a dispatch index the lease query can actually use.
--
-- `Frontier::lease` selects dispatchable rows and orders them:
--
--   SELECT ... FROM frontier_entry
--    WHERE status IN ('NEW','READY') AND earliest_fetch_at <= ?
--    ORDER BY priority, earliest_fetch_at, resource_id LIMIT ?
--
-- `idx_frontier_dispatch(status, priority, earliest_fetch_at)` leads with `status`, and
-- `status IN (...)` is a *set*: SQLite cannot use the index to produce the ORDER BY across two
-- status values, so it builds a temporary B-tree. Measured 2026-10-28 on a 2.3M-row fleet
-- shard: **522 ms per lease call** (the crawl leases in a loop, so this was most of its CPU),
-- versus **0.4 ms** through this partial index, which stores only dispatchable rows already
-- ordered by (priority, earliest_fetch_at, resource_id).
--
-- ANALYZE afterwards is not optional: without statistics the planner kept choosing the old
-- index even with this one present.
CREATE INDEX IF NOT EXISTS idx_frontier_ready_dispatch
  ON frontier_entry(priority, earliest_fetch_at, resource_id)
  WHERE status IN ('NEW', 'READY');

ANALYZE;
