//! Ingest coordinator (spec §3). OWNER: ingest agent.
//!
//! Applies stages idempotently, writing each transition to the append-only
//! journal and advancing the `ingest_op` projection **in the same batch**, so
//! the projection can never diverge from the journal. Re-applying a stage that
//! is already recorded is a no-op.
//!
//! ## Idempotency guarantee (precise)
//!
//! For a fixed `(op_key, stage)` pair:
//!
//! - if the stage is **already present** in `ingest_event` for that `op_key`,
//!   [`IngestCoordinator::record`] returns `Ok(())` and writes **nothing** — no
//!   journal row, no `ingest_op` update. This is the at-least-once delivery
//!   guarantee required by spec §10 fault 8.
//! - if the stage is **strictly lower** than the furthest stage already
//!   recorded and was never recorded itself, the transition is *backwards* and
//!   is rejected with [`AppError::Invalid`] — the journal is never moved
//!   backwards.
//! - otherwise the stage advances the state and appends exactly one journal row
//!   plus one `ingest_op` upsert, atomically in one transaction.
//!
//! Because only the first write of each stage mutates the journal, running the
//! full `FETCHED..=VISIBLE` walk twice produces exactly 7 events and an
//! identical final state.

use crate::journal::{IngestStage, IngestState};
use search_core::{AppError, AppResult, OpKey};
use search_metadata::{db_err, MetadataReader, MetadataWriter, Mutation};
use std::collections::BTreeSet;

/// Coordinates projections for one ingest operation.
pub struct IngestCoordinator;

/// Both views of an op's progress: the fast `ingest_op` projection and the
/// authoritative journal replay. Tests assert they agree after every step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestStateView {
    /// State read from the `ingest_op` projection table (may be `None`).
    pub projected: Option<IngestState>,
    /// State rebuilt by replaying the append-only journal (source of truth).
    pub replayed: Option<IngestState>,
}

impl IngestStateView {
    /// `true` when the projection agrees with the journal, or when both are
    /// absent. Divergence is a bug: the projection is derivable from the
    /// journal.
    pub fn agree(&self) -> bool {
        self.projected == self.replayed
    }
}

impl IngestCoordinator {
    /// Record a stage transition.
    ///
    /// Idempotent per `(op_key, stage)`: applying the same stage twice writes
    /// no journal row and does not advance the projection. Backwards
    /// transitions (an unrecorded stage below the current high-water mark) are
    /// rejected. See the module docs for the exact guarantee.
    ///
    /// The current state is read from the writer's own connection (the writer
    /// is the only writer; there is no separate reader here). The journal is
    /// consulted — not the projection — so a lagging projection cannot fool
    /// the idempotency check.
    pub fn record(
        writer: &mut MetadataWriter,
        op_key: &OpKey,
        resource_id: &str,
        stage: IngestStage,
    ) -> AppResult<()> {
        let conn = writer.conn();
        let (furthest, recorded) = replay_progress(conn, op_key.as_str())?;

        // Already applied -> idempotent no-op, no writes at all.
        if recorded.contains(&stage) {
            tracing::debug!(
                op_key = op_key.as_str(),
                stage = stage.as_str(),
                "ingest stage already recorded; idempotent no-op"
            );
            return Ok(());
        }

        // A stage below the high-water mark that was never recorded is a
        // backwards transition: reject, never move the journal backwards.
        if let Some(cur) = furthest {
            if stage < cur {
                return Err(AppError::Invalid(format!(
                    "backwards ingest transition for op_key {}: cannot record {} below current {}",
                    op_key.as_str(),
                    stage.as_str(),
                    cur.as_str()
                )));
            }
        }

        let now = search_core::time::now_unix_s();
        let blob = [
            Mutation::Journal {
                op_key: op_key.as_str().to_string(),
                resource_id: resource_id.to_string(),
                stage: stage.as_str().to_string(),
                detail: None,
                at: now,
            },
            Mutation::UpsertIngestOp {
                op_key: op_key.as_str().to_string(),
                resource_id: resource_id.to_string(),
                docver_id: None,
                state: stage.as_str().to_string(),
                at: now,
            },
        ];
        // Both writes land in one transaction: the projection can never outrun
        // the journal that is its source of truth.
        writer.apply_batch(&blob)?;
        tracing::debug!(
            op_key = op_key.as_str(),
            stage = stage.as_str(),
            "recorded ingest stage"
        );
        Ok(())
    }

    /// Rebuild state by replaying the journal for `op_key` (spec §3 rule 1:
    /// the journal is the source of truth; `ingest_op` is derivable).
    ///
    /// Returns the *replayed* state. Use [`IngestCoordinator::state_view`] to
    /// inspect the projection and the replay together.
    pub fn state(
        reader: &MetadataReader,
        op_key: &OpKey,
    ) -> AppResult<Option<IngestState>> {
        Ok(Self::state_view(reader, op_key)?.replayed)
    }

    /// Both the projected and replayed states for `op_key`, so callers/tests can
    /// assert they agree.
    pub fn state_view(
        reader: &MetadataReader,
        op_key: &OpKey,
    ) -> AppResult<IngestStateView> {
        let conn = reader.conn();
        let replayed = replay_state(conn, op_key.as_str())?;
        let projected = read_projection(conn, op_key.as_str())?;
        Ok(IngestStateView {
            projected,
            replayed,
        })
    }
}

/// Number of journal events recorded for an `op_key`. Used by tests and by the
/// projection-consistency checks.
pub fn journal_event_count(reader: &MetadataReader, op_key: &OpKey) -> AppResult<u64> {
    let n: i64 = reader
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM ingest_event WHERE op_key = ?1",
            rusqlite::params![op_key.as_str()],
            |r| r.get(0),
        )
        .map_err(db_err)?;
    Ok(n as u64)
}

/// Replay the journal for `op_key`, returning the furthest stage reached and
/// the set of distinct stages recorded (both ordered by `seq`).
fn replay_progress(
    conn: &rusqlite::Connection,
    op_key: &str,
) -> AppResult<(Option<IngestStage>, BTreeSet<IngestStage>)> {
    let mut stmt = conn
        .prepare("SELECT stage FROM ingest_event WHERE op_key = ?1 ORDER BY seq")
        .map_err(db_err)?;
    let rows = stmt
        .query_map(rusqlite::params![op_key], |r| r.get::<_, String>(0))
        .map_err(db_err)?;

    let mut furthest: Option<IngestStage> = None;
    let mut recorded = BTreeSet::new();
    for row in rows {
        let s = row.map_err(db_err)?;
        if let Some(stage) = IngestStage::parse(&s) {
            recorded.insert(stage);
            furthest = Some(match furthest {
                Some(cur) if cur >= stage => cur,
                _ => stage,
            });
        }
    }
    Ok((furthest, recorded))
}

/// The resource id of the latest journal event for `op_key`, if any.
fn replay_resource_id(
    conn: &rusqlite::Connection,
    op_key: &str,
) -> AppResult<Option<String>> {
    conn.query_row(
        "SELECT resource_id FROM ingest_event WHERE op_key = ?1 ORDER BY seq DESC LIMIT 1",
        rusqlite::params![op_key],
        |r| r.get::<_, String>(0),
    )
    .map(Some)
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        other => Err(db_err(other)),
    })
}

/// Fully rebuild [`IngestState`] from the journal.
fn replay_state(
    conn: &rusqlite::Connection,
    op_key: &str,
) -> AppResult<Option<IngestState>> {
    let (furthest, _) = replay_progress(conn, op_key)?;
    match furthest {
        None => Ok(None),
        Some(stage) => {
            let resource_id = replay_resource_id(conn, op_key)?.unwrap_or_default();
            Ok(Some(IngestState {
                op_key: op_key.to_string(),
                resource_id,
                docver_id: None,
                stage,
            }))
        }
    }
}

/// Read the fast `ingest_op` projection row.
fn read_projection(
    conn: &rusqlite::Connection,
    op_key: &str,
) -> AppResult<Option<IngestState>> {
    conn.query_row(
        "SELECT resource_id, docver_id, state FROM ingest_op WHERE op_key = ?1",
        rusqlite::params![op_key],
        |r| {
            let resource_id: String = r.get(0)?;
            let docver_id: Option<String> = r.get(1)?;
            let state: String = r.get(2)?;
            Ok((resource_id, docver_id, state))
        },
    )
    .map(|(resource_id, docver_id, state)| {
        IngestStage::parse(&state).map(|stage| IngestState {
            op_key: op_key.to_string(),
            resource_id,
            docver_id,
            stage,
        })
    })
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        other => Err(db_err(other)),
    })
}

/// Crash-recovery helper (spec §10 fault 1).
///
/// Returns the furthest stage already **committed** to the journal for
/// `op_key`, so a resumed pipeline can continue from the next stage without
/// redoing committed work. Returns [`AppError::NotFound`] when nothing has been
/// recorded yet.
pub fn resume(reader: &MetadataReader, op_key: &OpKey) -> AppResult<IngestStage> {
    let (furthest, _) = replay_progress(reader.conn(), op_key.as_str())?;
    furthest.ok_or_else(|| {
        AppError::NotFound(format!("no committed ingest stages for op_key {}", op_key.as_str()))
    })
}

/// Idempotency test harness: walk `FETCHED..=VISIBLE` for `op_key`, then return
/// the final state rebuilt from the journal.
///
/// Calling this twice must leave exactly the same 7 journal events and the same
/// final state (spec §10 fault 8, "at-least-once replay").
pub fn ingest_once(
    writer: &mut MetadataWriter,
    reader: &MetadataReader,
    op_key: &OpKey,
    resource_id: &str,
) -> AppResult<IngestState> {
    for stage in IngestStage::ALL {
        IngestCoordinator::record(writer, op_key, resource_id, stage)?;
    }
    IngestCoordinator::state(reader, op_key)?
        .ok_or_else(|| AppError::Internal("ingest_once produced no state".into()))
}

// Re-export rusqlite for the helpers above without leaking it from lib.rs.
use rusqlite;
