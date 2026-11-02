//! search-core: the frozen contract types shared by every crate.
//!
//! Contains the five semantic IDs, operational record types, resource/safety
//! limits, error taxonomy, the IndexManifest, and time helpers. Nothing here
//! performs I/O. See `docs/specs/STAGE0_SPEC.md` for the authoritative definition.

pub mod config;
pub mod contract;
pub mod error;
pub mod ids;
pub mod limits;
pub mod llm;
pub mod manifest;
pub mod text;
pub mod time;
pub mod usage;

pub use contract::{
    Intent, QueryAnalysis, SearchFilters, SearchRequest, SearchResponse, SearchResult,
};
pub use error::{AppError, AppResult};
pub use ids::{
    ContentId, DocId, DocVerId, DomainId, ManifestId, OpKey, ResourceId, SourceId, WorkId,
};
pub use limits::Limits;
pub use llm::LlmAttempt;
pub use manifest::{IndexManifest, Provenance};
pub use usage::TokenUsage;
