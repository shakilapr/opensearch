-- Migration 0004: storage tier (plan Stage 5, docs/STAGE5_SPEC.md).
--
-- Adds the HOT/WARM/COLD *storage* tier of a resource. This is distinct from the crawler's
-- freshness `SourceClass` (a source cadence) and from any per-request ranking: it records where
-- the resource's bytes physically live. New resources default to 'hot'; the tier move planner
-- (crates/tiers) drives updates through `Mutation::UpdateStorageTier`.
--
-- `ALTER TABLE ... ADD COLUMN` has no `IF NOT EXISTS` form in SQLite, so replay safety comes
-- from the migration runner's ledger: this version is applied at most once.

PRAGMA foreign_keys = ON;

ALTER TABLE resource ADD COLUMN storage_tier TEXT NOT NULL DEFAULT 'hot';

CREATE INDEX IF NOT EXISTS idx_resource_storage_tier ON resource(storage_tier);
