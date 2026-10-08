//! HTTP server: `POST /search`, `GET /health` (spec §7).
//!
//! The router is built around a [`Searcher`] trait object held in an `Arc`, so
//! the whole thing is cheap to clone and trivially stubbed in tests. A tower
//! concurrency + load-shed layer bounds in-flight requests so the process can't
//! be flooded; saturated capacity sheds with `429`.

use crate::deep::{DeepSearchRequest, DeepSearchResponse, DeepSearcher, DocResponse};
use crate::query::{self, Searcher, DEFAULT_LIMIT};
use axum::{
    extract::{rejection::JsonRejection, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    BoxError, Json, Router,
};
use search_core::config::Config;
use search_core::{AppError, AppResult, SearchRequest, SearchResponse};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Instant;

/// Max in-flight requests before the load-shed layer starts returning `429`.
pub const DEFAULT_MAX_CONCURRENCY: usize = 64;

/// Cloneable, cheap (single `Arc`) application state.
#[derive(Clone)]
pub struct AppState {
    pub inner: Arc<Inner>,
}

pub struct Inner {
    pub searcher: Arc<dyn Searcher + Send + Sync>,
    /// Optional deep-search / document-read capability (`/search_deep`, `/read`).
    deep: Option<Arc<dyn DeepSearcher + Send + Sync>>,
    started: Instant,
}

impl AppState {
    pub fn new(searcher: Arc<dyn Searcher + Send + Sync>) -> Self {
        Self {
            inner: Arc::new(Inner {
                searcher,
                deep: None,
                started: Instant::now(),
            }),
        }
    }

    /// Build state that also serves `/search_deep` and `/read`.
    pub fn with_deep(
        searcher: Arc<dyn Searcher + Send + Sync>,
        deep: Arc<dyn DeepSearcher + Send + Sync>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                searcher,
                deep: Some(deep),
                started: Instant::now(),
            }),
        }
    }

    pub fn searcher(&self) -> &(dyn Searcher + Send + Sync) {
        self.inner.searcher.as_ref()
    }

    /// The deep-search backend, when one was configured.
    pub fn deep(&self) -> Option<&(dyn DeepSearcher + Send + Sync)> {
        self.inner.deep.as_deref()
    }

    pub fn uptime_secs(&self) -> u64 {
        self.inner.started.elapsed().as_secs()
    }
}

/// Error type returned by handlers; maps every failure onto an HTTP status.
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn from_app(e: AppError) -> Self {
        let status = match &e {
            AppError::Invalid(_) => StatusCode::BAD_REQUEST,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::Deadline => StatusCode::GATEWAY_TIMEOUT,
            // "busy" — a resource bound means try again later.
            AppError::LimitExceeded(_) => StatusCode::TOO_MANY_REQUESTS,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            message: e.to_string(),
        }
    }

    fn from_rejection(r: JsonRejection) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: format!("invalid request body: {r}"),
        }
    }

    fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: msg.into(),
        }
    }

    /// The deep-search backend was not configured for this server.
    fn unavailable() -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "deep search is not configured on this server".to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(serde_json::json!({
            "error": self.message,
            "status": self.status.as_u16(),
        }));
        (self.status, body).into_response()
    }
}

/// Build the application router.
pub fn router(state: AppState) -> Router {
    // Outer->inner: map shed errors to 429, shed when full, then bound the
    // number of concurrently-running handlers.
    let bounded = tower::ServiceBuilder::new()
        .layer(axum::error_handling::HandleErrorLayer::new(|_: BoxError| async {
            StatusCode::TOO_MANY_REQUESTS
        }))
        .layer(tower::load_shed::LoadShedLayer::new())
        .layer(tower::limit::ConcurrencyLimitLayer::new(
            DEFAULT_MAX_CONCURRENCY,
        ));

    Router::new()
        .route("/health", get(health))
        .route("/search", post(search))
        .route("/search_deep", post(search_deep))
        .route("/read", get(read_doc))
        .layer(bounded)
        .with_state(state)
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let (generation, manifest_id) = match state.searcher().manifest() {
        Ok(m) => (m.generation, m.manifest_id),
        Err(_) => (0, "unavailable".to_string()),
    };
    Json(serde_json::json!({
        "status": "ok",
        "generation": generation,
        "index_manifest_id": manifest_id,
        "uptime_seconds": state.uptime_secs(),
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn search(
    State(state): State<AppState>,
    req: Result<Json<SearchRequest>, JsonRejection>,
) -> Result<Json<SearchResponse>, ApiError> {
    let Json(req) = req.map_err(ApiError::from_rejection)?;

    // Search is CPU/IO bound; keep it off the async runtime's worker threads.
    let searcher = state.inner.searcher.clone();
    let query_len = req.query.len();
    let limit = req.limit;
    let resp = tokio::task::spawn_blocking(move || query::execute(searcher.as_ref(), &req))
        .await
        .map_err(|e| ApiError::internal(format!("search task panicked: {e}")))?
        .map_err(ApiError::from_app)?;

    tracing::debug!(
        query_len,
        limit,
        hits = resp.results.len(),
        "POST /search served"
    );
    Ok(Json(resp))
}

/// `POST /search_deep` — navigation-driven search returning hits + trace.
async fn search_deep(
    State(state): State<AppState>,
    req: Result<Json<DeepSearchRequest>, JsonRejection>,
) -> Result<Json<DeepSearchResponse>, ApiError> {
    let Json(req) = req.map_err(ApiError::from_rejection)?;

    let query = req.query.trim().to_string();
    if query.is_empty() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "query must not be empty".to_string(),
        });
    }
    let limit = query::normalize_limit(req.limit.unwrap_or(DEFAULT_LIMIT))
        .map_err(ApiError::from_app)?;

    let deep = state
        .inner
        .deep
        .clone()
        .ok_or_else(ApiError::unavailable)?;
    // Navigation runs synchronous index searches: keep it off the async workers.
    let resp = tokio::task::spawn_blocking(move || deep.deep_search(&query, limit))
        .await
        .map_err(|e| ApiError::internal(format!("deep search task panicked: {e}")))?
        .map_err(ApiError::from_app)?;

    Ok(Json(resp))
}

/// Query parameters for `GET /read`.
#[derive(Debug, Deserialize)]
struct ReadParams {
    doc_id: Option<String>,
}

/// `GET /read?doc_id=` — the normalized text of a previously-seen document.
async fn read_doc(
    State(state): State<AppState>,
    Query(params): Query<ReadParams>,
) -> Result<Json<DocResponse>, ApiError> {
    let doc_id = params
        .doc_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "missing required query parameter 'doc_id'".to_string(),
        })?;

    let deep = state
        .inner
        .deep
        .clone()
        .ok_or_else(ApiError::unavailable)?;
    let resp = tokio::task::spawn_blocking(move || deep.read(&doc_id))
        .await
        .map_err(|e| ApiError::internal(format!("read task panicked: {e}")))?
        .map_err(ApiError::from_app)?;

    Ok(Json(resp))
}

/// Bind and serve until the process is stopped.
///
/// `deep` optionally enables `/search_deep` and `/read`; when `None` those
/// routes answer `503`.
pub async fn serve(
    cfg: &Config,
    searcher: Arc<dyn Searcher + Send + Sync>,
    deep: Option<Arc<dyn DeepSearcher + Send + Sync>>,
) -> AppResult<()> {
    // Ensure a subscriber exists even when the caller (e.g. an embedding binary)
    // forgot to initialise one. Idempotent.
    crate::init_tracing();
    let state = match deep {
        Some(d) => AppState::with_deep(searcher, d),
        None => AppState::new(searcher),
    };
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(&cfg.bind)
        .await
        .map_err(|e| AppError::Config(format!("failed to bind {}: {e}", cfg.bind)))?;
    tracing::info!(addr = %cfg.bind, "search-api listening");
    axum::serve(listener, app).await?;
    Ok(())
}
