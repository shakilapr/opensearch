//! Ingest coordinator (spec §3). OWNER: ingest agent.
//!
//! Applies stages idempotently, writing each transition to the append-only
//! journal and advancing the `ingest_op` projection. Re-applying a stage that
//! is already recorded is a no-op.

use crate::journal::{IngestStage, IngestState};
use search_core::{AppError, AppResult, OpKey};

/// Coordinates projections for one ingest operation.
pub struct IngestCoordinator;

impl IngestCoordinator {
    /// Record a stage transition. MUST be idempotent: applying the same
    /// `(op_key, stage)` twice must not advance the state twice.
    pub fn record(
        _writer: &mut search_metadata::MetadataWriter,
        _op_key: &OpKey,
        _resource_id: &str,
        _stage: IngestStage,
    ) -> AppResult<()> {
        Err(AppError::Internal("ingest::record not yet implemented".into()))
    }

    pub fn state(
        _reader: &search_metadata::MetadataReader,
        _op_key: &OpKey,
    ) -> AppResult<Option<IngestState>> {
        Err(AppError::Internal("ingest::state not yet implemented".into()))
    }
}
