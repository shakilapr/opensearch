//! Priority dispatch queue (spec §5). OWNER: frontier agent.
//!
//! [`Frontier`] is a thin, thread-safe façade over the `frontier_entry` and
//! `origin_state` tables in the metadata SQLite database. It opens its **own**
//! connection for every operation and wraps each dispatch/reclaim in an
//! `IMMEDIATE` transaction, so two workers can never lease the same row: SQLite
//! serialises the write transactions and the `epoch` guard fences stale writers.
//!
//! Dispatch is priority-first, then earliest-deadline, but is always gated by
//! per-origin concurrency so a single host is never hammered.

use std::collections::HashMap;
use std::path::PathBuf;

use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use search_core::{AppError, AppResult};
use serde::{Deserialize, Serialize};

use crate::lease::Lease;

/// Scheduler priority tiers. Lower value == dispatched first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Priority {
    QueryCritical = 0,
    Hot = 1,
    Normal = 2,
    Discovery = 3,
    Backfill = 4,
}

impl Priority {
    pub fn as_i64(self) -> i64 {
        self as i64
    }
}

impl From<i64> for Priority {
    fn from(v: i64) -> Self {
        match v {
            0 => Priority::QueryCritical,
            1 => Priority::Hot,
            2 => Priority::Normal,
            3 => Priority::Discovery,
            _ => Priority::Backfill,
        }
    }
}

/// A row to enqueue into the frontier. `push` upserts on `resource_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontierEntry {
    pub resource_id: String,
    pub url: String,
    pub domain_id: String,
    pub priority: Priority,
    /// Unix seconds before which this row must not be dispatched (polite delay).
    pub earliest_fetch_at: i64,
    pub expected_value: Option<f64>,
    pub discovered_from: Option<String>,
}

impl FrontierEntry {
    /// Convenience constructor for the common case: dispatch now, no valuation.
    pub fn new(
        resource_id: impl Into<String>,
        url: impl Into<String>,
        domain_id: impl Into<String>,
        priority: Priority,
        earliest_fetch_at: i64,
    ) -> Self {
        Self {
            resource_id: resource_id.into(),
            url: url.into(),
            domain_id: domain_id.into(),
            priority,
            earliest_fetch_at,
            expected_value: None,
            discovered_from: None,
        }
    }
}

/// Tunables for leasing and backoff.
#[derive(Debug, Clone, Copy)]
pub struct FrontierConfig {
    /// How long a granted lease stays valid before it is reclaimable (seconds).
    pub lease_ttl: i64,
    /// Backoff base: delay before retry #1 is `base * 2^attempt` (seconds).
    pub backoff_base: i64,
    /// Hard ceiling on retry backoff (seconds).
    pub backoff_cap: i64,
    /// Default per-origin concurrency when `origin_state` has no row.
    pub default_concurrency: i64,
    /// Ceiling on the adaptive politeness delay (seconds).
    pub max_adaptive_delay: i64,
}

impl Default for FrontierConfig {
    fn default() -> Self {
        Self {
            lease_ttl: 300,
            backoff_base: 5,
            backoff_cap: 3600,
            default_concurrency: 2,
            max_adaptive_delay: 3600,
        }
    }
}

/// The frontier. Dispatch goes through per-origin gates, never FIFO.
pub struct Frontier {
    db_path: PathBuf,
    cfg: FrontierConfig,
}

impl Frontier {
    /// Open the frontier over the metadata database at `path`. The database is
    /// created/migrated on first use by the metadata crate's `open`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: path.into(),
            cfg: FrontierConfig::default(),
        }
    }

    pub fn with_config(path: impl Into<PathBuf>, cfg: FrontierConfig) -> Self {
        Self {
            db_path: path.into(),
            cfg,
        }
    }

    pub fn config(&self) -> FrontierConfig {
        self.cfg
    }

    /// Open a fresh read-write connection. `rusqlite::Connection` is not `Sync`,
    /// so every operation opens its own; a shared `&Frontier` is `Send + Sync`.
    fn conn(&self) -> AppResult<rusqlite::Connection> {
        search_metadata::writer::open(&self.db_path)
    }

    // ------------------------------------------------------------------ push

    /// Insert or upsert a frontier row. Existing scheduling state
    /// (`status`, `attempt`, `epoch`) is preserved so a re-crawl does not
    /// discard an in-flight lease.
    pub fn push(&self, entry: FrontierEntry) -> AppResult<()> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO frontier_entry
                 (resource_id, url, domain_id, priority, earliest_fetch_at,
                  retry_count, status, lease_until, attempt, epoch,
                  expected_value, discovered_from)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, 'NEW', NULL, 0, 0, ?6, ?7)
             ON CONFLICT(resource_id) DO UPDATE SET
                 url = excluded.url,
                 domain_id = excluded.domain_id,
                 priority = excluded.priority,
                 earliest_fetch_at = excluded.earliest_fetch_at,
                 expected_value = COALESCE(excluded.expected_value, frontier_entry.expected_value),
                 discovered_from = COALESCE(excluded.discovered_from, frontier_entry.discovered_from)",
            params![
                entry.resource_id,
                entry.url,
                entry.domain_id,
                entry.priority.as_i64(),
                entry.earliest_fetch_at,
                entry.expected_value,
                entry.discovered_from,
            ],
        )
        .map_err(search_metadata::db_err)?;
        Ok(())
    }

    // ----------------------------------------------------------------- lease

    /// Atomically grant up to `n` leases to `owner`, honouring priority,
    /// politeness delay and per-origin concurrency. The whole selection +
    /// reservation runs inside one `IMMEDIATE` transaction, so concurrent
    /// callers can never receive the same row.
    pub fn lease(&self, owner: &str, now: i64, n: usize) -> AppResult<Vec<Lease>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        let mut conn = self.conn()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(search_metadata::db_err)?;

        // Scan a superset of candidates: some will be rejected by per-origin
        // concurrency, so we look past the first `n` rows.
        let cand_limit = (n as i64).saturating_mul(20).max(256);

        let candidates: Vec<(String, String, i64, i64)> = {
            let mut stmt = tx
                .prepare(
                    "SELECT resource_id, domain_id, attempt, epoch
                       FROM frontier_entry
                      WHERE status IN ('NEW', 'READY')
                        AND earliest_fetch_at <= ?1
                        AND (lease_until IS NULL OR lease_until < ?1)
                      ORDER BY priority ASC, earliest_fetch_at ASC, resource_id ASC
                      LIMIT ?2",
                )
                .map_err(search_metadata::db_err)?;
            let rows = stmt
                .query_map(params![now, cand_limit], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })
                .map_err(search_metadata::db_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(search_metadata::db_err)?;
            rows
        };

        let mut per_domain_used: HashMap<String, i64> = HashMap::new();
        let mut domain_gate: HashMap<String, (i64, i64)> = HashMap::new();
        let mut out: Vec<Lease> = Vec::new();

        for (resource_id, domain_id, attempt, epoch) in candidates {
            if out.len() >= n {
                break;
            }

            // Resolve the origin gate once per domain.
            if !domain_gate.contains_key(&domain_id) {
                let gate = tx
                    .query_row(
                        "SELECT concurrency, next_allowed_at FROM origin_state WHERE domain_id = ?1",
                        params![domain_id],
                        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
                    )
                    .optional()
                    .map_err(search_metadata::db_err)?
                    .unwrap_or((self.cfg.default_concurrency, 0));
                domain_gate.insert(domain_id.clone(), gate);
            }
            let (concurrency, next_allowed_at) = domain_gate[&domain_id];
            if next_allowed_at > now {
                continue;
            }

            // Count live leases for this domain, lazily, once.
            let used = *per_domain_used.entry(domain_id.clone()).or_insert_with(|| {
                tx.query_row(
                    "SELECT COUNT(*) FROM frontier_entry
                      WHERE domain_id = ?1 AND status = 'FETCHING' AND lease_until >= ?2",
                    params![domain_id, now],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0)
            });
            if used >= concurrency {
                continue;
            }

            let new_epoch = epoch + 1;
            let lease_until = now.saturating_add(self.cfg.lease_ttl);
            let changed = tx
                .execute(
                    "UPDATE frontier_entry
                        SET status = 'FETCHING', lease_until = ?2, attempt = attempt + 1,
                            owner = ?3, epoch = ?4
                      WHERE resource_id = ?1 AND epoch = ?5",
                    params![resource_id, lease_until, owner, new_epoch, epoch],
                )
                .map_err(search_metadata::db_err)?;
            if changed != 1 {
                // Lost a race (should not happen under IMMEDIATE); skip.
                continue;
            }

            bump_active(&tx, &domain_id, 1)?;
            *per_domain_used.entry(domain_id.clone()).or_insert(0) += 1;
            out.push(Lease {
                resource_id,
                owner: owner.to_string(),
                epoch: new_epoch,
                lease_until,
                attempt: attempt + 1,
            });
        }

        tx.commit().map_err(search_metadata::db_err)?;
        Ok(out)
    }

    // ---------------------------------------------------------------- complete

    /// Mark a leased resource as successfully fetched. Rejects stale leases
    /// (fencing) with [`AppError::LeaseFenced`].
    pub fn complete(&self, lease: &Lease, now: i64) -> AppResult<()> {
        let mut conn = self.conn()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(search_metadata::db_err)?;

        let (epoch, owner, domain_id, status) = self.load_row(&tx, &lease.resource_id)?;
        check_fence(epoch, owner.as_deref(), &status, lease)?;

        tx.execute(
            "UPDATE frontier_entry
                SET status = 'DONE', lease_until = NULL, owner = NULL
              WHERE resource_id = ?1",
            params![lease.resource_id],
        )
        .map_err(search_metadata::db_err)?;
        let _ = now;
        bump_active(&tx, &domain_id, -1)?;
        tx.commit().map_err(search_metadata::db_err)?;
        Ok(())
    }

    // -------------------------------------------------------------------- fail

    /// Record a failed fetch. With `retryable`, the row returns to `READY` with
    /// exponential backoff; otherwise it becomes terminal (`FAILED`). Rejects
    /// stale leases with [`AppError::LeaseFenced`].
    pub fn fail(&self, lease: &Lease, now: i64, retryable: bool) -> AppResult<()> {
        let mut conn = self.conn()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(search_metadata::db_err)?;

        let (epoch, owner, domain_id, status) = self.load_row(&tx, &lease.resource_id)?;
        check_fence(epoch, owner.as_deref(), &status, lease)?;

        if retryable {
            let delay = self.backoff_secs(lease.attempt);
            let next_at = now.saturating_add(delay);
            tx.execute(
                "UPDATE frontier_entry
                    SET status = 'READY', lease_until = NULL, owner = NULL,
                        retry_count = retry_count + 1, earliest_fetch_at = ?2
                  WHERE resource_id = ?1",
                params![lease.resource_id, next_at],
            )
            .map_err(search_metadata::db_err)?;
        } else {
            tx.execute(
                "UPDATE frontier_entry
                    SET status = 'FAILED', lease_until = NULL, owner = NULL
                  WHERE resource_id = ?1",
                params![lease.resource_id],
            )
            .map_err(search_metadata::db_err)?;
        }
        bump_active(&tx, &domain_id, -1)?;
        tx.commit().map_err(search_metadata::db_err)?;
        Ok(())
    }

    /// Exponential backoff for a retry, `base * 2^attempt`, capped at
    /// [`FrontierConfig::backoff_cap`]. Deterministic (no jitter) so callers can
    /// add their own jitter and so the schedule is reproducible.
    pub fn backoff_secs(&self, attempt: i64) -> i64 {
        let exp = attempt.clamp(0, 30) as u32;
        let factor = 1i64.checked_shl(exp).unwrap_or(i64::MAX);
        let raw = self.cfg.backoff_base.saturating_mul(factor);
        raw.min(self.cfg.backoff_cap).max(0)
    }

    // --------------------------------------------------------- crash recovery

    /// Reclaim zombie leases (`status = FETCHING` and `lease_until < now`) back
    /// to `READY` for crash recovery. Stale epochs remain stale, so a revived
    /// zombie worker's writes are still fenced. Returns rows reclaimed.
    pub fn release_expired(&self, now: i64) -> AppResult<usize> {
        let mut conn = self.conn()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(search_metadata::db_err)?;

        let reclaimed = tx
            .execute(
                "UPDATE frontier_entry
                    SET status = 'READY', lease_until = NULL, owner = NULL
                  WHERE status = 'FETCHING' AND lease_until IS NOT NULL AND lease_until < ?1",
                params![now],
            )
            .map_err(search_metadata::db_err)?;

        tx.execute(
            "UPDATE origin_state
                SET active_leases = (
                    SELECT COUNT(*) FROM frontier_entry f
                     WHERE f.domain_id = origin_state.domain_id
                       AND f.status = 'FETCHING' AND f.lease_until >= ?1)",
            params![now],
        )
        .map_err(search_metadata::db_err)?;

        tx.commit().map_err(search_metadata::db_err)?;
        Ok(reclaimed)
    }

    // ---------------------------------------------------- per-origin politeness

    /// Record a fetch outcome for a domain and adapt politeness. `error_rate`
    /// is an EWMA; `adaptive_delay` doubles on error and halves on success. On
    /// error the origin is also paused via `next_allowed_at` so the very next
    /// `lease` skips it.
    pub fn observe_error(&self, domain_id: &str, is_error: bool) -> AppResult<()> {
        let conn = self.conn()?;
        let current = conn
            .query_row(
                "SELECT error_rate, adaptive_delay FROM origin_state WHERE domain_id = ?1",
                params![domain_id],
                |r| Ok((r.get::<_, f64>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(search_metadata::db_err)?
            .unwrap_or((0.0, 0));

        let sample = if is_error { 1.0 } else { 0.0 };
        let error_rate = 0.9 * current.0 + 0.1 * sample;
        let adaptive_delay = if is_error {
            (current.1.saturating_mul(2).saturating_add(1)).min(self.cfg.max_adaptive_delay)
        } else {
            current.1 / 2
        };
        let next_allowed_at = if is_error {
            search_core::time::now_unix_s().saturating_add(adaptive_delay)
        } else {
            0
        };

        conn.execute(
            "INSERT INTO origin_state
                 (domain_id, concurrency, next_allowed_at, error_rate, adaptive_delay, active_leases)
             VALUES (?1, ?2, ?3, ?4, ?5, 0)
             ON CONFLICT(domain_id) DO UPDATE SET
                 error_rate = ?4,
                 adaptive_delay = ?5,
                 next_allowed_at = MAX(origin_state.next_allowed_at, ?3)",
            params![
                domain_id,
                self.cfg.default_concurrency,
                next_allowed_at,
                error_rate,
                adaptive_delay,
            ],
        )
        .map_err(search_metadata::db_err)?;
        Ok(())
    }

    /// Backpressure signal: should the dispatcher hold off on this origin?
    /// True when the error rate is high, an adaptive delay is active, or the
    /// origin is paused until a future time.
    pub fn should_throttle(&self, domain_id: &str) -> bool {
        self.try_should_throttle(domain_id).unwrap_or_else(|e| {
            tracing::warn!(domain = domain_id, error = %e, "should_throttle query failed; not throttling");
            false
        })
    }

    fn try_should_throttle(&self, domain_id: &str) -> AppResult<bool> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT error_rate, adaptive_delay, next_allowed_at
                   FROM origin_state WHERE domain_id = ?1",
                params![domain_id],
                |r| Ok((r.get::<_, f64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)),
            )
            .optional()
            .map_err(search_metadata::db_err)?;
        let (error_rate, adaptive_delay, next_allowed_at) = match row {
            Some(v) => v,
            None => return Ok(false),
        };
        let now = search_core::time::now_unix_s();
        Ok(error_rate >= 0.5 || adaptive_delay > 0 || next_allowed_at > now)
    }

    /// Set the per-origin concurrency cap (creates the `origin_state` row).
    pub fn set_origin_concurrency(&self, domain_id: &str, concurrency: i64) -> AppResult<()> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO origin_state
                 (domain_id, concurrency, next_allowed_at, error_rate, adaptive_delay, active_leases)
             VALUES (?1, ?2, 0, 0.0, 0, 0)
             ON CONFLICT(domain_id) DO UPDATE SET concurrency = ?2",
            params![domain_id, concurrency],
        )
        .map_err(search_metadata::db_err)?;
        Ok(())
    }

    // ------------------------------------------------------------------ internals

    fn load_row(
        &self,
        tx: &Transaction<'_>,
        resource_id: &str,
    ) -> AppResult<(i64, Option<String>, String, String)> {
        tx.query_row(
            "SELECT epoch, owner, domain_id, status FROM frontier_entry WHERE resource_id = ?1",
            params![resource_id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(search_metadata::db_err)?
        .ok_or_else(|| AppError::NotFound(resource_id.to_string()))
    }
}

impl Default for Frontier {
    fn default() -> Self {
        Self::new(":memory:")
    }
}

/// Reject a stale/zombie lease. An expired-then-reclaimed row has had its
/// `owner` cleared and (on re-dispatch) its `epoch` bumped, so both checks fire.
fn check_fence(
    stored_epoch: i64,
    stored_owner: Option<&str>,
    status: &str,
    lease: &Lease,
) -> AppResult<()> {
    if stored_owner != Some(lease.owner.as_str()) {
        return Err(AppError::LeaseFenced(format!(
            "owner mismatch for {}: stored {:?}, presented {:?}",
            lease.resource_id, stored_owner, lease.owner
        )));
    }
    if stored_epoch != lease.epoch {
        return Err(AppError::LeaseFenced(format!(
            "epoch mismatch for {}: stored {}, presented {}",
            lease.resource_id, stored_epoch, lease.epoch
        )));
    }
    if status != "FETCHING" {
        return Err(AppError::LeaseFenced(format!(
            "resource {} not in FETCHING state (status = {})",
            lease.resource_id, status
        )));
    }
    Ok(())
}

fn bump_active(tx: &Transaction<'_>, domain_id: &str, delta: i64) -> AppResult<()> {
    tx.execute(
        "INSERT INTO origin_state
             (domain_id, concurrency, next_allowed_at, error_rate, adaptive_delay, active_leases)
         VALUES (?1, 2, 0, 0.0, 0, MAX(0, ?2))
         ON CONFLICT(domain_id) DO UPDATE SET active_leases = MAX(0, active_leases + ?2)",
        params![domain_id, delta],
    )
    .map_err(search_metadata::db_err)?;
    Ok(())
}
