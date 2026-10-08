//! Query analysis + execution (spec §7). OWNER: api agent.

use search_core::{QueryAnalysis, SearchResponse};

/// Run a search against the indexer + metadata, producing the frozen response.
pub fn execute(_req: &search_core::SearchRequest) -> search_core::AppResult<SearchResponse> {
    Err(search_core::AppError::Internal("api::query not yet implemented".into()))
}

/// Heuristic query analysis for Stage 0.
pub fn analyze(query: &str) -> QueryAnalysis {
    QueryAnalysis {
        original: query.to_string(),
        normalized_text: query.split_whitespace().collect::<Vec<_>>().join(" "),
        language: None,
        exact_phrases: Vec::new(),
        entities: Vec::new(),
        entity_hypotheses: Vec::new(),
        concepts: Vec::new(),
        hard_predicates: Vec::new(),
        soft_preferences: Vec::new(),
        time_interval: None,
        preferred_types: Vec::new(),
        intent: search_core::Intent::Factual,
        complexity: 0,
    }
}
