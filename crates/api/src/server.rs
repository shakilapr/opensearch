//! HTTP server (spec §7). OWNER: api agent.

use axum::{routing::get, routing::post, Json, Router};
use search_core::{SearchRequest, SearchResponse};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub inner: Arc<Inner>,
}

pub struct Inner {
    pub manifest_id: String,
    pub index_manifest_id: String,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/search", post(search))
        .with_state(state)
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn search(
    _state: axum::extract::State<AppState>,
    Json(_req): Json<SearchRequest>,
) -> Json<SearchResponse> {
    unimplemented!("api::server::search not yet implemented")
}
