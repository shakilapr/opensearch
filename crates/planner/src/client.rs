//! The planner client: a [`search_engine::QueryPlanner`] over `search-organize`'s cached,
//! live-verified query planner.
//!
//! `QueryPlanner::plan` is synchronous (the engine calls it on the search hot path), so the client
//! owns a small dedicated Tokio runtime and drives the async `LlmClient::plan_query` to
//! completion. When `plan` is invoked from inside another Tokio runtime it is moved to a fresh OS
//! thread first, because nesting `block_on` on a runtime thread panics.
//!
//! The prompt and the response parser are **not** duplicated here: they live in
//! `search_organize::queryplan`, which is the live-verified implementation (its plan was checked
//! against the real model). This crate owns only two things: the synchronous engine seam, and the
//! mapping from a `QueryPlan` onto the engine's §8 probe list. A second prompt for the same job
//! would be a second thing to keep correct.
//!
//! Failures never propagate to the search: a transport error, a timeout, or an unparseable
//! response all become an empty plan plus a note, so retrieval proceeds exactly as it would with
//! no planner at all.

use std::future::Future;
use std::sync::Arc;

use search_engine::analyze::QueryAnalysis;
use search_engine::{PlannedQueries, QueryPlanner};
use search_organize::{CacheStats, LlmClient, OrganizeError, QueryPlan};

use crate::config::PlannerConfig;

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

/// Map a `search-organize` plan onto the engine's probe list.
///
/// The plan's rewritten query — required terms (`+term`), exclusions (`-term`), and phrases folded
/// in as `"a b"~3` — becomes **one** §8 expansion variant, searched alongside the original under
/// the §10 family cap. A query that the planner left unchanged yields no variant at all, which is
/// the identity case. The plan's canonical entities and exclusions go into the trace notes: they
/// are what makes the decision auditable, and the entities are the input the §9.4 graph walk
/// wants.
pub fn plan_to_probes(
    plan: Option<QueryPlan>,
    original: &str,
    max_expansions: usize,
) -> PlannedQueries {
    let Some(plan) = plan else {
        return PlannedQueries::none();
    };
    let original = original.trim();

    let mut expansions: Vec<String> = Vec::new();
    let rewritten = plan.query.trim();
    // Only a *changed* query is a probe; echoing the original would search it twice.
    if !rewritten.is_empty() && rewritten != original && max_expansions > 0 {
        expansions.push(rewritten.to_string());
    }

    let mut notes: Vec<String> = Vec::new();
    if !plan.entities.is_empty() {
        notes.push(format!("entities: {}", plan.entities.join(", ")));
    }
    if !plan.phrases.is_empty() {
        notes.push(format!("phrases: {}", plan.phrases.join(", ")));
    }
    if !plan.negatives.is_empty() {
        notes.push(format!("negatives: {}", plan.negatives.join(", ")));
    }

    PlannedQueries { expansions, notes }
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
        _analysis: &QueryAnalysis,
    ) -> Result<PlannedQueries, OrganizeError> {
        // The engine has no conversation history yet, so the planner is asked with an empty one.
        let plan = self.inner.client.plan_query(query, &[]).await?;
        Ok(plan_to_probes(plan, query, self.inner.max_expansions))
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
