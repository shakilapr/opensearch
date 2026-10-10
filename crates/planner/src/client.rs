//! The planner client: a [`search_engine::QueryPlanner`] over the cached, single-flight
//! [`search_organize::LlmClient`].
//!
//! `QueryPlanner::plan` is synchronous (the engine calls it on the search hot path), so the
//! client owns a small dedicated Tokio runtime and drives the async client to completion. When
//! `plan` is invoked from inside another Tokio runtime it is moved to a fresh OS thread first,
//! because nesting `block_on` on a runtime thread panics.
//!
//! Failures never propagate to the search: a transport error, a timeout, or an unparseable
//! response all become an empty plan plus a note, so retrieval proceeds exactly as it would with
//! no planner at all.

use std::future::Future;
use std::sync::Arc;

use search_engine::analyze::QueryAnalysis;
use search_engine::{PlannedQueries, QueryPlanner};
use search_organize::{CacheStats, LlmClient, OrganizeError};

use crate::config::PlannerConfig;
use crate::prompt::{build_prompt, parse_plan};

/// A failure to construct the planner (bad config, or no runtime could be built).
#[derive(Debug, thiserror::Error)]
pub enum PlannerError {
    /// The underlying LLM client could not be built.
    #[error(transparent)]
    Organize(#[from] OrganizeError),
    /// The dedicated runtime could not be built.
    #[error("failed to build the planner runtime: {0}")]
    Runtime(String),
}

struct Inner {
    client: Arc<LlmClient>,
    rt: tokio::runtime::Runtime,
    max_expansions: usize,
}

/// An LLM-backed query planner implementing [`search_engine::QueryPlanner`].
///
/// Cheap to clone (`Arc` inside). Holds the cached client, the dedicated runtime, and the
/// expansion cap.
#[derive(Clone)]
pub struct LlmQueryPlanner {
    inner: Arc<Inner>,
}

impl LlmQueryPlanner {
    /// Build from resolved configuration, with the real HTTP transport.
    pub fn new(cfg: PlannerConfig) -> Result<Self, PlannerError> {
        let max_expansions = cfg.max_expansions;
        let client = LlmClient::new(cfg.llm, cfg.cache_dir, cfg.concurrency)?;
        Self::with_client(Arc::new(client), max_expansions)
    }

    /// Build around a caller-supplied client (hermetic tests, or a custom backend).
    pub fn with_client(
        client: Arc<LlmClient>,
        max_expansions: usize,
    ) -> Result<Self, PlannerError> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| PlannerError::Runtime(e.to_string()))?;
        Ok(Self {
            inner: Arc::new(Inner { client, rt, max_expansions }),
        })
    }

    /// The configured model id.
    pub fn model(&self) -> &str {
        self.inner.client.model()
    }

    /// Snapshot of cache activity.
    pub fn cache_stats(&self) -> CacheStats {
        self.inner.client.cache_stats()
    }

    /// Drive `fut` to completion from a synchronous context.
    fn block_on<F, T>(&self, fut: F) -> T
    where
        F: Future<Output = T> + Send,
        T: Send,
    {
        if tokio::runtime::Handle::try_current().is_ok() {
            // Already inside a runtime: run on our own runtime from a dedicated thread so we
            // never nest `block_on` on the caller's runtime (which panics).
            std::thread::scope(|s| {
                s.spawn(|| self.inner.rt.block_on(fut))
                    .join()
                    .expect("planner runtime thread panicked")
            })
        } else {
            self.inner.rt.block_on(fut)
        }
    }

    async fn plan_inner(
        &self,
        query: &str,
        analysis: &QueryAnalysis,
    ) -> Result<PlannedQueries, OrganizeError> {
        let prompt = build_prompt(query, analysis);
        let content = self.inner.client.complete(&prompt).await?;
        Ok(match parse_plan(&content, self.inner.max_expansions, query) {
            Ok(planned) => planned,
            Err(e) => PlannedQueries {
                expansions: Vec::new(),
                notes: vec![format!("unparseable model output ({e})")],
            },
        })
    }
}

impl QueryPlanner for LlmQueryPlanner {
    fn plan(&self, query: &str, analysis: &QueryAnalysis) -> PlannedQueries {
        match self.block_on(self.plan_inner(query, analysis)) {
            Ok(planned) => planned,
            // A planner must never fail a search. Report the failure in the trace and return no
            // expansions, which is exactly the no-planner behaviour.
            Err(e) => PlannedQueries {
                expansions: Vec::new(),
                notes: vec![format!("planner unavailable: {e}")],
            },
        }
    }
}
